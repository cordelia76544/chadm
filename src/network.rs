//! Linux bridge + persistent TAP. Lifecycle callers must hold the metadata lock.
//! Never infer ownership from an interface name alone. No DHCP server or IP assignment.
use crate::command;
use crate::config::HostConfig;
use crate::error::{Error, Result};
use crate::instance::{self, InstanceConfig, NetworkConfig};
use crate::instance_config::{bridge_name, validate};
use crate::metadata::{self, Lock};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

const TAP_FLAGS: u32 = 0x0002 | 0x1000 | 0x4000; // TAP, NO_PI, VNET_HDR (CH v52)
const PERSIST: u32 = 0x0800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub index: u32,
    pub mtu: u32,
    pub bridge: bool,
    pub up: bool,
    pub alias: String,
    pub tun_flags: Option<u32>,
    pub master: Option<String>,
}

fn resource(message: impl Into<String>) -> Error {
    Error::Resource(message.into())
}

/// Reads the current network namespace through sysfs; permission/read failures are errors.
fn read_link(root: &Path, name: &str) -> Result<Option<Link>> {
    bridge_name(name)?;
    let dir = root.join(name);
    match fs::metadata(&dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(metadata::io_error("inspect network interface", &dir, error)),
        Ok(_) => (),
    }
    let read = |field: &str| -> Result<String> {
        let path = dir.join(field);
        fs::read_to_string(&path)
            .map(|s| s.trim().to_owned())
            .map_err(|e| metadata::io_error("read network interface", &path, e))
    };
    let integer = |field: &str, hex: bool| -> Result<u32> {
        let text = read(field)?;
        u32::from_str_radix(
            if hex {
                text.strip_prefix("0x").unwrap_or(&text)
            } else {
                &text
            },
            if hex { 16 } else { 10 },
        )
        .map_err(|_| resource(format!("invalid {field} for interface {name:?}")))
    };
    let index = integer("ifindex", false)?;
    if index == 0 {
        return Err(resource("invalid zero interface index"));
    }
    let tun_flags = match fs::metadata(dir.join("tun_flags")) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(metadata::io_error("inspect TAP flags", &dir, e)),
        Ok(_) => Some(integer("tun_flags", true)?),
    };
    let master = match fs::read_link(dir.join("master")) {
        Ok(path) => Some(
            path.file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| resource("invalid interface master"))?
                .to_owned(),
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(metadata::io_error("read interface master", &dir, e)),
    };
    let bridge = match fs::metadata(dir.join("bridge")) {
        Ok(m) => m.is_dir(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(metadata::io_error("inspect bridge", &dir, e)),
    };
    Ok(Some(Link {
        index,
        mtu: integer("mtu", false)?,
        bridge,
        up: integer("flags", true)? & 1 != 0,
        alias: read("ifalias")?,
        tun_flags,
        master,
    }))
}

pub fn bridge_status(name: &str) -> Result<Link> {
    require_bridge(&Linux, name)
}
fn require_bridge(backend: &impl Backend, name: &str) -> Result<Link> {
    bridge_name(name)?;
    let link = backend
        .link(name)?
        .ok_or_else(|| resource(format!("bridge {name:?} does not exist")))?;
    if !link.bridge {
        return Err(resource(format!(
            "interface {name:?} is not a Linux bridge"
        )));
    }
    if !link.up {
        return Err(resource(format!(
            "bridge {name:?} is down; bring it up before preparing TAPs"
        )));
    }
    Ok(link)
}
fn marker(config: &InstanceConfig, index: usize) -> String {
    format!("chadm:{}:{index}", config.id)
}
fn owned(link: &Link, config: &InstanceConfig, index: usize) -> Result<()> {
    if link.alias != marker(config, index)
        || link.tun_flags.is_none_or(|flags| {
            flags & (TAP_FLAGS | PERSIST) != TAP_FLAGS | PERSIST || flags & 1 != 0
        })
    {
        return Err(resource(format!("interface {:?} is not a compatible TAP owned by this chadm instance; refusing modification", config.network[index].tap)));
    }
    Ok(())
}

// Small injectable boundary for testing actual failure sequences without CAP_NET_ADMIN.
trait Backend {
    fn link(&self, name: &str) -> Result<Option<Link>>;
    fn create(&self, name: &str, alias: &str) -> Result<Link>;
    fn set(&self, name: &str, args: &[&str]) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}
