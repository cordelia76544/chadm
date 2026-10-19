use chadm::{checkpoint, config::HostConfig, image, instance, metadata, repo, volume};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    process::Command,
};
struct Fixture {
    root: PathBuf,
    host: HostConfig,
    config: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("chadm-snap-{}", metadata::random_id().unwrap()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let host = HostConfig {
            data_dir: root.join("data"),
            runtime_dir: root.join("run"),
            ..Default::default()
        };
        let config = root.join("host.toml");
        fs::write(&config, toml::to_string(&host).unwrap()).unwrap();
        repo::create(&host, "local", &root.join("storage")).unwrap();
        let source = root.join("base.raw");
        fs::File::create(&source).unwrap().set_len(2 << 20).unwrap();
        image::create(&host, "base", "local", &source).unwrap();
        let mut options = instance::CreateOptions::new("test01", "base");
        options.no_cloud_init = true;
        options
            .volumes
            .push(instance::parse_volume("data,size=2M").unwrap());
        instance::create(&host, &options).unwrap();
        Self { root, host, config }
    }
    fn payload(&self, name: &str) -> PathBuf {
        volume::show(&self.host, name).unwrap().path.unwrap()
    }
    fn write(&self, name: &str, bytes: &[u8]) {
        let path = self.payload(name);
        let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(123)).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn read(&self, name: &str) -> [u8; 8] {
        let mut file = fs::File::open(self.payload(name)).unwrap();
        file.seek(SeekFrom::Start(123)).unwrap();
        let mut bytes = [0; 8];
        file.read_exact(&mut bytes).unwrap();
        bytes
    }
    fn cli(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_chadm"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn file_snapshot_restores_all_disks_preserving_identity_and_refs() {
    let f = Fixture::new();
    f.write("test01-root", b"root-old");
    f.write("test01-data", b"data-old");
    let root_inode = fs::metadata(f.payload("test01-root")).unwrap().ino();
    let output = f.cli(&[
        "snapshot",
        "create",
        "--name",
        "before-upgrade",
        "--instance",
        "test01",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(checkpoint::list(&f.host).unwrap().len(), 1);
    let info = checkpoint::show(&f.host, "before-upgrade").unwrap();
    assert!(info.available);
    assert_eq!(info.volumes.len(), 2);
    assert!(info.json().contains("file-reflink-or-copy"));
    assert!(instance::delete(&f.host, "test01")
        .unwrap_err()
        .to_string()
        .contains("snapshots"));
    assert!(volume::delete(&f.host, "test01-root").is_err());
    assert!(image::delete(&f.host, "base").is_err());
    volume::create(&f.host, "extra", "local", 1 << 20).unwrap();
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    f.write("test01-root", b"root-new");
    f.write("test01-data", b"data-new");
    checkpoint::restore(&f.host, "before-upgrade").unwrap();
    assert_eq!(&f.read("test01-root"), b"root-old");
    assert_eq!(&f.read("test01-data"), b"data-old");
    assert_eq!(
        fs::metadata(f.payload("test01-root")).unwrap().ino(),
        root_inode
    );
    assert!(volume::show(&f.host, "test01-root").unwrap().available);
    checkpoint::restore(&f.host, "before-upgrade").unwrap();
    checkpoint::delete(&f.host, "before-upgrade", false).unwrap();
    assert!(checkpoint::list(&f.host).unwrap().is_empty());
    volume::attach(&f.host, "extra", "test01").unwrap();
}
#[test]
fn snapshot_payload_tampering_fails_before_any_live_disk_changes() {
    let f = Fixture::new();
    f.write("test01-root", b"old-root");
    f.write("test01-data", b"old-data");
    checkpoint::create(&f.host, "s1", "test01").unwrap();
    f.write("test01-root", b"new-root");
    f.write("test01-data", b"new-data");
    let manifest: toml::Value = toml::from_str(
        &fs::read_to_string(f.host.data_dir.join("snapshots/s1/config.toml")).unwrap(),
    )
    .unwrap();
    let id = manifest["id"].as_str().unwrap();
    let data = volume::show(&f.host, "test01-data").unwrap();
    let snapshot = f
        .root
        .join("storage/volumes")
        .join(format!(".chadm-snapshot-{id}-{}.raw", data.config.id));
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(checkpoint::restore(&f.host, "s1").is_err());
    assert_eq!(&f.read("test01-root"), b"new-root");
    assert_eq!(&f.read("test01-data"), b"new-data");
    assert!(!checkpoint::show(&f.host, "s1").unwrap().available);
    assert!(checkpoint::delete(&f.host, "s1", false).is_err());
    assert!(snapshot.exists());
}

#[test]
fn replaced_live_volume_is_detected_before_restoring_any_other_disk() {
    let f = Fixture::new();
    f.write("test01-root", b"old-root");
    checkpoint::create(&f.host, "s1", "test01").unwrap();
    f.write("test01-root", b"new-root");
    let path = f.payload("test01-data");
    fs::rename(&path, path.with_extension("preserved")).unwrap();
    fs::File::create(&path).unwrap().set_len(2 << 20).unwrap();
    assert!(checkpoint::restore(&f.host, "s1").is_err());
    assert_eq!(&f.read("test01-root"), b"new-root");
}
#[test]
fn config_drift_invalid_names_and_read_queries_are_safe() {
    let f = Fixture::new();
    for name in ["../bad", "UPPER", "a;reboot"] {
        assert!(checkpoint::create(&f.host, name, "test01").is_err());
    }
    assert!(!f.host.data_dir.join("snapshots").exists());
    checkpoint::create(&f.host, "s1", "test01").unwrap();
    assert!(checkpoint::create(&f.host, "s1", "test01").is_err());
    let path = f.host.layout().instances.join("test01/config.toml");
    let old = fs::read_to_string(&path).unwrap();
    fs::write(&path, old.replace("cpus = 2", "cpus = 3")).unwrap();
    assert!(checkpoint::restore(&f.host, "s1").is_err());
    assert!(!checkpoint::show(&f.host, "s1").unwrap().available);
    let output = f.cli(&["snapshot", "show", "s1", "--json"]);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("\"available\":false"));
    fs::write(&path, old).unwrap();
    checkpoint::delete(&f.host, "s1", false).unwrap();
}
#[test]
fn interrupted_restore_blocks_start_and_delete_but_can_be_retried() {
    let f = Fixture::new();
    f.write("test01-root", b"old-root");
    checkpoint::create(&f.host, "s1", "test01").unwrap();
    f.write("test01-root", b"new-root");
    let path = f.host.data_dir.join("snapshots/s1/config.toml");
    let record = fs::read_to_string(&path)
        .unwrap()
        .replace("phase = \"ready\"", "phase = \"restoring\"");
    fs::write(&path, record).unwrap();
    assert!(checkpoint::delete(&f.host, "s1", false).is_err());
    assert_eq!(
        instance::show(&f.host, "test01").unwrap().state,
        "restoring"
    );
    assert!(checkpoint::create(&f.host, "s2", "test01").is_err());
    checkpoint::restore(&f.host, "s1").unwrap();
    assert_eq!(&f.read("test01-root"), b"old-root");
    assert_eq!(
        checkpoint::show(&f.host, "s1").unwrap().phase,
        checkpoint::Phase::Ready
    );
}
