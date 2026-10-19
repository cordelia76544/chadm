use chadm::config::HostConfig;
use chadm::image::{self, ImageConfig, ImageFormat};
use chadm::{metadata, repo};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
    host: HostConfig,
    config: PathBuf,
    storage: PathBuf,
    input: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "chadm-image-test-{}",
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
        let input = root.join("input ';$(touch injected).qcow2"); // raw despite its suffix
        let mut file = fs::File::create(&input).unwrap();
        file.set_len(8 << 20).unwrap();
        file.write_all(b"boot bytes").unwrap();
        file.seek(SeekFrom::End(-7)).unwrap();
        file.write_all(b"the end").unwrap();
        Self {
            root,
            host,
            config,
            storage,
            input,
        }
    }
    fn repo(&self) {
        repo::create(&self.host, "local", &self.storage).unwrap();
    }
    fn create(&self) -> ImageConfig {
        image::create(&self.host, "base", "local", &self.input).unwrap()
    }
    fn raw(&self) -> PathBuf {
        self.storage.join("images/base.raw")
    }
    fn marker(&self) -> PathBuf {
        self.storage.join("images/base.chadm.toml")
    }
    fn metadata(&self) -> PathBuf {
        self.host.layout().images.join("base/config.toml")
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_chadm"))
            .arg("--config")
            .arg(&self.config)
            .args(args)
            .output()
            .unwrap()
    }
    fn assert_empty(&self) {
        assert_eq!(fs::read_dir(self.host.layout().images).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(self.storage.join("images")).unwrap().count(),
            0
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::set_permissions(
            self.storage.join("images"),
            fs::Permissions::from_mode(0o700),
        );
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn qemu(args: &[&str], paths: &[&Path]) {
    let program = chadm::command::executable(Path::new("qemu-img"))
        .expect("qcow2 integration tests require qemu-img");
    let output = Command::new(program)
        .args(args)
        .args(paths)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cli_raw_roundtrip_is_readonly_independent_and_json_is_available() {
    let f = Fixture::new();
    f.repo();
    let output = Command::new(env!("CARGO_BIN_EXE_chadm"))
        .arg("--config")
        .arg(&f.config)
        .args([
            "image", "create", "--name", "base", "--repo", "local", "--path",
        ])
        .arg(&f.input)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: ImageConfig = toml::from_str(&fs::read_to_string(f.metadata()).unwrap()).unwrap();
    let marker: ImageConfig = toml::from_str(&fs::read_to_string(f.marker()).unwrap()).unwrap();
    assert_eq!(config, marker);
    assert_eq!(config.format, ImageFormat::Raw);
    assert_eq!(config.source_format, ImageFormat::Raw);
    assert_eq!(config.virtual_size_bytes, 8 << 20);
    let stat = fs::metadata(f.raw()).unwrap();
    assert_eq!(stat.mode() & 0o777, 0o400);
    assert_eq!(stat.ino(), config.inode);
    assert_ne!(stat.ino(), fs::metadata(&f.input).unwrap().ino());
    assert!(stat.blocks() * 512 < stat.len());
    assert_eq!(fs::read(&f.input).unwrap(), fs::read(f.raw()).unwrap());
    fs::write(&f.input, b"changed source").unwrap();
    assert_eq!(fs::metadata(f.raw()).unwrap().len(), 8 << 20);
    assert!(fs::read(f.raw()).unwrap().starts_with(b"boot bytes"));
    assert_eq!(
        fs::read_dir(f.metadata().parent().unwrap())
            .unwrap()
            .count(),
        1
    );
    let output = f.cli(&["image", "show", "base", "--json"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("\"available\":true"));
    assert!(text.contains("\"format\":\"raw\""));
    assert!(text.contains("\\\"quoted\\\""));
    assert!(text.contains("\"source_format\":\"raw\""));
    assert!(f.cli(&["image", "list"]).status.success());
    assert!(f.cli(&["image", "show", "base"]).status.success());
    assert!(repo::delete(&f.host, "local")
        .unwrap_err()
        .to_string()
        .contains("referenced by images"));
    assert!(f.cli(&["image", "delete", "base"]).status.success());
    f.assert_empty();
    assert_eq!(fs::read(&f.input).unwrap(), b"changed source");
    assert_eq!(f.cli(&["image", "list", "--json"]).stdout, b"[]\n");
    repo::delete(&f.host, "local").unwrap();
    assert!(f.storage.is_dir());
}

#[test]
fn qcow2_v2_v3_and_compressed_imports_match_raw_contents() {
    let f = Fixture::new();
    f.repo();
    for (name, opts) in [
        ("version-two", "compat=0.10"),
        ("version-three", "compat=1.1"),
    ] {
        let source = f.root.join(format!("{name}.raw")); // qcow2 despite suffix
        qemu(
            &["convert", "-f", "raw", "-O", "qcow2", "-o", opts],
            &[&f.input, &source],
        );
        let image = image::create(&f.host, name, "local", &source).unwrap();
        assert_eq!(image.source_format, ImageFormat::Qcow2);
        assert_eq!(image.virtual_size_bytes, 8 << 20);
        let info = image::show(&f.host, name).unwrap();
        assert!(info.available);
        assert_eq!(
            fs::read(info.path.unwrap()).unwrap(),
            fs::read(&f.input).unwrap()
        );
        image::delete(&f.host, name).unwrap();
    }
    let source = f.root.join("compressed");
    qemu(
        &["convert", "-c", "-f", "raw", "-O", "qcow2"],
        &[&f.input, &source],
    );
    image::create(&f.host, "compressed", "local", &source).unwrap();
    assert_eq!(
        fs::read(image::show(&f.host, "compressed").unwrap().path.unwrap()).unwrap(),
        fs::read(&f.input).unwrap()
    );
}

#[test]
fn backing_chain_and_external_data_are_rejected_and_rolled_back() {
    let f = Fixture::new();
    f.repo();
    let overlay = f.root.join("overlay");
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-F",
            "raw",
            "-b",
            f.input.to_str().unwrap(),
        ],
        &[&overlay],
    );
    let error = image::create(&f.host, "base", "local", &overlay)
        .unwrap_err()
        .to_string();
    assert!(error.contains("backing files"), "{error}");
    f.assert_empty();
    let external = f.root.join("external");
    let data = f.root.join("external-data");
    qemu(
        &[
            "create",
            "-f",
            "qcow2",
            "-o",
            &format!("data_file={}", data.display()),
            external.to_str().unwrap(),
            "8M",
        ],
        &[],
    );
    let error = image::create(&f.host, "base", "local", &external)
        .unwrap_err()
        .to_string();
    assert!(error.contains("external data"), "{error}");
    f.assert_empty();
    assert!(overlay.exists());
    assert!(external.exists());
    assert!(data.exists());
    assert!(fs::read(&f.input).unwrap().starts_with(b"boot bytes"));
}

#[test]
fn conversion_failure_keeps_source_and_removes_partial_target() {
    let f = Fixture::new();
    f.repo();
    let source = f.root.join("broken");
    qemu(
        &["create", "-f", "qcow2", source.to_str().unwrap(), "8M"],
        &[],
    );
    let file = fs::OpenOptions::new().write(true).open(&source).unwrap();
    file.write_all_at(&1u64.to_be_bytes(), 48).unwrap(); // unaligned refcount table offset
    let before = fs::read(&source).unwrap();
    let error = image::create(&f.host, "base", "local", &source)
        .unwrap_err()
        .to_string();
    assert!(error.contains("conversion failed"), "{error}");
    f.assert_empty();
    assert_eq!(fs::read(source).unwrap(), before);
}

#[test]
fn missing_queries_and_invalid_inputs_do_not_publish_resources() {
    let f = Fixture::new();
    assert_eq!(f.cli(&["image", "list", "--json"]).stdout, b"[]\n");
    assert_eq!(f.cli(&["image", "show", "base"]).status.code(), Some(1));
    assert_eq!(f.cli(&["image", "delete", "base"]).status.code(), Some(1));
    for name in ["../base", "a/b", "a;echo", "UPPER"] {
        assert!(image::create(&f.host, name, "local", &f.input).is_err());
    }
    assert!(image::create(&f.host, "base", "missing", &f.input).is_err());
    assert!(!f.host.data_dir.exists());
    f.repo();
    let link = f.root.join("link");
    std::os::unix::fs::symlink(&f.input, &link).unwrap();
    let empty = f.root.join("empty");
    fs::write(&empty, []).unwrap();
    for path in [
        &link,
        &empty,
        &f.root,
        &PathBuf::from("/dev/null"),
        &f.root.join("missing"),
    ] {
        assert!(image::create(&f.host, "base", "local", path).is_err());
        f.assert_empty();
    }
    assert_eq!(
        f.cli(&[
            "image",
            "create",
            "--name",
            "base",
            "--repo",
            "local",
            "--url",
            "https://example.invalid/image"
        ])
        .status
        .code(),
        Some(2)
    );
}

#[test]
fn names_are_global_and_orphan_storage_is_never_overwritten() {
    let f = Fixture::new();
    f.repo();
    f.create();
    repo::create(&f.host, "second", &f.root.join("second")).unwrap();
    assert!(image::create(&f.host, "base", "second", &f.input)
        .unwrap_err()
        .to_string()
        .contains("already exists"));
    image::create(&f.host, "alpha", "second", &f.input).unwrap();
    let images = image::list(&f.host).unwrap();
    assert_eq!(images[0].config.name, "alpha");
    assert_eq!(images[1].config.name, "base");
    image::delete(&f.host, "base").unwrap();
    for path in [f.raw(), f.marker()] {
        fs::write(&path, "foreign").unwrap();
        assert!(image::create(&f.host, "base", "local", &f.input).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "foreign");
        assert!(!f.metadata().exists());
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn writable_replaced_or_resized_base_blocks_deletion() {
    let f = Fixture::new();
    f.repo();
    let config = f.create();
    fs::set_permissions(f.raw(), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!image::show(&f.host, "base").unwrap().available);
    assert!(image::delete(&f.host, "base").is_err());
    fs::set_permissions(f.raw(), fs::Permissions::from_mode(0o400)).unwrap();
    let saved = f.storage.join("saved.raw");
    fs::rename(f.raw(), &saved).unwrap();
    fs::write(f.raw(), "foreign").unwrap();
    assert!(image::delete(&f.host, "base").is_err());
    assert_eq!(fs::read_to_string(f.raw()).unwrap(), "foreign");
    fs::remove_file(f.raw()).unwrap();
    fs::rename(saved, f.raw()).unwrap();
    fs::set_permissions(f.raw(), fs::Permissions::from_mode(0o600)).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(f.raw())
        .unwrap()
        .set_len(1024)
        .unwrap();
    fs::set_permissions(f.raw(), fs::Permissions::from_mode(0o400)).unwrap();
    assert!(image::delete(&f.host, "base").is_err());
    assert_eq!(image::show(&f.host, "base").unwrap().config, config);
}

#[test]
fn xattr_marker_and_repo_generation_are_all_required() {
    let f = Fixture::new();
    f.repo();
    let mut config = f.create();
    let bytes = fs::read(f.raw()).unwrap();
    fs::rename(f.raw(), f.storage.join("saved.raw")).unwrap();
    fs::write(f.raw(), bytes).unwrap();
    fs::set_permissions(f.raw(), fs::Permissions::from_mode(0o400)).unwrap();
    config.inode = fs::metadata(f.raw()).unwrap().ino();
    let text = toml::to_string(&config).unwrap();
    fs::write(f.metadata(), &text).unwrap();
    fs::write(f.marker(), &text).unwrap();
    assert!(image::show(&f.host, "base")
        .unwrap()
        .reason
        .unwrap()
        .contains("ownership attribute"));
    assert!(image::delete(&f.host, "base").is_err());
    fs::remove_file(f.raw()).unwrap();
    fs::rename(f.storage.join("saved.raw"), f.raw()).unwrap();
    config.inode = fs::metadata(f.raw()).unwrap().ino();
    config.repo_id = "0".repeat(32);
    fs::write(f.metadata(), toml::to_string(&config).unwrap()).unwrap();
    assert!(image::show(&f.host, "base")
        .unwrap()
        .reason
        .unwrap()
        .contains("different generation"));
    assert!(image::delete(&f.host, "base").is_err());
    assert!(f.raw().exists());
}

#[test]
fn links_unknown_metadata_transactions_and_instance_references_fail_closed() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let saved = f.storage.join("saved.raw");
    fs::rename(f.raw(), &saved).unwrap();
    std::os::unix::fs::symlink(&saved, f.raw()).unwrap();
    assert!(image::delete(&f.host, "base").is_err());
    fs::remove_file(f.raw()).unwrap();
    fs::hard_link(&saved, f.raw()).unwrap();
    assert!(image::delete(&f.host, "base").is_err());
    fs::remove_file(f.raw()).unwrap();
    fs::rename(saved, f.raw()).unwrap();
    let extra = f.metadata().parent().unwrap().join("keep");
    fs::write(&extra, "keep").unwrap();
    assert!(image::delete(&f.host, "base").is_err());
    fs::remove_file(extra).unwrap();
    let stage = f.host.layout().images.join(".create-incomplete");
    fs::DirBuilder::new().mode(0o700).create(&stage).unwrap();
    assert!(image::list(&f.host).is_err());
    assert!(image::delete(&f.host, "base").is_err());
    assert!(image::create(&f.host, "other", "local", &f.input).is_err());
    assert!(repo::delete(&f.host, "local").is_err());
    fs::remove_dir(stage).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(f.host.layout().instances)
        .unwrap();
    fs::write(f.host.layout().instances.join("unknown"), "unsupported").unwrap();
    assert!(image::delete(&f.host, "base")
        .unwrap_err()
        .to_string()
        .contains("references"));
    assert!(f.raw().exists());
}

#[test]
fn missing_payload_still_blocks_repo_delete_and_lock_conflicts_do_not_write() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let lock = metadata::Lock::acquire(&f.host.data_dir, true)
        .unwrap()
        .unwrap();
    let output = f.cli(&["image", "delete", "base"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("another chadm command"));
    drop(lock);
    fs::remove_file(f.raw()).unwrap();
    fs::remove_file(f.marker()).unwrap();
    assert!(!image::show(&f.host, "base").unwrap().available);
    assert!(repo::delete(&f.host, "local")
        .unwrap_err()
        .to_string()
        .contains("referenced by images"));
}

#[test]
fn permission_failure_rolls_back_creation_and_deletion() {
    let f = Fixture::new();
    f.repo();
    let parent = f.storage.join("images");
    if fs::metadata(&parent).unwrap().uid() == 0 {
        return;
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(image::create(&f.host, "base", "local", &f.input).is_err());
    f.assert_empty();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    f.create();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(image::delete(&f.host, "base").is_err());
    assert!(image::show(&f.host, "base").unwrap().available);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    image::delete(&f.host, "base").unwrap();
}

#[test]
fn marker_mismatch_corrupt_metadata_and_unavailable_repo_preserve_base() {
    let f = Fixture::new();
    f.repo();
    let config = f.create();
    let original = fs::read_to_string(f.marker()).unwrap();
    fs::write(f.marker(), original.replace(&config.id, &"0".repeat(32))).unwrap();
    assert!(!image::show(&f.host, "base").unwrap().available);
    assert!(image::delete(&f.host, "base").is_err());
    fs::write(f.marker(), original).unwrap();
    let original = fs::read_to_string(f.metadata()).unwrap();
    fs::write(
        f.metadata(),
        original.replace("schema_version = 1", "schema_version = 2"),
    )
    .unwrap();
    assert!(image::list(&f.host).is_err());
    assert!(image::delete(&f.host, "base").is_err());
    fs::write(f.metadata(), original).unwrap();
    fs::remove_file(f.storage.join(".chadm-repo.toml")).unwrap();
    assert!(!image::show(&f.host, "base").unwrap().available);
    assert!(image::delete(&f.host, "base").is_err());
    assert!(image::create(&f.host, "other", "local", &f.input).is_err());
    assert!(f.raw().exists());
}

#[test]
fn doctor_checks_base_image_availability() {
    let f = Fixture::new();
    f.repo();
    f.create();
    let text = String::from_utf8(f.cli(&["doctor"]).stdout).unwrap();
    assert!(text
        .lines()
        .find(|line| line.starts_with("images "))
        .unwrap()
        .contains("OK"));
    fs::remove_file(f.marker()).unwrap();
    let output = f.cli(&["doctor"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text
        .lines()
        .find(|line| line.starts_with("images "))
        .unwrap()
        .contains("FAIL"));
}

#[test]
fn windows_flag_imports_raw_and_qcow2_and_is_visible_in_metadata_and_output() {
    let f = Fixture::new();
    f.repo();
    let output = f.cli(&[
        "image",
        "create",
        "--name",
        "base",
        "--repo",
        "local",
        "--path",
        f.input.to_str().unwrap(),
        "--windows",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let info = image::show(&f.host, "base").unwrap();
    assert!(info.available && info.config.windows);
    let marker: ImageConfig = toml::from_str(&fs::read_to_string(f.marker()).unwrap()).unwrap();
    assert_eq!(marker, info.config);
    for args in [
        vec!["image", "list", "--json"],
        vec!["image", "show", "base", "--json"],
    ] {
        let output = f.cli(&args);
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"windows\":true"));
    }
    let output = f.cli(&["image", "show", "base"]);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Profile: windows"));

    let qcow2 = f.root.join("windows.qcow2");
    qemu(
        &["convert", "-f", "raw", "-O", "qcow2"],
        &[&f.input, &qcow2],
    );
    let imported = image::create_with_windows(&f.host, "converted", "local", &qcow2, true).unwrap();
    assert!(imported.windows);
    assert_eq!(imported.source_format, ImageFormat::Qcow2);
    let info = image::show(&f.host, "converted").unwrap();
    assert!(info.available);
    assert_eq!(
        fs::read(info.path.unwrap()).unwrap(),
        fs::read(&f.input).unwrap()
    );
}

#[test]
fn windows_seed_is_rejected_before_writes_and_legacy_images_default_to_generic() {
    let f = Fixture::new();
    let error =
        image::create_with_windows(&f.host, "default", "cloudinit", &f.input, true).unwrap_err();
    assert!(error.to_string().contains("--windows is not allowed"));
    assert!(!f.host.data_dir.exists());
    f.repo();
    let config = f.create();
    assert!(!config.windows);
    for path in [f.metadata(), f.marker()] {
        let legacy = fs::read_to_string(&path)
            .unwrap()
            .replace("windows = false\n", "");
        fs::write(path, legacy).unwrap();
    }
    let info = image::show(&f.host, "base").unwrap();
    assert!(info.available);
    assert!(!info.config.windows);
}