struct Linux;
impl Linux {
    fn ip(args: &[&str]) -> Result<()> {
        let ip = command::executable(Path::new("ip"))
            .ok_or_else(|| resource("iproute2 is not installed"))?;
        command::run_utility(
            &ip,
            &args
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>(),
            Duration::from_secs(3),
        )
        .map_err(resource)
    }
}
impl Backend for Linux {
    fn link(&self, name: &str) -> Result<Option<Link>> {
        read_link(Path::new("/sys/class/net"), name)
    }
    fn create(&self, name: &str, alias: &str) -> Result<Link> {
        bridge_name(name)?;
        // These ioctl encodings are Linux asm-generic, as used on CH's x86_64/aarch64 targets.
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        return Err(resource(
            "native TAP creation currently requires Linux x86_64 or aarch64",
        ));
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        {
            let path = Path::new("/dev/net/tun");
            let fd = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(0x20000 | 0x800)
                .open(path)
                .map_err(|e| metadata::io_error("open TUN/TAP device", path, e))?;
            if !fd
                .metadata()
                .map_err(|e| metadata::io_error("inspect TUN/TAP device", path, e))?
                .file_type()
                .is_char_device()
            {
                return Err(resource("/dev/net/tun is not a character device"));
            }
            #[repr(C)]
            struct IfReq {
                name: [u8; 16],
                data: [u64; 3],
            }
            let mut request = IfReq {
                name: [0; 16],
                data: [0; 3],
            };
            request.name[..name.len()].copy_from_slice(name.as_bytes());
            // ifr_flags occupies the first two native-endian bytes of the union.
            let flags = (TAP_FLAGS | 0x8000) as u16; // TUN_EXCL: never attach to an existing TAP.
            let flag_bytes = flags.to_ne_bytes();
            let union = request.data.as_mut_ptr().cast::<u8>();
            // SAFETY: union has 24 initialized bytes; the two flag bytes fit.
            unsafe {
                std::ptr::copy_nonoverlapping(flag_bytes.as_ptr(), union, 2);
            }
            unsafe extern "C" {
                fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
            }
            // SAFETY: initialized Linux ifreq (40 bytes, 8-byte aligned), valid fd.
            if unsafe { ioctl(fd.as_raw_fd(), 0x400454ca, &mut request as *mut IfReq) } < 0 {
                return Err(resource(format!(
                    "cannot create TAP {name:?}: {}",
                    io::Error::last_os_error()
                )));
            }
            // Nonpersistent until the ownership marker has been set: errors/drop/crash
            // before TUNSETPERSIST close this fd and remove only this new TAP.
            self.set(name, &["alias", alias])?;
            let mut link = self
                .link(name)?
                .ok_or_else(|| resource("new TAP disappeared"))?;
            if link.alias != alias || link.tun_flags.is_none_or(|f| f & TAP_FLAGS != TAP_FLAGS) {
                return Err(resource("new TAP identity changed before persistence"));
            }
            // SAFETY: TUNSETPERSIST takes an integer value, not a pointer.
            if unsafe { ioctl(fd.as_raw_fd(), 0x400454cb, 1 as std::ffi::c_ulong) } < 0 {
                return Err(resource(format!(
                    "cannot persist TAP {name:?}: {}",
                    io::Error::last_os_error()
                )));
            }
            link.tun_flags = link.tun_flags.map(|f| f | PERSIST);
            Ok(link)
        }
    }
    fn set(&self, name: &str, args: &[&str]) -> Result<()> {
        let mut argv = vec!["link", "set", "dev", name];
        argv.extend_from_slice(args);
        Self::ip(&argv)
    }
    fn delete(&self, name: &str) -> Result<()> {
        Self::ip(&["tuntap", "del", "dev", name, "mode", "tap"])
    }
}

