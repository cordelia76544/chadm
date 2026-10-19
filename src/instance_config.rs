use crate::error::{Error, Result};
use crate::{metadata, volume};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    pub schema_version: u32,
    pub managed_by: String,
    pub id: String,
    pub name: String,
    pub image: String,
    pub image_id: String,
    #[serde(default)]
    pub windows: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<CloudInitConfig>,
    pub compute: ComputeConfig,
    pub boot: BootConfig,
    pub disk: Vec<DiskConfig>,
    pub network: Vec<NetworkConfig>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudInitConfig {
    pub image: String,
    pub image_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputeConfig {
    pub cpus: u16,
    pub memory: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum BootConfig {
    Firmware {
        #[serde(default, skip_serializing)]
        firmware: Option<PathBuf>,
    },
    Direct {
        kernel: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initramfs: Option<PathBuf>,
        cmdline: String,
    },
}
impl BootConfig {
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Firmware { .. } => "firmware",
            Self::Direct { .. } => "direct",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    pub name: String,
    pub volume: String,
    pub volume_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    pub bridge: String,
    pub tap: String,
    pub mac: String,
    #[serde(default)]
    pub mode: NetworkMode,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    #[default]
    Dhcp,
}

#[derive(Debug, Clone)]
pub struct VolumeSpec {
    pub name: String,
    pub size_bytes: u64,
}
#[derive(Debug, Clone)]
pub struct NetworkSpec {
    pub bridge: String,
    pub mac: Option<String>,
}
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub name: String,
    pub image: String,
    pub repo: Option<String>,
    pub cloud_init: Option<String>,
    pub no_cloud_init: bool,
    pub cpus: u16,
    pub ram: u64,
    pub boot: BootConfig,
    pub volumes: Vec<VolumeSpec>,
    pub networks: Vec<NetworkSpec>,
}
impl CreateOptions {
    pub fn new(name: &str, image: &str) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            repo: None,
            cloud_init: None,
            no_cloud_init: false,
            cpus: 2,
            ram: 2 << 30,
            boot: BootConfig::Firmware { firmware: None },
            volumes: Vec::new(),
            networks: Vec::new(),
        }
    }
}

pub fn parse_volume(value: &str) -> Result<VolumeSpec> {
    let mut parts = value.split(',');
    let name = parts.next().unwrap_or("");
    metadata::validate_name("disk", name)?;
    let size = parts
        .next()
        .and_then(|part| part.strip_prefix("size="))
        .ok_or_else(|| Error::Resource("use --volume root,size=20G or data,size=30G".into()))?;
    if parts.next().is_some() {
        return Err(Error::Resource(
            "unknown or duplicate volume options".into(),
        ));
    }
    Ok(VolumeSpec {
        name: name.into(),
        size_bytes: volume::parse_size(size)?,
    })
}

