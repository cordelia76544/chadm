use chadm::{
    config::HostConfig,
    metadata::{self, Lock},
};
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// A separate test-process entry point keeps fork/allocation out of threaded tests.
#[test]
fn lock_holder_child() {
    let Some(path) = std::env::var_os("CHADM_LOCK_TEST_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(path);
    let _lock = Lock::acquire(&dir, true).unwrap();
    fs::write(dir.join("ready"), b"locked").unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
}

#[test]
fn killed_cli_releases_kernel_lock_without_replacing_lock_file() {
    let root = std::env::temp_dir().join(format!(
        "chadm-lock-crash-{}",
        metadata::random_id().unwrap()
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Child(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "lock_holder_child", "--nocapture"])
            .env("CHADM_LOCK_TEST_DIR", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("ready").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "lock holder exited early"
        );
        assert!(Instant::now() < deadline, "lock holder timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    let inode = fs::metadata(root.join(".lock")).unwrap().ino();
    assert!(Lock::acquire(&root, true).is_err());
    assert!(Lock::acquire(&root, false).is_err());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let lock = Lock::acquire(&root, true).unwrap();
    assert_eq!(fs::metadata(root.join(".lock")).unwrap().ino(), inode);
    drop(lock);
    assert!(Lock::acquire(&root, false).unwrap().is_some());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn doctor_json_is_read_only_and_retains_failure_exit_status() {
    let root = std::env::temp_dir().join(format!(
        "chadm-json-doctor-{}",
        metadata::random_id().unwrap()
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let host = HostConfig {
        data_dir: root.join("data"),
        runtime_dir: root.join("run"),
        cloud_hypervisor: root.join("missing-binary"),
        ..Default::default()
    };
    let path = root.join("host.toml");
    fs::write(&path, toml::to_string(&host).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_chadm"))
        .arg("--config")
        .arg(&path)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let json = String::from_utf8(output.stdout).unwrap();
    assert!(json.starts_with("{\"healthy\":false,\"checks\":["));
    assert!(json.contains("\"name\":\"cloud-hypervisor\",\"status\":\"FAIL\""));
    assert!(!host.data_dir.exists());
    assert!(!host.runtime_dir.exists());
    fs::remove_dir_all(root).unwrap();
}
