use chadm::config::HostConfig;
use chadm::metadata::Lock;
use chadm::repo::{self, RepoConfig};
use std::fs;
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
            "chadm-repo-test-{}",
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
        let storage = root.join("storage");
        Self {
            root,
            host,
            config,
            storage,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_chadm"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }

    fn create(&self) -> RepoConfig {
        repo::create(&self.host, "local", &self.storage).unwrap()
    }

    fn metadata(&self) -> PathBuf {
        self.host.layout().repos.join("local/config.toml")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only the unique private test directory is recursively removed.
        let _ = fs::set_permissions(self.host.layout().repos, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cli_roundtrip_and_root_preservation() {
    let f = Fixture::new();
    let created = f.cli(&[
        "repo",
        "create",
        "--name",
        "local",
        "--driver",
        "file",
        "--path",
        f.storage.to_str().unwrap(),
    ]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let repo: RepoConfig = toml::from_str(&fs::read_to_string(f.metadata()).unwrap()).unwrap();
    let marker: RepoConfig =
        toml::from_str(&fs::read_to_string(f.storage.join(".chadm-repo.toml")).unwrap()).unwrap();
    assert_eq!(repo, marker);
    assert_eq!(fs::metadata(&f.storage).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(f.metadata()).unwrap().mode() & 0o777, 0o600);
    let output = f.cli(&["repo", "show", "local", "--json"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"available\":true"));
    assert!(text.contains("\"driver\":\"file\""));
    let output = f.cli(&["repo", "list"]);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains("local"));
    assert!(f.cli(&["repo", "delete", "local"]).status.success());
    assert!(f.storage.is_dir());
    assert_eq!(fs::read_dir(&f.storage).unwrap().count(), 0);
    assert!(!f.metadata().exists());
    assert_eq!(f.cli(&["repo", "list", "--json"]).stdout, b"[]\n");
    // The retained empty root can be registered again with a new ownership ID.
    let next = f.create();
    assert_ne!(next.id, repo.id);
}

#[test]
fn reads_and_missing_deletion_do_not_initialize_directories() {
    let f = Fixture::new();
    assert_eq!(f.cli(&["repo", "list", "--json"]).stdout, b"[]\n");
    let output = f.cli(&["repo", "show", "local"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("was not found"));
    assert_eq!(f.cli(&["repo", "delete", "local"]).status.code(), Some(1));
    assert!(!f.host.data_dir.exists());
    assert!(!f.host.runtime_dir.exists());
}

#[test]
fn duplicate_names_paths_and_overlaps_are_rejected() {
    let f = Fixture::new();
    let original = f.create();
    assert!(repo::create(&f.host, "local", &f.root.join("other"))
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    for path in [&f.storage, &f.storage.join("nested"), &f.root] {
        assert!(repo::create(&f.host, "other", path).is_err());
    }
    assert_eq!(repo::show(&f.host, "local").unwrap().config, original);
    assert!(!f.root.join("other").exists());
    let second = repo::create(&f.host, "alpha", &f.root.join("second")).unwrap();
    let repos = repo::list(&f.host).unwrap();
    assert_eq!(repos[0].config.name, second.name);
    assert_eq!(repos[1].config.name, "local");
}

#[test]
fn invalid_names_paths_and_unsupported_driver_do_not_touch_storage() {
    let f = Fixture::new();
    for name in ["../escape", "local/other", "a;touch", "LOCAL"] {
        assert!(repo::create(&f.host, name, &f.storage).is_err());
    }
    for path in [
        PathBuf::from("/"),
        "relative".into(),
        f.root.join("../bad"),
        f.host.data_dir.clone(),
        f.host.layout().repos.join("oops"),
        f.host.runtime_dir.clone(),
    ] {
        assert!(repo::create(&f.host, "local", &path).is_err());
    }
    assert_eq!(
        f.cli(&[
            "repo",
            "create",
            "--name",
            "local",
            "--driver",
            "zfs",
            "--path",
            f.storage.to_str().unwrap()
        ])
        .status
        .code(),
        Some(2)
    );
    assert!(!f.host.data_dir.exists());
    assert!(!f.storage.exists());
}

#[test]
fn preexisting_nonempty_directory_is_never_adopted() {
    let f = Fixture::new();
    fs::create_dir(&f.storage).unwrap();
    fs::write(f.storage.join("keep.raw"), "precious").unwrap();
    assert!(repo::create(&f.host, "local", &f.storage).is_err());
    assert_eq!(
        fs::read_to_string(f.storage.join("keep.raw")).unwrap(),
        "precious"
    );
    assert!(!f.storage.join("images").exists());
}

#[test]
fn nonempty_payload_and_unknown_entries_block_deletion() {
    let f = Fixture::new();
    f.create();
    for path in [
        f.storage.join("volumes/keep.raw"),
        f.storage.join("images/keep.raw"),
        f.storage.join("unknown"),
        f.metadata().parent().unwrap().join("unexpected"),
    ] {
        fs::write(&path, "preserve").unwrap();
        assert!(repo::delete(&f.host, "local").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "preserve");
        assert!(f.metadata().exists());
        fs::remove_file(path).unwrap();
    }
    assert!(repo::delete(&f.host, "local").is_ok());
}

#[test]
fn missing_or_mismatched_ownership_blocks_deletion() {
    let f = Fixture::new();
    f.create();
    let marker = f.storage.join(".chadm-repo.toml");
    let text = fs::read_to_string(&marker).unwrap();
    fs::remove_file(&marker).unwrap();
    assert!(repo::delete(&f.host, "local").is_err());
    assert!(!repo::show(&f.host, "local").unwrap().available);
    chadm::metadata::write_new(
        &marker,
        &text.replace("managed_by = \"chadm\"", "managed_by = \"other\""),
    )
    .unwrap();
    assert!(repo::delete(&f.host, "local").is_err());
    assert!(f.metadata().exists());
}

#[test]
fn symlinked_storage_metadata_and_hardlinks_are_rejected() {
    let f = Fixture::new();
    let external = f.root.join("external");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("precious"), "keep").unwrap();
    std::os::unix::fs::symlink(&external, &f.storage).unwrap();
    assert!(repo::create(&f.host, "local", &f.storage).is_err());
    assert!(repo::create(&f.host, "local", &f.storage.join("child")).is_err());
    fs::remove_file(&f.storage).unwrap();
    f.create();
    fs::remove_dir(f.storage.join("volumes")).unwrap();
    std::os::unix::fs::symlink(&external, f.storage.join("volumes")).unwrap();
    assert!(repo::delete(&f.host, "local").is_err());
    assert!(external.join("precious").exists());
    let backup = f.root.join("backup.toml");
    fs::rename(f.metadata(), &backup).unwrap();
    std::os::unix::fs::symlink(&backup, f.metadata()).unwrap();
    assert!(repo::show(&f.host, "local").is_err());
    fs::remove_file(f.metadata()).unwrap();
    fs::hard_link(&backup, f.metadata()).unwrap();
    assert!(repo::show(&f.host, "local").is_err());
}

#[test]
fn untrusted_writable_ancestor_is_rejected() {
    let f = Fixture::new();
    let shared = f.root.join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(repo::create(&f.host, "local", &shared.join("storage")).is_err());
    assert!(!shared.join("storage").exists());
}

#[test]
fn held_lock_rejects_other_process_without_changing_data() {
    let f = Fixture::new();
    f.create();
    let _lock = Lock::acquire(&f.host.data_dir, true).unwrap().unwrap();
    let output = f.cli(&["repo", "delete", "local"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("another chadm command"));
    assert!(f.metadata().exists());
    assert!(f.storage.join(".chadm-repo.toml").exists());
}

#[test]
fn corrupt_metadata_and_incomplete_transactions_are_visible() {
    let f = Fixture::new();
    f.create();
    let original = fs::read_to_string(f.metadata()).unwrap();
    fs::write(
        f.metadata(),
        original.replace("schema_version = 1", "schema_version = 2"),
    )
    .unwrap();
    assert!(repo::list(&f.host).is_err());
    assert!(repo::delete(&f.host, "local").is_err());
    fs::write(f.metadata(), &original).unwrap();
    let stage = f.host.layout().repos.join(".create-unfinished");
    fs::create_dir(&stage).unwrap();
    let error = repo::list(&f.host).unwrap_err().to_string();
    assert!(error.contains("unfinished"), "{error}");
    assert!(repo::create(&f.host, "another", &f.root.join("another")).is_err());
}

#[test]
fn failed_metadata_publish_rolls_back_only_created_storage() {
    let f = Fixture::new();
    let first = f.create();
    let dir = f.host.layout().repos;
    if fs::metadata(&dir).unwrap().uid() == 0 {
        return;
    } // root bypasses DAC write denial
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    let other = f.root.join("other");
    assert!(repo::create(&f.host, "other", &other).is_err());
    assert!(!other.exists());
    assert_eq!(repo::show(&f.host, "local").unwrap().config, first);
    let existing = f.root.join("empty-existing");
    fs::create_dir(&existing).unwrap();
    assert!(repo::create(&f.host, "other", &existing).is_err());
    assert!(existing.is_dir());
    assert_eq!(fs::read_dir(&existing).unwrap().count(), 0);
}

#[test]
fn failed_storage_prepare_removes_transaction_and_preserves_existing_root() {
    let f = Fixture::new();
    fs::DirBuilder::new()
        .mode(0o500)
        .create(&f.storage)
        .unwrap();
    if fs::metadata(&f.storage).unwrap().uid() == 0 {
        fs::set_permissions(&f.storage, fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    assert!(repo::create(&f.host, "local", &f.storage).is_err());
    assert!(f.storage.is_dir());
    assert_eq!(fs::read_dir(f.host.layout().repos).unwrap().count(), 0);
    assert!(!f.metadata().exists());
    fs::set_permissions(&f.storage, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read_dir(&f.storage).unwrap().count(), 0);
    assert!(repo::create(&f.host, "local", &f.storage).is_ok());
}

#[test]
fn doctor_reports_repo_availability() {
    let f = Fixture::new();
    f.create();
    let output = f.cli(&["doctor"]);
    let text = String::from_utf8(output.stdout).unwrap();
    let row = text
        .lines()
        .find(|line| line.starts_with("repos "))
        .unwrap();
    assert!(row.contains("OK"));
    fs::remove_file(f.storage.join(".chadm-repo.toml")).unwrap();
    let output = f.cli(&["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text
        .lines()
        .find(|line| line.starts_with("repos "))
        .unwrap()
        .contains("FAIL"));
}

#[test]
fn operation_and_console_lock_directories_cannot_be_registered_as_storage() {
    let f = Fixture::new();
    for path in [
        f.host.data_dir.join("operations"),
        f.host.data_dir.join("operations/test01"),
        f.host.data_dir.join("console-sessions"),
        f.host.data_dir.join("console-sessions/test01"),
    ] {
        assert!(repo::create(&f.host, "local", &path)
            .unwrap_err()
            .to_string()
            .contains("overlaps"));
    }
    assert!(!f.host.data_dir.exists());
}
