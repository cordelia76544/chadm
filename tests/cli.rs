use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("chadm-test-{}-{id}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn chadm(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chadm"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn help_and_version_work_without_host_configuration() {
    let output = chadm(&["--help"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("doctor"));
    assert!(!text.contains("instance create"));
    let output = chadm(&["--config", "/nonexistent/chadm.toml", "version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("chadm {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_commands_and_missing_explicit_configuration_fail() {
    assert_eq!(
        chadm(&["instance", "unsupported", "test01"]).status.code(),
        Some(2)
    );
    let output = chadm(&["--config", "/nonexistent/chadm.toml", "doctor"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("cannot read"));
}

#[test]
fn doctor_does_not_create_resource_directories() {
    let temp = TempDir::new();
    let config_path = temp.0.join("config.toml");
    let config = chadm::config::HostConfig {
        data_dir: temp.0.join("data"),
        runtime_dir: temp.0.join("run"),
        // Deterministic failure, independent of host Cloud Hypervisor installation.
        cloud_hypervisor: temp.0.join("missing-cloud-hypervisor"),
        ..Default::default()
    };
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_chadm"))
        .arg("--config")
        .arg(&config_path)
        .arg("doctor")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("cloud-hypervisor"));
    assert!(text.contains("FAIL"));
    assert!(text.contains("not found"));
    assert!(!config.data_dir.exists());
    assert!(!config.runtime_dir.exists());
}

#[test]
fn config_rejects_malformed_oversized_and_symlink_files() {
    use chadm::config::HostConfig;
    let temp = TempDir::new();
    let path = temp.0.join("config.toml");
    std::fs::write(&path, "cpus = [").unwrap();
    assert!(HostConfig::load(Some(&path)).is_err());
    std::fs::write(&path, " ".repeat(65537)).unwrap();
    assert!(HostConfig::load(Some(&path))
        .unwrap_err()
        .to_string()
        .contains("64 KiB"));
    std::fs::write(&path, "").unwrap();
    assert!(HostConfig::load(Some(&path)).is_ok());
    let link = temp.0.join("link.toml");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(HostConfig::load(Some(&link))
        .unwrap_err()
        .to_string()
        .contains("symlink"));
}
