use chadm::config::HostConfig;
use chadm::instance::{self, BootConfig, CreateOptions, InstanceConfig};
use chadm::{image, metadata, repo, volume};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    host: HostConfig,
    config: PathBuf,
    storage: PathBuf,
    source: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "chadm-instance-test-{}",
            metadata::random_id().unwrap()
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
        let source = root.join("source.raw");
        let mut file = fs::File::create(&source).unwrap();
        file.set_len(8 << 20).unwrap();
        file.write_all(b"boot bytes").unwrap();
        Self {
            root,
            host,
            config,
            storage,
            source,
        }
    }
    fn prepare(&self) {
        repo::create(&self.host, "local", &self.storage).unwrap();
        image::create(&self.host, "base", "local", &self.source).unwrap();
        metadata::write_new(&self.host.firmware_path(), "fake firmware").unwrap();
    }
    fn options(&self, name: &str) -> CreateOptions {
        let mut options = CreateOptions::new(name, "base");
        options
            .networks
            .push(instance::parse_network("bridge=virbr0,dhcp").unwrap());
        options
    }
    fn create(&self) -> InstanceConfig {
        instance::create(&self.host, &self.options("test01")).unwrap()
    }
    fn metadata(&self) -> PathBuf {
        self.host.layout().instances.join("test01/config.toml")
    }
    fn raw(&self, name: &str) -> PathBuf {
        self.storage.join("volumes").join(format!("{name}.raw"))
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_chadm"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
    fn assert_no_instance_volumes(&self) {
        assert!(volume::list(&self.host).unwrap().is_empty());
        if self.host.layout().instances.exists() {
            assert_eq!(
                fs::read_dir(self.host.layout().instances).unwrap().count(),
                0
            );
        }
        assert!(!self.host.runtime_dir.exists());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::set_permissions(
            self.storage.join("volumes"),
            fs::Permissions::from_mode(0o700),
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cold_attach_detach_preserves_contents_and_enforces_exclusive_references() {
    let f = Fixture::new();
    f.prepare();
    let original = f.create();
    instance::create(&f.host, &f.options("test02")).unwrap();
    let volume = volume::create(&f.host, "extra", "local", 1 << 20).unwrap();
    let payload = f.raw("extra");
    fs::OpenOptions::new()
        .write(true)
        .open(&payload)
        .unwrap()
        .write_all(b"precious data")
        .unwrap();
    let before = fs::read(&payload).unwrap();
    assert!(f
        .cli(&["volume", "attach", "extra", "--instance", "test01"])
        .status
        .success());
    let attached = instance::show(&f.host, "test01").unwrap().config;
    assert_eq!(attached.disk[0], original.disk[0]);
    assert_eq!(attached.disk[1].volume_id, volume.id);
    assert_eq!(attached.network, original.network);
    assert_eq!(attached.cloud_init, original.cloud_init);
    assert!(volume::delete(&f.host, "extra").is_err());
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    assert!(volume::attach(&f.host, "extra", "test02")
        .unwrap_err()
        .to_string()
        .contains("attached to instance"));
    assert!(volume::detach(&f.host, "extra", "test02").is_err());
    assert!(volume::detach(&f.host, "test01-root", "test01")
        .unwrap_err()
        .to_string()
        .contains("root volume cannot be detached"));
    assert!(f
        .cli(&["volume", "detach", "extra", "--instance", "test01"])
        .status
        .success());
    assert_eq!(
        instance::show(&f.host, "test01").unwrap().config.disk,
        original.disk
    );
    assert_eq!(fs::read(&payload).unwrap(), before);
    assert!(volume::show(&f.host, "extra").unwrap().available);
    volume::attach(&f.host, "extra", "test02").unwrap();
    volume::detach(&f.host, "extra", "test02").unwrap();
    assert!(!f.host.runtime_dir.exists());
}

#[test]
fn cold_edit_rejects_locks_runtime_tampered_payloads_and_interrupted_updates() {
    let f = Fixture::new();
    f.prepare();
    f.create();
    volume::create(&f.host, "extra", "local", 1 << 20).unwrap();
    let before = fs::read(f.metadata()).unwrap();
    let lock = metadata::Lock::acquire(&f.host.data_dir.join("operations/test01"), true).unwrap();
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    drop(lock);
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(f.host.runtime_dir.join("test01"))
        .unwrap();
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    fs::remove_dir(f.host.runtime_dir.join("test01")).unwrap();
    let interrupted = f
        .metadata()
        .parent()
        .unwrap()
        .join(".config-interrupted.toml");
    fs::write(&interrupted, b"unknown evidence").unwrap();
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    assert_eq!(fs::read(&interrupted).unwrap(), b"unknown evidence");
    fs::remove_file(interrupted).unwrap();
    fs::set_permissions(f.raw("extra"), fs::Permissions::from_mode(0o666)).unwrap();
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    fs::set_permissions(f.raw("extra"), fs::Permissions::from_mode(0o600)).unwrap();
    // Publication cannot create its temporary file; old configuration stays intact.
    let dir = f.metadata().parent().unwrap().to_path_buf();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(volume::attach(&f.host, "extra", "test01").is_err());
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(fs::read(f.metadata()).unwrap(), before);
    volume::attach(&f.host, "extra", "test01").unwrap();
    let path = f.host.layout().volumes.join("extra/config.toml");
    let config = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        config.replace(
            &volume::show(&f.host, "extra").unwrap().config.id,
            &"a".repeat(32),
        ),
    )
    .unwrap();
    assert!(volume::detach(&f.host, "extra", "test01")
        .unwrap_err()
        .to_string()
        .contains("generation changed"));
}

#[test]
fn fixed_firmware_ignores_legacy_paths_and_cli_defaults_network_to_dhcp() {
    let mut f = Fixture::new();
    f.prepare();
    f.host.cloud_hypervisor = std::env::current_exe().unwrap();
    f.host.firmware = Some(f.root.join("legacy-host-missing.fd"));
    let mut options = f.options("test01");
    options.boot = BootConfig::Firmware {
        firmware: Some(f.root.join("legacy-instance-missing.fd")),
    };
    let config = instance::create(&f.host, &options).unwrap();
    let (_, args) = chadm::hypervisor::command_line(&f.host, &config).unwrap();
    let pair = args.windows(2).find(|p| p[0] == "--firmware").unwrap();
    assert_eq!(pair[1], f.host.firmware_path().into_os_string());
    assert!(!fs::read_to_string(f.metadata())
        .unwrap()
        .contains("firmware ="));
    assert!(instance::show(&f.host, "test01")
        .unwrap()
        .json()
        .contains(&f.host.firmware_path().to_string_lossy().to_string()));
    let help = f.cli(&["instance", "create", "--help"]);
    assert!(!String::from_utf8(help.stdout)
        .unwrap()
        .contains("--firmware"));
    assert_eq!(
        f.cli(&[
            "instance",
            "create",
            "--name",
            "oldflag",
            "--image",
            "base",
            "--firmware",
            "/tmp/CLOUDHV.fd"
        ])
        .status
        .code(),
        Some(2)
    );
    assert!(f
        .cli(&[
            "instance",
            "create",
            "--name",
            "test02",
            "--image",
            "base",
            "--boot",
            "fireware",
            "--net",
            "bridge=virbr0"
        ])
        .status
        .success());
    assert_eq!(
        instance::show(&f.host, "test02").unwrap().config.network[0].mode,
        instance::NetworkMode::Dhcp
    );
    fs::remove_file(f.host.firmware_path()).unwrap();
    assert!(chadm::hypervisor::command_line(&f.host, &config).is_err());
    std::os::unix::fs::symlink(&f.source, f.host.firmware_path()).unwrap();
    assert!(chadm::hypervisor::command_line(&f.host, &config).is_err());
}

#[test]
fn cli_firmware_creates_independent_root_data_and_persistent_mac() {
    let f = Fixture::new();
    f.prepare();
    let base = f.storage.join("images/base.raw");
    let before = fs::read(&base).unwrap();
    let result = f.cli(&[
        "instance",
        "create",
        "--name",
        "test01",
        "--image",
        "base",
        "--boot",
        "firmware",
        "--cpus",
        "2",
        "--ram",
        "2G",
        "--volume",
        "data,size=16M",
        "--volume",
        "root,size=32M",
        "--net",
        "bridge=virbr0,dhcp",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let config: InstanceConfig =
        toml::from_str(&fs::read_to_string(f.metadata()).unwrap()).unwrap();
    assert_eq!(config.compute.memory, "2G");
    assert_eq!(config.disk[0].name, "root");
    assert_eq!(config.disk[1].name, "data");
    assert_eq!(config.disk[0].volume, "test01-root");
    assert_eq!(config.network[0].tap, "ch-test01-0");
    let mac = &config.network[0].mac;
    assert_eq!(u8::from_str_radix(&mac[..2], 16).unwrap() & 3, 2);
    assert_eq!(fs::metadata(f.metadata()).unwrap().mode() & 0o777, 0o600);
    let root = f.raw("test01-root");
    assert_eq!(fs::metadata(&root).unwrap().len(), 32 << 20);
    assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o600);
    assert_ne!(
        fs::metadata(&root).unwrap().ino(),
        fs::metadata(&base).unwrap().ino()
    );
    let mut raw = fs::File::open(&root).unwrap();
    let mut bytes = vec![0; before.len()];
    raw.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, before);
    raw.seek(SeekFrom::End(-32)).unwrap();
    let mut tail = [1; 32];
    raw.read_exact(&mut tail).unwrap();
    assert_eq!(tail, [0; 32]);
    assert!(fs::read(f.raw("test01-data"))
        .unwrap()
        .iter()
        .all(|b| *b == 0));
    fs::OpenOptions::new()
        .write(true)
        .open(&root)
        .unwrap()
        .write_all(b"guest writes")
        .unwrap();
    assert_eq!(fs::read(base).unwrap(), before);
    let info = instance::show(&f.host, "test01").unwrap();
    assert!(info.available);
    assert_eq!(info.state, "stopped");
    assert_eq!(info.config.network[0].mac, *mac);
    assert_eq!(info.unit, "chadm-instance@test01.service");
    let text = String::from_utf8(f.cli(&["instance", "show", "test01", "--json"]).stdout).unwrap();
    assert!(text.contains("\"state_source\":\"metadata_only\""));
    assert!(text.contains("\"ip_status\":\"not_applicable\""));
    assert!(text.contains("\\\"quoted\\\""));
    assert!(text.contains("\"lifecycle_supported\":true"));
    let text = String::from_utf8(f.cli(&["instance", "show", "test01"]).stdout).unwrap();
    assert!(text.contains("IP: N/A (stopped)"));
    assert!(f.cli(&["instance", "list"]).status.success());
    assert!(!f.host.runtime_dir.exists());
    assert!(volume::delete(&f.host, "test01-root")
        .unwrap_err()
        .to_string()
        .contains("attached to instance"));
    assert!(image::delete(&f.host, "base").is_err());
    volume::create(&f.host, "unrelated", "local", 1024).unwrap();
    volume::delete(&f.host, "unrelated").unwrap();
    assert!(f.cli(&["instance", "delete", "test01"]).status.success());
    assert!(root.exists());
    assert!(f.raw("test01-data").exists());
    assert!(image::delete(&f.host, "base")
        .unwrap_err()
        .to_string()
        .contains("referenced by volume"));
    volume::delete(&f.host, "test01-root").unwrap();
    volume::delete(&f.host, "test01-data").unwrap();
    image::delete(&f.host, "base").unwrap();
    assert_eq!(f.cli(&["instance", "list", "--json"]).stdout, b"[]\n");
}

#[test]
fn default_root_uses_image_size_and_explicit_destination_repo_is_honored() {
    let f = Fixture::new();
    f.prepare();
    repo::create(&f.host, "second", &f.root.join("second")).unwrap();
    let mut options = CreateOptions::new("default", "base");
    instance::create(&f.host, &options).unwrap();
    let first = volume::show(&f.host, "default-root").unwrap();
    assert_eq!(first.config.repo, "local");
    assert_eq!(first.config.size_bytes, 8 << 20);
    options.name = "other".into();
    options.repo = Some("second".into());
    instance::create(&f.host, &options).unwrap();
    let second = volume::show(&f.host, "other-root").unwrap();
    assert_eq!(second.config.repo, "second");
    assert_eq!(
        fs::read(second.path.unwrap()).unwrap(),
        fs::read(&f.source).unwrap()
    );
    assert_eq!(second.config.source_image.as_deref(), Some("base"));
    assert!(instance::list(&f.host)
        .unwrap()
        .iter()
        .all(|info| info.available));
}

#[test]
fn direct_boot_requires_files_and_redacts_cmdline_from_query_output() {
    let f = Fixture::new();
    f.prepare();
    let kernel = f.root.join("kernel");
    let initrd = f.root.join("initrd");
    fs::write(&kernel, b"kernel").unwrap();
    fs::write(&initrd, b"initrd").unwrap();
    let output = f.cli(&[
        "instance",
        "create",
        "--name",
        "direct",
        "--image",
        "base",
        "--boot",
        "direct",
        "--kernel",
        kernel.to_str().unwrap(),
        "--initramfs",
        initrd.to_str().unwrap(),
        "--cmdline",
        "root=/dev/vda1 console=ttyS0 token=secret",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for args in [
        vec!["instance", "show", "direct"],
        vec!["instance", "show", "direct", "--json"],
    ] {
        let text = String::from_utf8(f.cli(&args).stdout).unwrap();
        assert!(!text.contains("token=secret"));
        assert!(text.contains("direct"));
    }
    let config: InstanceConfig = toml::from_str(
        &fs::read_to_string(f.host.layout().instances.join("direct/config.toml")).unwrap(),
    )
    .unwrap();
    match config.boot {
        BootConfig::Direct { cmdline, .. } => assert!(cmdline.contains("token=secret")),
        _ => panic!("direct boot expected"),
    }
    fs::remove_file(kernel).unwrap();
    assert!(!instance::show(&f.host, "direct").unwrap().available);
    instance::delete(&f.host, "direct").unwrap();
    for args in [
        vec![
            "instance", "create", "--name", "bad", "--image", "base", "--boot", "direct",
        ],
        vec![
            "instance", "create", "--name", "bad", "--image", "base", "--cpus", "0",
        ],
        vec![
            "instance", "create", "--name", "bad", "--image", "base", "--ram", "0",
        ],
    ] {
        assert_eq!(f.cli(&args).status.code(), Some(2));
    }
}

#[test]
fn invalid_specs_names_sizes_and_tap_lengths_fail_before_writes() {
    let f = Fixture::new();
    f.prepare();
    for name in ["../test", "a/b", "test;echo", "UPPER", "abcdefghijk"] {
        assert!(instance::create(&f.host, &f.options(name)).is_err());
        f.assert_no_instance_volumes();
    }
    let mut bad = f.options("test01");
    bad.cpus = 0;
    assert!(instance::create(&f.host, &bad).is_err());
    bad = f.options("test01");
    bad.ram = 0;
    assert!(instance::create(&f.host, &bad).is_err());
    bad = f.options("test01");
    bad.volumes
        .push(instance::parse_volume("root,size=1M").unwrap());
    assert!(instance::create(&f.host, &bad).is_err());
    bad = f.options("test01");
    bad.volumes = vec![instance::parse_volume("data,size=1M").unwrap(); 2];
    assert!(instance::create(&f.host, &bad).is_err());
    bad = f.options("test01");
    bad.boot = BootConfig::Direct {
        kernel: f.root.join("missing"),
        initramfs: None,
        cmdline: "root=/dev/vda1".into(),
    };
    assert!(instance::create(&f.host, &bad).is_err());
    f.assert_no_instance_volumes();
}

#[test]
fn duplicates_preexisting_volume_paths_and_mac_conflicts_preserve_resources() {
    let f = Fixture::new();
    f.prepare();
    let mut first = f.options("test01");
    first.networks[0].mac = Some("52:54:AA:BB:CC:DD".into());
    let config = instance::create(&f.host, &first).unwrap();
    assert_eq!(config.network[0].mac, "52:54:aa:bb:cc:dd");
    assert!(instance::create(&f.host, &first)
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    let mut second = f.options("test02");
    second.networks[0].mac = Some("52:54:aa:bb:cc:dd".into());
    assert!(instance::create(&f.host, &second)
        .unwrap_err()
        .to_string()
        .contains("MAC"));
    second.networks[0].mac = None;
    let config2 = instance::create(&f.host, &second).unwrap();
    assert_ne!(config2.network[0].mac, config.network[0].mac);
    let foreign = f.raw("blocked-data");
    fs::write(&foreign, "precious").unwrap();
    let mut blocked = f.options("blocked");
    blocked
        .volumes
        .push(instance::parse_volume("data,size=1M").unwrap());
    assert!(instance::create(&f.host, &blocked).is_err());
    assert_eq!(fs::read_to_string(foreign).unwrap(), "precious");
    assert!(!f.raw("blocked-root").exists());
}

#[test]
fn runtime_artifacts_produce_unknown_state_and_block_deletion() {
    let f = Fixture::new();
    f.prepare();
    f.create();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&f.host.runtime_dir)
        .unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(f.host.runtime_dir.join("test01"))
        .unwrap();
    let info = instance::show(&f.host, "test01").unwrap();
    assert_eq!(info.state, "unknown");
    assert!(!info.available);
    assert!(info.json().contains("\"ip_status\":\"unknown\""));
    assert!(instance::delete(&f.host, "test01")
        .unwrap_err()
        .to_string()
        .contains("unknown"));
    assert!(f.metadata().exists());
    assert_eq!(
        f.cli(&["instance", "start", "test01"]).status.code(),
        Some(1)
    );
    assert!(instance::create(&f.host, &f.options("test01")).is_err());
}

#[test]
fn missing_reads_and_creation_do_not_initialize_a_host() {
    let f = Fixture::new();
    assert_eq!(f.cli(&["instance", "list", "--json"]).stdout, b"[]\n");
    assert_eq!(
        f.cli(&["instance", "show", "test01"]).status.code(),
        Some(1)
    );
    assert_eq!(
        f.cli(&["instance", "delete", "test01"]).status.code(),
        Some(1)
    );
    assert!(instance::create(&f.host, &f.options("test01")).is_err());
    assert!(!f.host.data_dir.exists());
    assert!(!f.host.runtime_dir.exists());
}

#[test]
fn failed_storage_creation_rolls_back_instance_transaction() {
    let f = Fixture::new();
    f.prepare();
    let parent = f.storage.join("volumes");
    if fs::metadata(&parent).unwrap().uid() == 0 {
        return;
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    let error = instance::create(&f.host, &f.options("test01"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("raw volume"));
    f.assert_no_instance_volumes();
    assert!(image::show(&f.host, "base").unwrap().available);
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
    f.create();
}

#[test]
fn metadata_symlinks_hardlinks_and_unknown_entries_are_not_deleted() {
    let f = Fixture::new();
    f.prepare();
    f.create();
    let saved = f.root.join("saved.toml");
    fs::rename(f.metadata(), &saved).unwrap();
    std::os::unix::fs::symlink(&saved, f.metadata()).unwrap();
    assert!(instance::delete(&f.host, "test01").is_err());
    assert!(saved.exists());
    fs::remove_file(f.metadata()).unwrap();
    fs::hard_link(&saved, f.metadata()).unwrap();
    assert!(instance::delete(&f.host, "test01").is_err());
    fs::remove_file(f.metadata()).unwrap();
    fs::rename(saved, f.metadata()).unwrap();
    let extra = f.metadata().parent().unwrap().join("keep");
    fs::write(&extra, "precious").unwrap();
    assert!(instance::delete(&f.host, "test01").is_err());
    assert_eq!(fs::read_to_string(extra).unwrap(), "precious");
    assert!(f.raw("test01-root").exists());
}

#[test]
fn corrupt_and_unfinished_instance_metadata_block_reference_deletion() {
    let f = Fixture::new();
    f.prepare();
    f.create();
    let original = fs::read_to_string(f.metadata()).unwrap();
    fs::write(f.metadata(), format!("{original}\n[typo]\nvalue = true\n")).unwrap();
    assert!(instance::list(&f.host).is_err());
    assert!(volume::delete(&f.host, "test01-root").is_err());
    assert!(image::delete(&f.host, "base").is_err());
    fs::write(f.metadata(), original).unwrap();
    let stage = f.host.layout().instances.join(".create-incomplete");
    fs::DirBuilder::new().mode(0o700).create(&stage).unwrap();
    assert!(instance::list(&f.host).is_err());
    assert!(instance::delete(&f.host, "test01").is_err());
    assert!(volume::delete(&f.host, "test01-root").is_err());
    assert!(volume::create(&f.host, "new", "local", 1024).is_err());
    assert!(instance::create(&f.host, &f.options("other")).is_err());
}

#[test]
fn shared_writable_volumes_and_changed_generations_are_detected() {
    let f = Fixture::new();
    f.prepare();
    let first = f.create();
    let mut second = instance::create(&f.host, &f.options("test02")).unwrap();
    let original = second.clone();
    second.disk = first.disk.clone();
    let path = f.host.layout().instances.join("test02/config.toml");
    fs::write(&path, toml::to_string(&second).unwrap()).unwrap();
    assert!(instance::list(&f.host)
        .err()
        .unwrap()
        .to_string()
        .contains("multiple instances"));
    fs::write(&path, toml::to_string(&original).unwrap()).unwrap();
    let mut changed = first.clone();
    changed.disk[0].volume_id = "0".repeat(32);
    fs::write(f.metadata(), toml::to_string(&changed).unwrap()).unwrap();
    let info = instance::show(&f.host, "test01").unwrap();
    assert!(!info.available);
    assert!(info.reason.unwrap().contains("generation"));
    assert!(volume::delete(&f.host, "test01-root").is_err());
    fs::write(f.metadata(), toml::to_string(&first).unwrap()).unwrap();
    fs::remove_file(f.raw("test01-root")).unwrap();
    assert!(!instance::show(&f.host, "test01").unwrap().available);
    instance::delete(&f.host, "test01").unwrap(); // metadata can be removed despite missing storage
}

#[test]
fn lock_conflicts_and_doctor_report_metadata_storage_availability() {
    let f = Fixture::new();
    f.prepare();
    f.create();
    let lock = metadata::Lock::acquire(&f.host.data_dir, true)
        .unwrap()
        .unwrap();
    let output = f.cli(&["instance", "delete", "test01"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("another chadm command"));
    drop(lock);
    let text = String::from_utf8(f.cli(&["doctor"]).stdout).unwrap();
    assert!(text
        .lines()
        .find(|line| line.starts_with("instances "))
        .unwrap()
        .contains("OK"));
    fs::remove_file(f.raw("test01-root")).unwrap();
    let output = f.cli(&["doctor"]);
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(text
        .lines()
        .find(|line| line.starts_with("instances "))
        .unwrap()
        .contains("FAIL"));
}

#[test]
fn cloudinit_default_readonly_reference_and_delete_protection() {
    let mut fixture = Fixture::new();
    fixture.host.cloud_hypervisor = std::env::current_exe().unwrap();
    fixture.prepare();
    let seed_repo = repo::create(&fixture.host, "cloudinit", &fixture.root.join("seeds")).unwrap();
    assert_eq!(seed_repo.kind, repo::RepoKind::Cloudinit);
    let input = fixture.root.join("default.raw");
    fs::write(&input, vec![0u8; 8192]).unwrap();
    let seed = image::create(&fixture.host, "default", "cloudinit", &input).unwrap();
    let mut options = fixture.options("test01");
    options.boot = BootConfig::Firmware {
        firmware: Some(fixture.source.clone()),
    };
    let instance = instance::create(&fixture.host, &options).unwrap();
    let reference = instance.cloud_init.as_ref().unwrap();
    assert_eq!(reference.image, "default");
    assert_eq!(reference.image_id, seed.id);
    let show = instance::show(&fixture.host, "test01").unwrap();
    assert!(show.available);
    assert!(show.json().contains("\"readonly\":true"));
    let (_, argv) = chadm::hypervisor::command_line(&fixture.host, &instance).unwrap();
    let disks: Vec<_> = argv
        .windows(2)
        .filter(|pair| pair[0] == "--disk")
        .map(|pair| pair[1].to_string_lossy().into_owned())
        .collect();
    assert_eq!(disks.len(), 2);
    assert!(disks[0].contains("readonly=off"));
    assert!(disks[1].contains("images/default.raw,readonly=on,image_type=raw"));
    assert_eq!(
        fs::metadata(fixture.root.join("seeds/images/default.raw"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    assert!(image::delete(&fixture.host, "default")
        .unwrap_err()
        .to_string()
        .contains("referenced by instance"));
    let mut changed = instance.clone();
    changed.cloud_init.as_mut().unwrap().image_id = "f".repeat(32);
    assert!(chadm::hypervisor::command_line(&fixture.host, &changed).is_err());
    instance::delete(&fixture.host, "test01").unwrap();
    image::delete(&fixture.host, "default").unwrap();
}

#[test]
fn cloudinit_missing_default_fails_before_allocating_volumes_and_can_be_skipped() {
    let fixture = Fixture::new();
    fixture.prepare();
    repo::create(&fixture.host, "cloudinit", &fixture.root.join("seeds")).unwrap();
    assert!(fixture.create_err().contains("default"));
    fixture.assert_no_instance_volumes();
    let mut options = fixture.options("test01");
    options.no_cloud_init = true;
    let instance = instance::create(&fixture.host, &options).unwrap();
    assert!(instance.cloud_init.is_none());
}

#[test]
fn cloudinit_explicit_selection_rejects_foreign_images_and_reserved_volume_use() {
    let fixture = Fixture::new();
    fixture.prepare();
    repo::create(&fixture.host, "cloudinit", &fixture.root.join("seeds")).unwrap();
    let mut options = fixture.options("test01");
    options.cloud_init = Some("base".into());
    assert!(instance::create(&fixture.host, &options).is_err());
    fixture.assert_no_instance_volumes();
    let input = fixture.root.join("custom.raw");
    fs::write(&input, vec![0u8; 8192]).unwrap();
    image::create(&fixture.host, "custom", "cloudinit", &input).unwrap();
    options.cloud_init = Some("custom".into());
    assert_eq!(
        instance::create(&fixture.host, &options)
            .unwrap()
            .cloud_init
            .unwrap()
            .image,
        "custom"
    );
    assert!(volume::create(&fixture.host, "bad", "cloudinit", 8192).is_err());
    let mut invalid = CreateOptions::new("test02", "custom");
    invalid.no_cloud_init = true;
    assert!(instance::create(&fixture.host, &invalid).is_err());
}

#[test]
fn cloudinit_rejects_iso_and_qcow2_even_when_renamed_raw_and_rolls_back() {
    let fixture = Fixture::new();
    fixture.prepare();
    repo::create(&fixture.host, "cloudinit", &fixture.root.join("seeds")).unwrap();
    let iso = fixture.root.join("seed.iso");
    fs::write(&iso, vec![0u8; 65536]).unwrap();
    assert!(image::create(&fixture.host, "default", "cloudinit", &iso).is_err());
    let raw = fixture.root.join("seed.raw");
    let mut bytes = vec![0u8; 65536];
    bytes[32769..32774].copy_from_slice(b"CD001");
    fs::write(&raw, bytes).unwrap();
    assert!(image::create(&fixture.host, "default", "cloudinit", &raw)
        .unwrap_err()
        .to_string()
        .contains("ISO"));
    let mut qcow = vec![0u8; 512];
    qcow[..4].copy_from_slice(b"QFI\xfb");
    qcow[4..8].copy_from_slice(&3u32.to_be_bytes());
    qcow[20..24].copy_from_slice(&16u32.to_be_bytes());
    qcow[24..32].copy_from_slice(&8192u64.to_be_bytes());
    qcow[100..104].copy_from_slice(&104u32.to_be_bytes());
    fs::write(&raw, qcow).unwrap();
    assert!(image::create(&fixture.host, "default", "cloudinit", &raw).is_err());
    assert!(!fixture.host.layout().images.join("default").exists());
    assert_eq!(
        fs::read_dir(fixture.root.join("seeds/images"))
            .unwrap()
            .count(),
        0
    );
    fs::write(&raw, vec![0u8; 8192]).unwrap();
    image::create(&fixture.host, "default", "cloudinit", &raw).unwrap();
}

impl Fixture {
    fn create_err(&self) -> String {
        instance::create(&self.host, &self.options("test01"))
            .unwrap_err()
            .to_string()
    }
}

#[test]
fn cloudinit_cli_kind_requires_reserved_name_and_conflicting_flags_are_rejected() {
    let fixture = Fixture::new();
    let output = fixture.cli(&[
        "repo",
        "create",
        "--name",
        "wrong",
        "--kind",
        "cloudinit",
        "--driver",
        "file",
        "--path",
        fixture.root.join("seeds").to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(!fixture.host.data_dir.exists());
    let output = fixture.cli(&[
        "instance",
        "create",
        "--name",
        "test01",
        "--image",
        "base",
        "--cloud-init",
        "default",
        "--no-cloud-init",
    ]);
    assert!(!output.status.success());
}

#[test]
fn inspect_resolves_paths_and_redacts_cmdline_and_seed_contents() {
    let mut fixture = Fixture::new();
    fixture.prepare();
    fixture.host.cloud_hypervisor = std::env::current_exe().unwrap();
    fs::write(&fixture.config, toml::to_string(&fixture.host).unwrap()).unwrap();
    repo::create(&fixture.host, "cloudinit", &fixture.root.join("seeds")).unwrap();
    let input = fixture.root.join("seed.raw");
    fs::write(&input, b"SEED_PASSWORD_DO_NOT_PRINT").unwrap();
    image::create(&fixture.host, "default", "cloudinit", &input).unwrap();
    let mut options = fixture.options("test01");
    options.boot = BootConfig::Direct {
        kernel: fixture.source.clone(),
        initramfs: None,
        cmdline: "root=/dev/vda1 secret=CMDLINE_PASSWORD_DO_NOT_PRINT".into(),
    };
    instance::create(&fixture.host, &options).unwrap();
    let report = chadm::inspect::inspect(&fixture.host, "test01").unwrap();
    assert!(report.command_error.is_none());
    let before = fs::read(fixture.metadata()).unwrap();
    for output in [report.text(), report.json()] {
        assert!(!output.contains("CMDLINE_PASSWORD_DO_NOT_PRINT"));
        assert!(!output.contains("SEED_PASSWORD_DO_NOT_PRINT"));
        assert!(output.contains("<redacted>"));
        assert!(output.contains("chadm-instance@test01.service"));
        assert!(output.contains("ch-test01-0"));
        assert!(output.contains("readonly=on,image_type=raw"));
    }
    for args in [
        vec!["inspect", "test01"],
        vec!["inspect", "test01", "--json"],
    ] {
        let result = fixture.cli(&args);
        assert!(result.status.success());
        let stdout = String::from_utf8(result.stdout).unwrap();
        assert!(!stdout.contains("CMDLINE_PASSWORD_DO_NOT_PRINT"));
        assert!(!stdout.contains("SEED_PASSWORD_DO_NOT_PRINT"));
        assert!(stdout.contains("<redacted>"));
    }
    assert_eq!(before, fs::read(fixture.metadata()).unwrap());
    assert!(!fixture.host.runtime_dir.exists());
    assert!(!fixture.host.data_dir.join("console-sessions").exists());
}

#[test]
fn inspect_reports_missing_storage_without_mutating_it_and_console_refuses_stopped_vm() {
    let fixture = Fixture::new();
    fixture.prepare();
    fixture.create();
    fs::remove_file(fixture.raw("test01-root")).unwrap();
    let report = chadm::inspect::inspect(&fixture.host, "test01").unwrap();
    assert!(!report.instance.available);
    assert!(report.command_error.is_some());
    assert!(report.text().contains("unavailable"));
    let output = fixture.cli(&["instance", "console", "test01"]);
    assert!(!output.status.success());
    assert!(!fixture.host.runtime_dir.exists());
    assert!(!fixture.host.data_dir.join("console-sessions").exists());
}

#[test]
fn inspect_and_console_reject_invalid_or_missing_names_without_creating_state() {
    let fixture = Fixture::new();
    for name in ["missing", "../../etc", "a;reboot"] {
        assert!(chadm::inspect::inspect(&fixture.host, name).is_err());
        assert!(!fixture.cli(&["inspect", name, "--json"]).status.success());
        assert!(!fixture.cli(&["instance", "console", name]).status.success());
    }
    assert!(!fixture.host.data_dir.exists());
    assert!(!fixture.host.runtime_dir.exists());
}

#[test]
fn inspect_parser_errors_never_echo_sensitive_configuration_lines() {
    let fixture = Fixture::new();
    fixture.prepare();
    fixture.create();
    let mut text = fs::read_to_string(fixture.metadata()).unwrap();
    text.push_str("\ninvalid = \"CONFIG_ERROR_SECRET_DO_NOT_PRINT\n");
    fs::write(fixture.metadata(), text).unwrap();
    let result = fixture.cli(&["inspect", "test01"]);
    assert!(!result.status.success());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(stderr.contains("contents omitted"));
    assert!(!stderr.contains("CONFIG_ERROR_SECRET_DO_NOT_PRINT"));
    fs::write(
        &fixture.config,
        "firmware = \"HOST_CONFIG_SECRET_DO_NOT_PRINT\n",
    )
    .unwrap();
    let result = fixture.cli(&["inspect", "test01", "--json"]);
    assert!(!result.status.success());
    assert!(!String::from_utf8(result.stderr)
        .unwrap()
        .contains("HOST_CONFIG_SECRET_DO_NOT_PRINT"));
}

#[test]
fn windows_image_profile_is_inherited_persisted_and_applied_to_hypervisor_argv() {
    let fixture = Fixture::new();
    fixture.prepare();
    image::create_with_windows(&fixture.host, "windows", "local", &fixture.source, true).unwrap();
    let mut options = CreateOptions::new("test01", "windows");
    options.no_cloud_init = true;
    options.cpus = 4;
    options.ram = 4 << 30;
    let config = instance::create(&fixture.host, &options).unwrap();
    assert!(config.windows);
    assert_eq!(config.compute.cpus, 4);
    let persisted: InstanceConfig =
        toml::from_str(&fs::read_to_string(fixture.metadata()).unwrap()).unwrap();
    assert_eq!(persisted, config);
    let (_, argv) = chadm::hypervisor::command_line(&fixture.host, &persisted).unwrap();
    let args: Vec<_> = argv.iter().map(|arg| arg.to_str().unwrap()).collect();
    assert!(args
        .windows(2)
        .any(|args| args == ["--cpus", "boot=4,max=4,kvm_hyperv=on,max_phys_bits=39"]));
    assert!(args
        .windows(2)
        .any(|args| args == ["--memory", "size=4294967296"]));
    assert!(args
        .windows(2)
        .any(|args| args == ["--firmware", fixture.host.firmware_path().to_str().unwrap()]));
    assert!(!args.contains(&"--kernel"));
    let output = fixture.cli(&["instance", "show", "test01"]);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("kvm_hyperv: on"));
    let output = fixture.cli(&["inspect", "test01", "--json"]);
    assert!(output.status.success());
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.contains("\"windows\":true"));
    assert!(json.contains("kvm_hyperv=on,max_phys_bits=39"));
    volume::create(&fixture.host, "extra", "local", 1 << 20).unwrap();
    volume::attach(&fixture.host, "extra", "test01").unwrap();
    assert!(
        instance::show(&fixture.host, "test01")
            .unwrap()
            .config
            .windows
    );

    // The profile is copied at creation, and subsequent argv uses the instance's copy.
    let mut generic = config;
    generic.windows = false;
    let (_, args) = chadm::hypervisor::command_line(&fixture.host, &generic).unwrap();
    assert!(!args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("kvm_hyperv=")));
}

#[test]
fn windows_direct_boot_fails_before_storage_and_legacy_instances_remain_generic() {
    let fixture = Fixture::new();
    fixture.prepare();
    image::create_with_windows(&fixture.host, "windows", "local", &fixture.source, true).unwrap();
    let mut options = CreateOptions::new("badboot", "windows");
    options.boot = BootConfig::Direct {
        kernel: fixture.root.join("kernel"),
        initramfs: None,
        cmdline: "root=/dev/vda1".into(),
    };
    let error = instance::create(&fixture.host, &options).unwrap_err();
    assert!(error
        .to_string()
        .contains("Windows images require --boot firmware"));
    assert!(volume::list(&fixture.host).unwrap().is_empty());
    assert!(!fixture.host.layout().instances.join("badboot").exists());
    let config = fixture.create();
    let mut invalid = config.clone();
    invalid.windows = true;
    invalid.boot = options.boot;
    assert!(chadm::hypervisor::command_line(&fixture.host, &invalid).is_err());
    let legacy = fs::read_to_string(fixture.metadata())
        .unwrap()
        .replace("windows = false\n", "");
    fs::write(fixture.metadata(), legacy).unwrap();
    let info = instance::show(&fixture.host, "test01").unwrap();
    assert!(info.available);
    assert!(!info.config.windows);
}

#[test]
fn default_bridge_is_checked_before_storage_and_explicit_network_overrides_it() {
    let f = Fixture::new();
    f.prepare();
    let mut host = f.host.clone();
    host.default_bridge = Some("ch-missing-br".into());
    let mut options = CreateOptions::new("defaultnet", "base");
    let error = instance::create(&host, &options).unwrap_err().to_string();
    assert!(error.contains("does not exist"), "{error}");
    assert!(!host.layout().instances.join("defaultnet").exists());
    assert!(volume::list(&host).unwrap().is_empty());
    options
        .networks
        .push(instance::parse_network("dhcp").unwrap());
    assert!(instance::create(&host, &options)
        .unwrap_err()
        .to_string()
        .contains("does not exist"));
    host.default_bridge = Some("lo".into());
    assert!(instance::create(&host, &options)
        .unwrap_err()
        .to_string()
        .contains("not a Linux bridge"));
    host.default_bridge = None;
    assert!(instance::create(&host, &options)
        .unwrap_err()
        .to_string()
        .contains("default_bridge"));
    // Explicit networks retain metadata-only creation and override the default.
    host.default_bridge = Some("ch-missing-br".into());
    options.networks = vec![instance::parse_network("bridge=explicitbr").unwrap()];
    let created = instance::create(&host, &options).unwrap();
    assert_eq!(created.network[0].bridge, "explicitbr");
}
