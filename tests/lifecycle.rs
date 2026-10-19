use chadm::{config::HostConfig, image, instance, lifecycle, metadata, repo};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::process::Command;

#[test]
fn lifecycle_cli_validates_names_and_missing_metadata_without_starting_units() {
    let root = std::env::temp_dir().join(format!(
        "chadm-lifecycle-cli-{}",
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
    for action in ["start", "stop", "reboot", "recover"] {
        for name in ["missing", "../../etc", "a;reboot"] {
            let output = Command::new(env!("CARGO_BIN_EXE_chadm"))
                .arg("--config")
                .arg(&config)
                .args(["instance", action, name])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(!host.data_dir.exists());
            assert!(!host.runtime_dir.exists());
        }
    }
    let help = Command::new(env!("CARGO_BIN_EXE_chadm"))
        .args(["instance", "--help"])
        .output()
        .unwrap();
    let text = String::from_utf8(help.stdout).unwrap();
    for command in ["start", "stop", "reboot", "recover"] {
        assert!(text.contains(command));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_chadm"))
        .args(["__service", "prepare", "test01"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    fs::remove_dir_all(root).unwrap();
}

/// Explicit host-only test; fixed shared service config, unique repo/image/instance names.
#[test]
#[ignore = "requires root, installed shared template/binary, KVM/TUN, and CHADM_TEST_IMAGE plus fixed CLOUDHV.fd"]
fn host_file_vm_start_reboot_stop() {
    let source = std::env::var("CHADM_TEST_IMAGE")
        .expect("CHADM_TEST_IMAGE must name an offline bootable cloud image");
    assert!(
        HostConfig::load(None).unwrap().firmware_path().is_file(),
        "install compatible firmware at data_dir/CLOUDHV.fd before running this host test"
    );
    let bridge = std::env::var("CHADM_TEST_BRIDGE").unwrap_or_else(|_| "virbr0".into());
    let host = HostConfig::load(None).unwrap();
    let id = metadata::random_id().unwrap();
    let name = format!("p7-{}", &id[..6]);
    let root = std::env::temp_dir().join(format!("chadm-live-{id}"));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    repo::create(&host, &name, &root.join("storage")).unwrap();
    image::create(&host, &name, &name, std::path::Path::new(&source)).unwrap();
    let mut options = instance::CreateOptions::new(&name, &name);
    options.no_cloud_init = true; // Host smoke test must not inherit unrelated default credentials.
    options.cpus = 1;
    options.ram = 512 << 20;
    options.boot = instance::BootConfig::Firmware { firmware: None };
    options
        .networks
        .push(instance::parse_network(&format!("bridge={bridge},dhcp")).unwrap());
    let config = instance::create(&host, &options).unwrap();
    struct Cleanup<'a> {
        host: &'a HostConfig,
        name: &'a str,
        root: &'a std::path::Path,
    }
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            let cleanup = (|| -> chadm::error::Result<()> {
                let info = instance::show(self.host, self.name)?;
                if info.systemd_managed {
                    lifecycle::stop(self.host, self.name, true)?;
                }
                instance::delete(self.host, self.name)?;
                chadm::volume::delete(self.host, &format!("{}-root", self.name))?;
                image::delete(self.host, self.name)?;
                repo::delete(self.host, self.name)?;
                Ok(())
            })();
            if let Err(error) = cleanup {
                eprintln!(
                    "host test cleanup failed: {error}; preserving resources {:?} at {:?}",
                    self.name, self.root
                );
            } else {
                let _ = fs::remove_dir_all(self.root);
            }
        }
    }
    let _cleanup = Cleanup {
        host: &host,
        name: &name,
        root: &root,
    };
    lifecycle::start(&host, &name).unwrap();
    chadm::volume::create(&host, &format!("{name}-extra"), &name, 16 << 20).unwrap();
    chadm::volume::attach(&host, &format!("{name}-extra"), &name).unwrap();
    assert!(instance::show(&host, &name)
        .unwrap()
        .config
        .disk
        .iter()
        .any(|disk| disk.volume == format!("{name}-extra")));
    assert!(chadm::volume::delete(&host, &format!("{name}-extra")).is_err());
    // This new blank disk is never mounted or formatted by the Guest.
    chadm::volume::detach(&host, &format!("{name}-extra"), &name).unwrap();
    chadm::volume::delete(&host, &format!("{name}-extra")).unwrap();
    lifecycle::suspend(&host, &name).unwrap();
    assert_eq!(instance::show(&host, &name).unwrap().state, "suspended");
    assert!(!host.runtime_dir.join(&name).exists());
    assert!(lifecycle::start(&host, &name).is_err());
    lifecycle::resume(&host, &name).unwrap();
    assert_eq!(instance::show(&host, &name).unwrap().state, "running");
    assert!(chadm::checkpoint::list(&host)
        .unwrap()
        .iter()
        .all(|s| s.instance != name));
    let info = instance::show(&host, &name).unwrap();
    assert_eq!(info.state, "running");
    assert_eq!(info.state_source, "systemd+api");
    assert!(lifecycle::start(&host, &name).is_err());
    assert!(instance::delete(&host, &name).is_err());
    lifecycle::reboot(&host, &name, true).unwrap();
    assert_eq!(
        instance::show(&host, &name).unwrap().config.network,
        config.network
    );
    lifecycle::stop(&host, &name, true).unwrap();
    let info = instance::show(&host, &name).unwrap();
    assert_eq!(info.state, "stopped");
    assert!(!host.runtime_dir.join(&name).exists());
    assert!(!std::path::Path::new("/sys/class/net")
        .join(&config.network[0].tap)
        .exists());
    // Prepare an owned orphan TAP without a VM, mimicking interrupted preparation.
    chadm::network::prepare(&host, &name).unwrap();
    lifecycle::recover(&host, &name).unwrap();
    lifecycle::recover(&host, &name).unwrap();
    assert!(!std::path::Path::new("/sys/class/net")
        .join(&config.network[0].tap)
        .exists());
    // Crash only this unique test VMM through its own systemd unit, never a PID file.
    lifecycle::start(&host, &name).unwrap();
    let systemctl = chadm::command::executable(std::path::Path::new("systemctl")).unwrap();
    chadm::command::probe(
        &systemctl,
        &[
            "--system",
            "kill",
            "--kill-whom=main",
            "--signal=SIGKILL",
            &chadm::systemd::unit(&name).unwrap(),
        ],
    )
    .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    loop {
        let info = instance::show(&host, &name).unwrap();
        if ["failed", "stopped"].contains(&info.state) && !host.runtime_dir.join(&name).exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "ExecStopPost did not clean crashed test VM"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    lifecycle::recover(&host, &name).unwrap();
    lifecycle::start(&host, &name).unwrap();
    lifecycle::stop(&host, &name, true).unwrap();
}
