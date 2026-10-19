use chadm::{config::HostConfig, image, instance, metadata, network, repo};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    host: HostConfig,
    name: String,
    keep: std::cell::Cell<bool>,
}
impl Fixture {
    fn new(bridge: &str) -> Self {
        let id = metadata::random_id().unwrap();
        let root = std::env::temp_dir().join(format!("chadm-network-{id}"));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let host = HostConfig {
            data_dir: root.join("data"),
            runtime_dir: root.join("run"),
            ..Default::default()
        };
        repo::create(&host, "local", &root.join("storage")).unwrap();
        let source = root.join("input.raw");
        fs::write(&source, b"network test image").unwrap();
        image::create(&host, "base", "local", &source).unwrap();
        let name = format!("n-{}", &id[..6]);
        let mut options = instance::CreateOptions::new(&name, "base");
        options
            .networks
            .push(instance::parse_network(&format!("bridge={bridge},dhcp")).unwrap());
        instance::create(&host, &options).unwrap();
        Self {
            root,
            host,
            name,
            keep: std::cell::Cell::new(false),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.keep.get() {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}
#[test]
fn missing_bridge_error_no_runtime_or_network_created() {
    let f = Fixture::new("ch-nonexistent");
    let before = instance::show(&f.host, &f.name).unwrap().config;
    let error = network::prepare(&f.host, &f.name).unwrap_err().to_string();
    assert!(
        error.contains("bridge \"ch-nonexistent\" does not exist"),
        "{error}"
    );
    assert!(!f.host.runtime_dir.exists());
    assert!(!std::path::Path::new("/sys/class/net")
        .join(&before.network[0].tap)
        .exists());
    network::cleanup(&f.host, &f.name).unwrap();
    assert_eq!(instance::show(&f.host, &f.name).unwrap().config, before);
    let report = chadm::doctor::run(&f.host);
    let check = report.checks.iter().find(|c| c.name == "bridges").unwrap();
    assert_eq!(check.status, chadm::doctor::Status::Fail);
    assert!(check.detail.contains("ch-nonexistent"));
    let observation = network::observe(&before, 0).unwrap();
    assert_eq!(observation.status, "unavailable");
    assert!(observation.addresses.is_empty());
    assert!(network::observe(&before, 1).is_err());
}
#[test]
fn runtime_artifacts_and_lock_conflicts_block_offline_modification() {
    let f = Fixture::new("ch-nonexistent");
    let lock = metadata::Lock::acquire(&f.host.data_dir, true).unwrap();
    for result in [
        network::prepare(&f.host, &f.name),
        network::cleanup(&f.host, &f.name),
    ] {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("another chadm command"));
    }
    drop(lock);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&f.host.runtime_dir)
        .unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(f.host.runtime_dir.join(&f.name))
        .unwrap();
    for result in [
        network::prepare(&f.host, &f.name),
        network::cleanup(&f.host, &f.name),
    ] {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("runtime state is unknown"));
    }
}
#[test]
fn invalid_or_missing_instances_cannot_create_directories() {
    let root =
        std::env::temp_dir().join(format!("chadm-network-{}", metadata::random_id().unwrap()));
    let host = HostConfig {
        data_dir: root.join("data"),
        runtime_dir: root.join("run"),
        ..Default::default()
    };
    assert!(network::prepare(&host, "../../etc").is_err());
    assert!(network::cleanup(&host, "missing").is_err());
    assert!(!root.exists());
}

/// Explicit opt-in: creates/deletes only a random fixture's TAP on the supplied existing bridge.
#[test]
#[ignore = "requires root/CAP_NET_ADMIN, /dev/net/tun, and CHADM_TEST_BRIDGE; changes live TAP state"]
fn host_tap_lifecycle() {
    let bridge = std::env::var("CHADM_TEST_BRIDGE")
        .expect("set CHADM_TEST_BRIDGE to an existing UP Linux bridge");
    let f = Fixture::new(&bridge);
    struct Cleanup<'a>(&'a Fixture);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Err(error) = network::cleanup(&self.0.host, &self.0.name) {
                self.0.keep.set(true);
                eprintln!(
                    "TAP cleanup failed: {error}; preserving fixture {:?}",
                    self.0.root
                );
            }
        }
    }
    let _cleanup = Cleanup(&f);
    let before = instance::show(&f.host, &f.name).unwrap().config;
    let original_bridge = network::bridge_status(&bridge).unwrap();
    network::prepare(&f.host, &f.name).unwrap();
    let tap = &before.network[0].tap;
    let path = std::path::Path::new("/sys/class/net").join(tap);
    assert!(path.exists());
    assert_eq!(
        fs::read_to_string(path.join("ifalias")).unwrap().trim(),
        format!("chadm:{}:0", before.id)
    );
    let flags = fs::read_to_string(path.join("tun_flags")).unwrap();
    let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).unwrap();
    assert_eq!(flags & 0x5802, 0x5802); // TAP, NO_PI, VNET_HDR, PERSIST
    assert_eq!(
        fs::read_link(path.join("master"))
            .unwrap()
            .file_name()
            .unwrap(),
        bridge.as_str()
    );
    assert!(instance::delete(&f.host, &f.name)
        .unwrap_err()
        .to_string()
        .contains("still exists"));
    assert!(network::prepare(&f.host, &f.name).is_err());
    network::cleanup(&f.host, &f.name).unwrap();
    network::cleanup(&f.host, &f.name).unwrap();
    assert!(!path.exists());
    network::prepare(&f.host, &f.name).unwrap();
    network::cleanup(&f.host, &f.name).unwrap();
    assert_eq!(instance::show(&f.host, &f.name).unwrap().config, before);
    assert_eq!(network::bridge_status(&bridge).unwrap(), original_bridge);
    assert!(!f.host.runtime_dir.exists());
}