fn same_owned(
    backend: &impl Backend,
    config: &InstanceConfig,
    index: usize,
    identity: u32,
) -> Result<Link> {
    let link = backend
        .link(&config.network[index].tap)?
        .ok_or_else(|| resource("managed TAP disappeared"))?;
    owned(&link, config, index)?;
    if link.index != identity {
        return Err(resource(
            "TAP interface index changed; refusing modification",
        ));
    }
    Ok(link)
}
fn remove_owned(
    backend: &impl Backend,
    config: &InstanceConfig,
    index: usize,
    identity: u32,
) -> Result<()> {
    same_owned(backend, config, index, identity)?;
    backend.delete(&config.network[index].tap)?;
    if backend.link(&config.network[index].tap)?.is_some() {
        return Err(resource(
            "TAP still exists after cleanup; inspect active TAP users",
        ));
    }
    Ok(())
}

/// Validate resources again immediately before the VMM opens them.
pub(crate) fn verify_prepared(config: &InstanceConfig) -> Result<()> {
    validate(config)?;
    for (index, net) in config.network.iter().enumerate() {
        let bridge = require_bridge(&Linux, &net.bridge)?;
        let link = Linux
            .link(&net.tap)?
            .ok_or_else(|| resource("prepared TAP disappeared"))?;
        owned(&link, config, index)?;
        if !link.up || link.master.as_deref() != Some(&net.bridge) || link.mtu != bridge.mtu {
            return Err(resource("prepared TAP bridge, UP state or MTU changed"));
        }
    }
    Ok(())
}
/// Only used by locked lifecycle helpers; public wrappers below validate metadata/state.
pub(crate) fn prepare_unlocked(config: &InstanceConfig) -> Result<()> {
    prepare_with(&Linux, config)
}
fn prepare_with(backend: &impl Backend, config: &InstanceConfig) -> Result<()> {
    validate(config)?;
    let mut bridges = Vec::new();
    // Preflight every NIC before creating anything. Existing names are never adopted.
    for net in &config.network {
        bridges.push(require_bridge(backend, &net.bridge)?);
        if backend.link(&net.tap)?.is_some() {
            return Err(resource(format!(
                "TAP {:?} already exists; inspect ownership and clean it before preparing networks",
                net.tap
            )));
        }
    }
    let mut created = Vec::new();
    let result = (|| {
        for (index, net) in config.network.iter().enumerate() {
            let link = backend.create(&net.tap, &marker(config, index))?;
            created.push((index, link.index));
            owned(&link, config, index)?;
            if require_bridge(backend, &net.bridge)?.index != bridges[index].index {
                return Err(resource("bridge identity changed during TAP preparation"));
            }
            same_owned(backend, config, index, link.index)?;
            backend.set(&net.tap, &["mtu", &bridges[index].mtu.to_string()])?;
            same_owned(backend, config, index, link.index)?;
            backend.set(&net.tap, &["master", &net.bridge])?;
            same_owned(backend, config, index, link.index)?;
            backend.set(&net.tap, &["up"])?;
            let final_link = same_owned(backend, config, index, link.index)?;
            if !final_link.up
                || final_link.master.as_deref() != Some(&net.bridge)
                || final_link.mtu != bridges[index].mtu
            {
                return Err(resource("TAP bridge/up verification failed"));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        for (index, identity) in created.into_iter().rev() {
            if let Err(cleanup) = remove_owned(backend, config, index, identity) {
                failures.push(cleanup.to_string());
            }
        }
        return Err(if failures.is_empty() {
            error
        } else {
            resource(format!(
                "{error}; TAP rollback incomplete: {}",
                failures.join("; ")
            ))
        });
    }
    Ok(())
}
pub(crate) fn cleanup_unlocked(config: &InstanceConfig) -> Result<()> {
    cleanup_with(&Linux, config)
}
fn cleanup_with(backend: &impl Backend, config: &InstanceConfig) -> Result<()> {
    validate(config)?;
    let mut present = Vec::new();
    // Fail closed on any foreign interface before deleting even one owned interface.
    for (index, net) in config.network.iter().enumerate() {
        if let Some(link) = backend.link(&net.tap)? {
            owned(&link, config, index)?;
            present.push((index, link.index));
        }
    }
    for (index, identity) in present.into_iter().rev() {
        remove_owned(backend, config, index, identity)?;
    }
    Ok(())
}
fn offline(host: &HostConfig, name: &str, prepare: bool) -> Result<()> {
    host.validate()?;
    metadata::validate_name("instance", name)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(resource(format!("instance {name:?} was not found")));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    instance::configs_unlocked(host)?;
    let config = instance::load(host, name)?;
    if !["stopped", "failed"].contains(&instance::runtime_state(host, name)?)
        || instance::offline_runtime_state(host, name)? != "stopped"
    {
        return Err(resource(
            "instance runtime state is unknown; refusing offline network modification",
        ));
    }
    if prepare {
        prepare_unlocked(&config)
    } else {
        cleanup_unlocked(&config)
    }
}
/// Offline phase-6 validation entry point. No runtime files, units or VM are created.
pub fn prepare(host: &HostConfig, name: &str) -> Result<()> {
    offline(host, name, true)
}
pub fn cleanup(host: &HostConfig, name: &str) -> Result<()> {
    offline(host, name, false)
}
/// Preserve ownership metadata while a planned TAP exists, even if it belongs to someone else.
pub(crate) fn ensure_absent(config: &InstanceConfig) -> Result<()> {
    for net in &config.network {
        if Linux.link(&net.tap)?.is_some() {
            return Err(resource(format!(
                "interface {:?} still exists; clean managed TAPs before deleting instance metadata",
                net.tap
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    pub address: std::net::IpAddr,
    pub neighbor_state: String,
    pub source: &'static str,
}
#[derive(Debug)]
pub struct IpObservation {
    pub status: &'static str,
    pub queried_at_unix: Option<u64>,
    pub addresses: Vec<Address>,
    pub reason: Option<String>,
}
/// Passive observation only. Caller decides whether the instance is running.
/// MAC needs both a matching neighbor and the corresponding bridge FDB port.
pub fn observe(config: &InstanceConfig, index: usize) -> Result<IpObservation> {
    validate(config)?;
    let net = config
        .network
        .get(index)
        .ok_or_else(|| resource("network index is out of range"))?;
    let result: Result<Vec<Address>> = (|| {
        require_bridge(&Linux, &net.bridge)?;
        let Some(link) = Linux.link(&net.tap)? else {
            return Ok(Vec::new());
        };
        owned(&link, config, index)?;
        if link.master.as_deref() != Some(&net.bridge) {
            return Ok(Vec::new());
        }
        let bridge = command::executable(Path::new("bridge"))
            .ok_or_else(|| resource("bridge utility is not installed"))?;
        let ip = command::executable(Path::new("ip"))
            .ok_or_else(|| resource("ip utility is not installed"))?;
        let fdb =
            command::network_output(&bridge, &["-color=never", "fdb", "show", "br", &net.bridge])
                .map_err(resource)?;
        let neighbors =
            command::network_output(&ip, &["-color=never", "neigh", "show", "dev", &net.bridge])
                .map_err(resource)?;
        let after = same_owned(&Linux, config, index, link.index)?;
        if after.master.as_deref() != Some(&net.bridge) {
            return Err(resource("TAP bridge changed during IP observation"));
        }
        Ok(addresses(net, &fdb, &neighbors))
    })();
    Ok(match result {
        Ok(addresses) => IpObservation {
            status: if addresses.is_empty() {
                "unknown"
            } else {
                "observed"
            },
            queried_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs()),
            addresses,
            reason: None,
        },
        Err(error) => IpObservation {
            status: "unavailable",
            queried_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs()),
            addresses: Vec::new(),
            reason: Some(error.to_string()),
        },
    })
}
fn addresses(net: &NetworkConfig, fdb: &str, neighbors: &str) -> Vec<Address> {
    let pair = |words: &[&str], key: &str, value: &str| words.windows(2).any(|w| w == [key, value]);
    let matching: Vec<_> = fdb
        .lines()
        .filter(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            words
                .first()
                .is_some_and(|s| s.eq_ignore_ascii_case(&net.mac))
                && pair(&words, "master", &net.bridge)
                && !words.contains(&"self")
                && !words.contains(&"permanent")
                && !words.contains(&"vlan")
        })
        .collect();
    // Ambiguous or moved MACs are unknown; do not claim a neighbor from another TAP.
    if matching.is_empty()
        || matching.iter().any(|line| {
            !pair(
                &line.split_whitespace().collect::<Vec<_>>(),
                "dev",
                &net.tap,
            )
        })
    {
        return Vec::new();
    }
    let mut found = Vec::new();
    for line in neighbors.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        if !words
            .windows(2)
            .any(|w| w[0] == "lladdr" && w[1].eq_ignore_ascii_case(&net.mac))
        {
            continue;
        }
        if words.contains(&"dev") && !pair(&words, "dev", &net.bridge) {
            continue;
        }
        let Some(address) = words
            .first()
            .and_then(|s| s.parse::<std::net::IpAddr>().ok())
        else {
            continue;
        };
        if address.is_unspecified() || address.is_multicast() || address.is_loopback() {
            continue;
        }
        let Some(state) = words
            .iter()
            .find(|s| ["REACHABLE", "STALE", "DELAY", "PROBE", "PERMANENT", "NOARP"].contains(s))
        else {
            continue;
        };
        let entry = Address {
            address,
            neighbor_state: (*state).into(),
            source: "bridge_neighbor_fdb",
        };
        if !found.contains(&entry) {
            found.push(entry);
        }
    }
    found.sort_by_key(|a| a.address);
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::{BootConfig, ComputeConfig, DiskConfig, NetworkMode};
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn config() -> InstanceConfig {
        InstanceConfig {
            schema_version: 1,
            managed_by: "chadm".into(),
            id: "a".repeat(32),
            name: "test01".into(),
            image: "base".into(),
            image_id: "b".repeat(32),
            windows: false,
            cloud_init: None,
            compute: ComputeConfig {
                cpus: 2,
                memory: "2G".into(),
            },
            boot: BootConfig::Firmware { firmware: None },
            disk: vec![DiskConfig {
                name: "root".into(),
                volume: "test01-root".into(),
                volume_id: "c".repeat(32),
            }],
            network: (0..2)
                .map(|i| NetworkConfig {
                    bridge: "virbr0".into(),
                    tap: format!("ch-test01-{i}"),
                    mac: format!("52:54:00:00:00:0{i}"),
                    mode: NetworkMode::Dhcp,
                })
                .collect(),
        }
    }
    fn bridge() -> Link {
        Link {
            index: 1,
            mtu: 9000,
            bridge: true,
            up: true,
            alias: String::new(),
            tun_flags: None,
            master: None,
        }
    }
    #[derive(Default)]
    struct Fake {
        links: RefCell<HashMap<String, Link>>,
        commands: RefCell<Vec<String>>,
        fail: RefCell<Option<String>>,
        replacement: RefCell<bool>,
    }
    impl Fake {
        fn new() -> Self {
            let fake = Self::default();
            fake.links.borrow_mut().insert("virbr0".into(), bridge());
            fake
        }
    }
    impl Backend for Fake {
        fn link(&self, name: &str) -> Result<Option<Link>> {
            Ok(self.links.borrow().get(name).cloned())
        }
        fn create(&self, name: &str, alias: &str) -> Result<Link> {
            let mut links = self.links.borrow_mut();
            if links.contains_key(name) {
                return Err(resource("collision"));
            }
            let link = Link {
                index: 10 + links.len() as u32,
                mtu: 1500,
                bridge: false,
                up: false,
                alias: alias.into(),
                tun_flags: Some(TAP_FLAGS | PERSIST),
                master: None,
            };
            links.insert(name.into(), link.clone());
            self.commands.borrow_mut().push(format!("create {name}"));
            Ok(link)
        }
        fn set(&self, name: &str, args: &[&str]) -> Result<()> {
            let operation = format!("{name} {}", args.join(" "));
            self.commands.borrow_mut().push(operation.clone());
            if self.fail.borrow().as_deref() == Some(&operation) {
                if *self.replacement.borrow() {
                    self.links.borrow_mut().get_mut(name).unwrap().index += 100;
                }
                return Err(resource("injected failure"));
            }
            let mut links = self.links.borrow_mut();
            let link = links.get_mut(name).unwrap();
            match args {
                ["master", master] => link.master = Some((*master).into()),
                ["up"] => link.up = true,
                ["mtu", mtu] => link.mtu = mtu.parse().map_err(|_| resource("invalid MTU"))?,
                _ => return Err(resource("unexpected command")),
            }
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.commands.borrow_mut().push(format!("delete {name}"));
            self.links.borrow_mut().remove(name);
            Ok(())
        }
    }

    #[test]
    fn bridge_validation_and_invalid_names_fail_before_mutation() {
        for kind in [0, 1, 2] {
            let fake = Fake::new();
            match kind {
                0 => {
                    fake.links.borrow_mut().clear();
                }
                1 => fake.links.borrow_mut().get_mut("virbr0").unwrap().bridge = false,
                _ => fake.links.borrow_mut().get_mut("virbr0").unwrap().up = false,
            }
            assert!(prepare_with(&fake, &config()).is_err());
            assert!(fake.commands.borrow().is_empty());
        }
        for name in [
            "../virbr0",
            "-bad",
            "a;touch /tmp/foo",
            "",
            "this-is-too-long",
        ] {
            assert!(bridge_status(name).is_err());
        }
    }
    #[test]
    fn creates_bridge_up_then_cleans_reverse_and_cleanup_is_idempotent() {
        let fake = Fake::new();
        let config = config();
        prepare_with(&fake, &config).unwrap();
        for (index, net) in config.network.iter().enumerate() {
            let link = fake.link(&net.tap).unwrap().unwrap();
            assert!(link.up);
            assert_eq!(link.mtu, 9000);
            assert_eq!(link.master.as_deref(), Some("virbr0"));
            assert_eq!(link.alias, marker(&config, index));
        }
        assert!(prepare_with(&fake, &config)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        cleanup_with(&fake, &config).unwrap();
        cleanup_with(&fake, &config).unwrap();
        assert_eq!(fake.links.borrow().len(), 1);
        assert_eq!(
            fake.commands
                .borrow()
                .iter()
                .filter(|s| s.starts_with("delete"))
                .cloned()
                .collect::<Vec<_>>(),
            vec!["delete ch-test01-1", "delete ch-test01-0"]
        );
        prepare_with(&fake, &config).unwrap(); // repeated prepare after cleanup retains MAC/config
        assert_eq!(config.network[0].mac, "52:54:00:00:00:00");
    }
    #[test]
    fn all_nics_preflight_and_foreign_devices_preserved() {
        let fake = Fake::new();
        fake.links
            .borrow_mut()
            .insert("ch-test01-1".into(), bridge());
        assert!(prepare_with(&fake, &config()).is_err());
        assert!(fake.commands.borrow().is_empty());
        assert!(fake.link("ch-test01-0").unwrap().is_none());
        assert!(cleanup_with(&fake, &config()).is_err());
        assert!(fake.commands.borrow().is_empty());
    }
    #[test]
    fn rejects_old_generation_wrong_kind_missing_marker_and_incompatible_flags() {
        for change in 0..4 {
            let fake = Fake::new();
            let config = config();
            prepare_with(&fake, &config).unwrap();
            let mut links = fake.links.borrow_mut();
            let link = links.get_mut("ch-test01-1").unwrap();
            match change {
                0 => link.alias = format!("chadm:{}:1", "d".repeat(32)),
                1 => link.tun_flags = None,
                2 => link.alias.clear(),
                _ => link.tun_flags = Some(1 | PERSIST),
            }
            drop(links);
            let before = fake.commands.borrow().len();
            assert!(cleanup_with(&fake, &config).is_err());
            assert_eq!(fake.commands.borrow().len(), before); // no partial cleanup
        }
    }
    #[test]
    fn rollback_all_new_taps_after_second_nic_failure() {
        let fake = Fake::new();
        *fake.fail.borrow_mut() = Some("ch-test01-1 up".into());
        let error = prepare_with(&fake, &config()).unwrap_err().to_string();
        assert!(error.contains("injected failure"));
        assert!(!error.contains("rollback incomplete"));
        assert_eq!(fake.links.borrow().len(), 1);
    }
    #[test]
    fn rollback_preserves_interface_replaced_during_failure() {
        let fake = Fake::new();
        *fake.fail.borrow_mut() = Some("ch-test01-1 up".into());
        *fake.replacement.borrow_mut() = true;
        let error = prepare_with(&fake, &config()).unwrap_err().to_string();
        assert!(error.contains("rollback incomplete"));
        assert!(error.contains("index changed"));
        assert!(fake.link("ch-test01-1").unwrap().is_some());
        assert!(fake.link("ch-test01-0").unwrap().is_none());
    }
    #[test]
    fn stale_partial_taps_can_be_cleaned_without_the_original_bridge() {
        let fake = Fake::new();
        let config = config();
        // Crash between persistence and master/up: alias already proves generation.
        fake.create(&config.network[0].tap, &marker(&config, 0))
            .unwrap();
        fake.links.borrow_mut().remove("virbr0");
        cleanup_with(&fake, &config).unwrap();
        cleanup_with(&fake, &config).unwrap();
        assert!(fake.link(&config.network[0].tap).unwrap().is_none());
        fake.links.borrow_mut().insert("virbr0".into(), bridge());
        prepare_with(&fake, &config).unwrap();
        assert!(fake.link(&config.network[0].tap).unwrap().unwrap().up);
    }
    #[test]
    fn parses_multiple_ips_stale_and_filters_other_macs_ports_and_invalid_neighbors() {
        let net = &config().network[0];
        let fdb = "52:54:00:00:00:00 dev ch-test01-0 master virbr0\n";
        let neighbors = "192.0.2.10 lladdr 52:54:00:00:00:00 REACHABLE\n2001:db8::10 lladdr 52:54:00:00:00:00 router STALE\nfe80::1 dev virbr0 lladdr 52:54:00:00:00:00 DELAY\n192.0.2.12 lladdr 52:54:00:00:00:01 REACHABLE\n192.0.2.13 lladdr 52:54:00:00:00:00 FAILED\n192.0.2.14 INCOMPLETE\n192.0.2.15 dev another lladdr 52:54:00:00:00:00 REACHABLE\ngarbage lladdr 52:54:00:00:00:00 REACHABLE\n";
        let result = addresses(net, fdb, neighbors);
        assert_eq!(result.len(), 3);
        assert!(result
            .iter()
            .any(|a| a.address.to_string() == "2001:db8::10" && a.neighbor_state == "STALE"));
        assert!(result.iter().all(|a| a.source == "bridge_neighbor_fdb"));
        for fdb in [
            "",
            "52:54:00:00:00:00 dev other master virbr0",
            "52:54:00:00:00:00 dev ch-test01-0 self",
            "52:54:00:00:00:00 dev ch-test01-0 master virbr0 permanent",
            "52:54:00:00:00:00 dev ch-test01-0 master other",
            "52:54:00:00:00:00 dev ch-test01-0 vlan 2 master virbr0",
        ] {
            assert!(addresses(net, fdb, neighbors).is_empty());
        }
        assert!(addresses(
            net,
            &format!("{fdb}52:54:00:00:00:00 dev other master virbr0"),
            neighbors
        )
        .is_empty());
    }
    #[test]
    fn malformed_config_cannot_mutate_network_or_invent_address() {
        let fake = Fake::new();
        let mut config = config();
        config.network[0].tap = "../../etc".into();
        assert!(prepare_with(&fake, &config).is_err());
        assert!(cleanup_with(&fake, &config).is_err());
        assert!(observe(&config, 0).is_err());
        assert!(fake.commands.borrow().is_empty());
    }
    #[test]
    fn sysfs_parser_reads_bridge_tap_and_distinguishes_missing_from_invalid() {
        let root = std::env::temp_dir().join(format!(
            "chadm-net-sysfs-{}",
            metadata::random_id().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let dir = root.join("tap0");
        fs::create_dir(&dir).unwrap();
        for (field, value) in [
            ("ifindex", "12\n"),
            ("mtu", "1500\n"),
            ("flags", "0x1\n"),
            ("ifalias", "chadm:marker\n"),
            ("tun_flags", "0x5802\n"),
        ] {
            fs::write(dir.join(field), value).unwrap();
        }
        std::os::unix::fs::symlink("../virbr0", dir.join("master")).unwrap();
        let link = read_link(&root, "tap0").unwrap().unwrap();
        assert_eq!(link.index, 12);
        assert!(link.up);
        assert!(!link.bridge);
        assert_eq!(link.master.as_deref(), Some("virbr0"));
        assert_eq!(link.tun_flags, Some(TAP_FLAGS | PERSIST));
        assert!(read_link(&root, "missing").unwrap().is_none());
        fs::write(dir.join("tun_flags"), "invalid").unwrap();
        assert!(read_link(&root, "tap0").is_err());
        fs::remove_dir_all(root).unwrap(); // isolated fixture only
    }
}
