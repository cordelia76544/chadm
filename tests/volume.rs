use chadm::config::HostConfig;
use chadm::metadata::Lock;
use chadm::repo;
use chadm::volume::{self, VolumeConfig};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    host: HostConfig,
    config: PathBuf,
    storage: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "chadm-volume-test-{}",
            chadm::metadata::random_id().unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let host = HostConfig {
            data_dir: root.join("data"),
            runtime_dir: root.join("run"),
            ..Default::default()
        };
        let config = root.join("host.toml");
        fs::write(&config, toml::to_string(&host).unwrap()).unwrap();
        let storage = root.join("storage \"quoted\" \\本地");
        Self {
            root,
            host,
            config,
            storage,
        }
    }
    fn repo(&self) {
        repo::create(&self.host, "local", &self.storage).unwrap();
    }
    fn create(&self) -> VolumeConfig {
        volume::create(&self.host, "data", "local", 64 << 20).unwrap()
    }
    fn raw(&self) -> PathBuf {
        self.storage.join("volumes/data.raw")
    }
    fn marker(&self) -> PathBuf {
        self.storage.join("volumes/data.chadm.toml")
    }
    fn metadata(&self) -> PathBuf {
        self.host.layout().volumes.join("data/config.toml")
    }
    fn cli(&self, args: &[&str]) -> Output {
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
        // Only clean this unique test fixture, never a caller-supplied path.
        let _ = fs::set_permissions(
            self.storage.join("volumes"),
            fs::Permissions::from_mode(0o700),
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cli_roundtrip_creates_sparse_zero_filled_raw_and_preserves_repo() {
    let f = Fixture::new();
    f.repo();
    let output = f.cli(&[
        "volume", "create", "--name", "data", "--repo", "local", "--size", "64M",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: VolumeConfig = toml::from_str(&fs::read_to_string(f.metadata()).unwrap()).unwrap();
    let marker: VolumeConfig = toml::from_str(&fs::read_to_string(f.marker()).unwrap()).unwrap();
    assert_eq!(config, marker);
    let stat = fs::metadata(f.raw()).unwrap();
    assert_eq!(stat.len(), 64 << 20);
    assert_eq!(stat.ino(), config.inode);
    assert_eq!(stat.mode() & 0o777, 0o600);
    assert!(stat.blocks() * 512 < stat.len());
    let mut raw = fs::File::open(f.raw()).unwrap();
    let mut sample = [1; 32];
    raw.read_exact(&mut sample).unwrap();
    assert_eq!(sample, [0; 32]);
    raw.seek(SeekFrom::End(-32)).unwrap();
    raw.read_exact(&mut sample).unwrap();
    assert_eq!(sample, [0; 32]);
    let output = f.cli(&["volume", "show", "data", "--json"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"available\":true"));
    assert!(text.contains("\"size_bytes\":67108864"));
    assert!(text.contains("\\\"quoted\\\""));
    assert!(f.cli(&["volume", "list"]).status.success());
    assert_eq!(f.cli(&["repo", "delete", "local"]).status.code(), Some(1));
    assert!(f.cli(&["volume", "delete", "data"]).status.success());
    assert!(!f.raw().exists());
    assert!(!f.marker().exists());
    assert!(!f.metadata().exists());
    assert!(f.storage.is_dir());
    assert!(repo::show(&f.host, "local").unwrap().available);
    assert_eq!(f.cli(&["volume", "list", "--json"]).stdout, b"[]\n");
    repo::delete(&f.host, "local").unwrap();
}

#[test]
fn missing_queries_and_repo_do_not_create_resource_directories() {
    let f = Fixture::new();
    assert_eq!(f.cli(&["volume", "list", "--json"]).stdout, b"[]\n");
    assert_eq!(f.cli(&["volume", "show", "data"]).status.code(), Some(1));
    assert_eq!(f.cli(&["volume", "delete", "data"]).status.code(), Some(1));
    let error = volume::create(&f.host, "data", "missing", 1024)
        .unwrap_err()
        .to_string();
    assert!(error.contains("repo \"missing\" was not found"));
    assert!(!f.host.data_dir.exists());
    assert!(!f.host.runtime_dir.exists());
}

#[test]
fn duplicates_are_global_and_list_is_sorted() {
    let f = Fixture::new();
    f.repo();
    let first = f.create();
    assert!(volume::create(&f.host, "data", "local", 1024)
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    repo::create(&f.host, "second", &f.root.join("second")).unwrap();
    assert!(volume::create(&f.host, "data", "second", 1024).is_err());
    volume::create(&f.host, "alpha", "second", 1024).unwrap();
    let list = volume::list(&f.host).unwrap();
    assert_eq!(list[0].config.name, "alpha");
    assert_eq!(list[1].config, first);
}

#[test]
fn invalid_names_sizes_and_unsupported_operations_fail_before_writes() {
    let f = Fixture::new();
    for name in ["../data", "a/b", "a;echo", "UPPER"] {
        assert!(volume::create(&f.host, name, "local", 1024).is_err());
    }
    for size in [0, u64::MAX] {
        assert!(volume::create(&f.host, "data", "local", size).is_err());
    }
    for size in ["0", "1.5G", "9223372036854775808"] {
        assert_eq!(
            f.cli(&["volume", "create", "--name", "data", "--repo", "local", "--size", size])
                .status
                .code(),
            Some(2)
        );
    }
    assert_eq!(f.cli(&["volume", "attach", "data"]).status.code(), Some(2));
    assert_eq!(
        f.cli(&["volume", "create", "--name", "data", "--driver", "file", "--size", "1G"])
            .status
            .code(),
        Some(2)
    );
    assert!(!f.host.data_dir.exists());
}

#[test]
fn orphan_payload_or_marker_is_never_overwritten() {
    let f = Fixture::new();
    f.repo();
    for path in [f.raw(), f.marker()] {
        fs::write(&path, "preserve").unwrap();
        assert!(volume::create(&f.host, "data", "local", 1024).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "preserve");
        assert!(!f.metadata().exists());
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn replaced_inode_and_resized_payload_block_deletion() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let saved = f.storage.join("saved.raw");
    fs::rename(f.raw(), &saved).unwrap();
    fs::write(f.raw(), "foreign").unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert_eq!(fs::read_to_string(f.raw()).unwrap(), "foreign");
    assert!(!volume::show(&f.host, "data").unwrap().available);
    fs::remove_file(f.raw()).unwrap();
    fs::rename(&saved, f.raw()).unwrap();
    let raw = fs::OpenOptions::new().write(true).open(f.raw()).unwrap();
    raw.set_len(1024).unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(f.metadata().exists());
}

#[test]
fn matching_inode_and_size_without_file_ownership_id_are_insufficient() {
    let f = Fixture::new();
    f.repo();
    let mut config = f.create();
    fs::rename(f.raw(), f.storage.join("saved.raw")).unwrap();
    let file = fs::File::create(f.raw()).unwrap();
    file.set_len(config.size_bytes).unwrap();
    fs::set_permissions(f.raw(), fs::Permissions::from_mode(0o600)).unwrap();
    config.inode = file.metadata().unwrap().ino();
    let text = toml::to_string(&config).unwrap();
    fs::write(f.metadata(), &text).unwrap();
    fs::write(f.marker(), &text).unwrap();
    let info = volume::show(&f.host, "data").unwrap();
    assert!(!info.available);
    assert!(info.reason.unwrap().contains("ownership attribute"));
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(f.raw().exists());
}

#[test]
fn symlinks_and_hardlinks_are_rejected_without_touching_targets() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let saved = f.storage.join("saved.raw");
    fs::rename(f.raw(), &saved).unwrap();
    std::os::unix::fs::symlink(&saved, f.raw()).unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(saved.exists());
    fs::remove_file(f.raw()).unwrap();
    fs::hard_link(&saved, f.raw()).unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    fs::remove_file(f.raw()).unwrap();
    fs::rename(&saved, f.raw()).unwrap();
    let marker = f.storage.join("saved.toml");
    fs::rename(f.marker(), &marker).unwrap();
    std::os::unix::fs::symlink(&marker, f.marker()).unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(f.raw().exists());
    assert!(marker.exists());
}

#[test]
fn marker_mismatch_and_repo_generation_changes_block_deletion() {
    let f = Fixture::new();
    f.repo();
    let volume = f.create();
    let marker = fs::read_to_string(f.marker()).unwrap();
    fs::write(f.marker(), marker.replace(&volume.id, &"0".repeat(32))).unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    fs::write(f.marker(), &marker).unwrap();
    let mut config = volume.clone();
    config.repo_id = "0".repeat(32);
    fs::write(f.metadata(), toml::to_string(&config).unwrap()).unwrap();
    let info = volume::show(&f.host, "data").unwrap();
    assert!(!info.available);
    assert!(info.reason.unwrap().contains("different generation"));
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(f.raw().exists());
}

#[test]
fn metadata_reference_blocks_repo_delete_even_after_payload_is_lost() {
    let f = Fixture::new();
    f.repo();
    f.create();
    fs::remove_file(f.raw()).unwrap();
    fs::remove_file(f.marker()).unwrap();
    assert!(!volume::show(&f.host, "data").unwrap().available);
    let error = repo::delete(&f.host, "local").unwrap_err().to_string();
    assert!(error.contains("referenced by volumes"));
    assert!(f.storage.join(".chadm-repo.toml").exists());
}

#[test]
fn metadata_corruption_unfinished_transactions_and_unknown_entries_block_writes() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let config = fs::read_to_string(f.metadata()).unwrap();
    fs::write(
        f.metadata(),
        config.replace("schema_version = 1", "schema_version = 2"),
    )
    .unwrap();
    assert!(volume::list(&f.host).is_err());
    assert!(volume::delete(&f.host, "data").is_err());
    fs::write(f.metadata(), config).unwrap();
    let extra = f.metadata().parent().unwrap().join("precious");
    fs::write(&extra, "keep").unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert_eq!(fs::read_to_string(&extra).unwrap(), "keep");
    fs::remove_file(extra).unwrap();
    let stage = f.host.layout().volumes.join(".create-incomplete");
    fs::DirBuilder::new().mode(0o700).create(stage).unwrap();
    assert!(volume::list(&f.host).is_err());
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(volume::create(&f.host, "other", "local", 1024).is_err());
}

#[test]
fn lock_conflict_and_unknown_instance_references_leave_payload_intact() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let lock = Lock::acquire(&f.host.data_dir, true).unwrap().unwrap();
    let output = f.cli(&["volume", "delete", "data"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("another chadm command"));
    drop(lock);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(f.host.layout().instances)
        .unwrap();
    fs::write(f.host.layout().instances.join("unknown"), "unsupported").unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(f.raw().exists());
}

#[test]
fn storage_permission_failure_rolls_back_creation_and_deletion_metadata() {
    let f = Fixture::new();
    f.repo();
    let parent = f.storage.join("volumes");
    if fs::metadata(&parent).unwrap().uid() == 0 {
        return;
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(volume::create(&f.host, "data", "local", 1024).is_err());
    assert!(!f.raw().exists());
    assert_eq!(fs::read_dir(f.host.layout().volumes).unwrap().count(), 0);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let original = f.create();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(volume::delete(&f.host, "data").is_err());
    assert_eq!(volume::show(&f.host, "data").unwrap().config, original);
    assert!(f.raw().exists());
    assert!(f.marker().exists());
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    volume::delete(&f.host, "data").unwrap();
}

#[test]
fn unavailable_repo_blocks_volume_creation_and_deletion() {
    let f = Fixture::new();
    f.repo();
    f.create();
    fs::remove_file(f.storage.join(".chadm-repo.toml")).unwrap();
    assert!(volume::create(&f.host, "other", "local", 1024).is_err());
    assert!(volume::delete(&f.host, "data").is_err());
    assert!(!volume::show(&f.host, "data").unwrap().available);
    assert!(f.raw().exists());
}

#[test]
fn doctor_detects_missing_volume_ownership_evidence() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let output = f.cli(&["doctor"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text
        .lines()
        .find(|line| line.starts_with("volumes "))
        .unwrap()
        .contains("OK"));
    fs::remove_file(f.marker()).unwrap();
    let output = f.cli(&["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text
        .lines()
        .find(|line| line.starts_with("volumes "))
        .unwrap()
        .contains("FAIL"));
}