pub fn parse_mac(value: &str) -> Result<String> {
    let parts: Vec<_> = value.split(':').collect();
    if parts.len() != 6
        || parts
            .iter()
            .any(|part| part.len() != 2 || !part.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::Resource(format!(
            "invalid MAC address {value:?}; use six colon-separated hexadecimal octets"
        )));
    }
    let mut bytes = [0u8; 6];
    for (index, part) in parts.iter().enumerate() {
        bytes[index] = u8::from_str_radix(part, 16)
            .map_err(|_| Error::Resource("invalid MAC address".into()))?;
    }
    if bytes[0] & 1 != 0 || bytes == [0; 6] {
        return Err(Error::Resource(
            "MAC must be a nonzero unicast address".into(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}
pub(crate) fn bridge_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 15
        || !name.as_bytes()[0].is_ascii_alphanumeric() && name.as_bytes()[0] != b'_'
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(Error::Resource(format!(
            "invalid bridge name {name:?}; use 1..15 ASCII letters, digits, '_', '-' or '.'"
        )));
    }
    Ok(())
}
pub fn parse_network(value: &str) -> Result<NetworkSpec> {
    let mut bridge = None;
    let mut mac = None;
    let mut dhcp = false;
    for part in value.split(',') {
        if let Some(value) = part.strip_prefix("bridge=") {
            if bridge.replace(value.to_string()).is_some() {
                return Err(Error::Resource("duplicate bridge option".into()));
            }
            bridge_name(value)?;
        } else if let Some(value) = part.strip_prefix("mac=") {
            if mac.replace(parse_mac(value)?).is_some() {
                return Err(Error::Resource("duplicate MAC option".into()));
            }
        } else if part == "dhcp" && !dhcp {
            dhcp = true;
        } else {
            return Err(Error::Resource(format!(
                "unsupported or duplicate network option {part:?}; use bridge=virbr0,dhcp"
            )));
        }
    }
    Ok(NetworkSpec {
        bridge: bridge.ok_or_else(|| Error::Resource("network requires bridge=<name>".into()))?,
        mac,
    })
}
pub(crate) fn tap_name(name: &str, index: usize) -> Result<String> {
    let tap = format!("ch-{name}-{index}");
    if tap.len() > 15 {
        return Err(Error::Resource(format!(
            "TAP name {tap:?} exceeds Linux's 15-byte limit; use a shorter instance name"
        )));
    }
    Ok(tap)
}
pub(crate) fn memory_string(size: u64) -> String {
    for (shift, suffix) in [(40, "T"), (30, "G"), (20, "M"), (10, "K")] {
        if size >= 1 << shift && size % (1 << shift) == 0 {
            return format!("{}{suffix}", size >> shift);
        }
    }
    size.to_string()
}
pub(crate) fn new_mac(used: &HashSet<String>) -> Result<String> {
    for _ in 0..100 {
        let random = metadata::random_id()?;
        let mac = format!(
            "52:54:{}:{}:{}:{}",
            &random[0..2],
            &random[2..4],
            &random[4..6],
            &random[6..8]
        );
        if !used.contains(&mac) {
            return Ok(mac);
        }
    }
    Err(Error::Resource(
        "cannot allocate a unique MAC address".into(),
    ))
}

pub(crate) fn validate(config: &InstanceConfig) -> Result<()> {
    if config.windows {
        if !cfg!(target_arch = "x86_64") {
            return Err(Error::Resource(
                "Windows profile currently requires an x86_64 KVM host".into(),
            ));
        }
        if !matches!(config.boot, BootConfig::Firmware { .. }) {
            return Err(Error::Resource(
                "Windows images require --boot firmware; direct boot is not supported".into(),
            ));
        }
    }
    if let Some(seed) = &config.cloud_init {
        metadata::validate_name("cloud-init image", &seed.image)?;
        if !metadata::valid_id(&seed.image_id) {
            return Err(Error::Resource(
                "invalid cloud-init image generation".into(),
            ));
        }
    }
    metadata::validate_name("instance", &config.name)?;
    metadata::validate_name("image", &config.image)?;
    if config.schema_version != 1
        || config.managed_by != "chadm"
        || !metadata::valid_id(&config.id)
        || !metadata::valid_id(&config.image_id)
    {
        return Err(Error::Resource(
            "unsupported or invalid instance ownership metadata".into(),
        ));
    }
    if config.compute.cpus == 0 {
        return Err(Error::Resource("CPUs must be greater than zero".into()));
    }
    volume::parse_size(&config.compute.memory)
        .map_err(|error| Error::Resource(format!("invalid instance memory: {error}")))?;
    match &config.boot {
        BootConfig::Firmware { .. } => {}
        BootConfig::Direct {
            kernel,
            initramfs,
            cmdline,
        } => {
            metadata::validate_path(kernel)?;
            if let Some(path) = initramfs {
                metadata::validate_path(path)?;
            }
            if cmdline.is_empty() || cmdline.len() > 8192 || cmdline.chars().any(char::is_control) {
                return Err(Error::Resource("direct boot requires a nonempty cmdline of at most 8192 bytes without control characters".into()));
            }
        }
    }
    if config.disk.is_empty() || config.disk.len() > 32 || config.disk[0].name != "root" {
        return Err(Error::Resource(
            "instance requires root as its first disk and at most 32 disks".into(),
        ));
    }
    let mut aliases = HashSet::new();
    let mut names = HashSet::new();
    let mut ids = HashSet::new();
    for disk in &config.disk {
        metadata::validate_name("disk", &disk.name)?;
        metadata::validate_name("volume", &disk.volume)?;
        if !metadata::valid_id(&disk.volume_id)
            || !aliases.insert(&disk.name)
            || !names.insert(&disk.volume)
            || !ids.insert(&disk.volume_id)
        {
            return Err(Error::Resource(
                "invalid or duplicate instance disk reference".into(),
            ));
        }
    }
    if config.network.len() > 16 {
        return Err(Error::Resource("at most 16 networks are supported".into()));
    }
    let mut macs = HashSet::new();
    for (index, net) in config.network.iter().enumerate() {
        bridge_name(&net.bridge)?;
        if net.tap != tap_name(&config.name, index)?
            || parse_mac(&net.mac)? != net.mac
            || !macs.insert(&net.mac)
        {
            return Err(Error::Resource(
                "invalid TAP or duplicate/noncanonical MAC in instance configuration".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_boot_files(
    boot: &BootConfig,
    firmware_path: &Path,
    required: bool,
) -> Result<()> {
    let readable = |path: &Path| -> Result<File> {
        metadata::validate_path(path)?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(0x20000 | 0x800)
            .open(path)
            .map_err(|error| metadata::io_error("open boot file", path, error))?;
        let stat = file
            .metadata()
            .map_err(|error| metadata::io_error("inspect boot file", path, error))?;
        if !stat.is_file() || stat.len() == 0 {
            return Err(Error::Resource(format!(
                "boot file {path:?} must be a nonempty regular file"
            )));
        }
        Ok(file)
    };
    match boot {
        BootConfig::Firmware { .. } => {
            if !required
                && std::fs::symlink_metadata(firmware_path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(());
            }
            metadata::directory(
                firmware_path
                    .parent()
                    .ok_or_else(|| Error::Resource("invalid firmware path".into()))?,
                false,
            )?;
            let file = metadata::open_regular(firmware_path, false)?;
            if file
                .metadata()
                .map_err(|e| metadata::io_error("inspect firmware", firmware_path, e))?
                .len()
                == 0
            {
                return Err(Error::Resource(
                    "CLOUDHV.fd must be a nonempty regular file".into(),
                ));
            }
        }
        BootConfig::Direct {
            kernel, initramfs, ..
        } => {
            readable(kernel)?;
            if let Some(path) = initramfs {
                readable(path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_volume_and_network_input() {
        assert_eq!(parse_volume("root,size=20G").unwrap().size_bytes, 20 << 30);
        for input in [
            "root",
            "root,size=0",
            "root,size=1G,size=2G",
            "../root,size=2G",
        ] {
            assert!(parse_volume(input).is_err());
        }
        assert_eq!(parse_network("bridge=virbr0").unwrap().bridge, "virbr0");
        let spec = parse_network("bridge=virbr0,mac=52:54:AA:BB:CC:DD,dhcp").unwrap();
        assert_eq!(spec.mac.as_deref(), Some("52:54:aa:bb:cc:dd"));
        for input in [
            "dhcp",
            "bridge=virbr0,static",
            "bridge=../x,dhcp",
            "bridge=br0,bridge=br1,dhcp",
            "bridge=br0,dhcp,dhcp",
            "bridge=br0,mac=ff:ff:ff:ff:ff:ff,dhcp",
        ] {
            assert!(parse_network(input).is_err(), "{input}");
        }
        assert!(tap_name("abcdefghij", 0).is_ok());
        assert!(tap_name("abcdefghijk", 0).is_err());
        assert!(tap_name("abcdefghij", 10).is_err());
        assert!(parse_mac("00:00:00:00:00:00").is_err());
    }
}
