use chadm::{config::HostConfig, image, instance, metadata, repo, volume};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::process::Command;

fn fixture() -> (PathBuf, HostConfig) {
    let root =
        std::env::temp_dir().join(format!("chadm-zfs-test-{}", metadata::random_id().unwrap()));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let host = HostConfig {
        data_dir: root.join("data"),
        runtime_dir: root.join("run"),
        ..Default::default()
    };
    (root, host)
}

#[test]
fn invalid_zfs_namespace_and_file_only_cloudinit_fail_before_writes() {
    let (root, host) = fixture();
    for dataset in [
        "tank",
        "tank/../bad",
        "tank/chadm@snap",
        "tank/chadm#bookmark",
        "-r/tank",
        "tank/x;touch",
    ] {
        assert!(repo::create_zfs(&host, "fast", dataset).is_err());
    }
    assert!(repo::create_zfs(&host, "cloudinit", "tank/chadm").is_err());
    assert!(!host.data_dir.exists());
    let config = root.join("host.toml");
    fs::write(&config, toml::to_string(&host).unwrap()).unwrap();
    for args in [
        vec!["repo", "create", "--name", "fast", "--driver", "zfs"],
        vec![
            "repo",
            "create",
            "--name",
            "fast",
            "--driver",
            "zfs",
            "--path",
            "/tmp/storage",
        ],
        vec![
            "repo",
            "create",
            "--name",
            "local",
            "--driver",
            "file",
            "--pool",
            "tank/chadm",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_chadm"))
            .arg("--config")
            .arg(&config)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
    assert!(!host.data_dir.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_file_metadata_omits_native_identity_and_rejects_injected_zfs_fields() {
    let (root, host) = fixture();
    let storage = root.join("storage");
    let original = repo::create(&host, "local", &storage).unwrap();
    let original_volume = volume::create(&host, "data", "local", 1 << 20).unwrap();
    let path = host.layout().volumes.join("data/config.toml");
    assert!(!fs::read_to_string(&path).unwrap().contains("zfs_guid"));
    assert_eq!(original_volume.zfs_guid, None);
    let mut tampered = original_volume.clone();
    tampered.inode = 0;
    tampered.zfs_guid = Some(123);
    fs::write(&path, toml::to_string(&tampered).unwrap()).unwrap();
    assert!(!volume::show(&host, "data").unwrap().available);
    assert!(volume::delete(&host, "data").is_err());
    assert!(storage.join("volumes/data.raw").exists());
    fs::write(&path, toml::to_string(&original_volume).unwrap()).unwrap();
    volume::delete(&host, "data").unwrap();
    let path = host.layout().repos.join("local/config.toml");
    let mut tampered = original.clone();
    tampered.pool = Some("tank/chadm".into());
    tampered.zfs_guid = Some(123);
    fs::write(&path, toml::to_string(&tampered).unwrap()).unwrap();
    assert!(repo::delete(&host, "local").is_err());
    assert!(storage.is_dir());
    fs::write(&path, toml::to_string(&original).unwrap()).unwrap();
    repo::delete(&host, "local").unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires root, real OpenZFS and explicit CHADM_TEST_ZFS_PARENT; creates only a unique child dataset"]
fn host_zfs_repo_volume_image_clone_cleanup() {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    assert_eq!(
        unsafe { geteuid() },
        0,
        "run this explicit host test as root"
    );
    let parent = std::env::var("CHADM_TEST_ZFS_PARENT")
        .expect("set an existing parent filesystem, e.g. tank");
    let (root, host) = fixture();
    let dataset = format!("{parent}/chadm-test-{}", metadata::random_id().unwrap());
    eprintln!(
        "test metadata: {}; native dataset: {dataset}; preserve these on failure",
        root.display()
    );
    // No automatic native cleanup on failure: retain metadata/ownership evidence.
    let native = repo::create_zfs(&host, "fast", &dataset).unwrap();
    assert!(repo::show(&host, "fast").unwrap().available);
    volume::create(&host, "blank", "fast", 16 << 20).unwrap();
    let blank = volume::show(&host, "blank").unwrap();
    assert!(blank.available, "{:?}", blank.reason);
    assert!(blank.config.zfs_guid.is_some());
    let mut bytes = [1; 32];
    let mut file = fs::File::open(blank.path.unwrap()).unwrap();
    file.read_exact(&mut bytes).unwrap();
    assert_eq!(bytes, [0; 32]);
    drop(file);
    volume::delete(&host, "blank").unwrap();

    repo::create(&host, "local", &root.join("file-storage")).unwrap();
    let source = root.join("source.raw");
    let mut file = fs::File::create(&source).unwrap();
    file.set_len(1 << 20).unwrap();
    file.write_all(b"chadm independent root copy").unwrap();
    drop(file);
    image::create(&host, "base", "fast", &source).unwrap();
    let base = image::show(&host, "base").unwrap().path.unwrap();
    assert_eq!(
        fs::metadata(&base).unwrap().permissions().mode() & 0o777,
        0o400
    );
    let before = fs::read(&base).unwrap();
    volume::create(&host, "cold", "fast", 8 << 20).unwrap();
    for (name, target) in [("native", "fast"), ("filevm", "local")] {
        let mut options = instance::CreateOptions::new(name, "base");
        options.repo = Some(target.into());
        options.no_cloud_init = true;
        instance::create(&host, &options).unwrap();
        volume::attach(&host, "cold", name).unwrap();
        assert!(volume::delete(&host, "cold").is_err());
        volume::detach(&host, "cold", name).unwrap();
        assert!(volume::show(&host, "cold").unwrap().available);
        let name_volume = format!("{name}-root");
        chadm::checkpoint::create(&host, &format!("snap-{name}"), name).unwrap();
        let root_volume = volume::show(&host, &name_volume).unwrap();
        assert!(root_volume.available, "{:?}", root_volume.reason);
        let mut disk = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root_volume.path.unwrap())
            .unwrap();
        let mut cloned = vec![0; before.len()];
        disk.read_exact(&mut cloned).unwrap();
        assert_eq!(cloned, before);
        disk.seek(SeekFrom::Start(0)).unwrap();
        disk.write_all(b"guest writes").unwrap();
        disk.sync_all().unwrap();
        drop(disk);
        assert_eq!(fs::read(&base).unwrap(), before);
        chadm::checkpoint::restore(&host, &format!("snap-{name}")).unwrap();
        let path = volume::show(&host, &name_volume).unwrap().path.unwrap();
        let mut restored = vec![0; before.len()];
        fs::File::open(path)
            .unwrap()
            .read_exact(&mut restored)
            .unwrap();
        assert_eq!(restored, before);
        chadm::checkpoint::delete(&host, &format!("snap-{name}"), false).unwrap();
        assert!(volume::delete(&host, &name_volume).is_err());
        instance::delete(&host, name).unwrap();
        volume::delete(&host, &name_volume).unwrap();
    }
    volume::delete(&host, "cold").unwrap();
    // Verify the opposite copy direction: file image -> ZFS root.
    image::create(&host, "filebase", "local", &source).unwrap();
    let mut options = instance::CreateOptions::new("cross", "filebase");
    options.repo = Some("fast".into());
    options.no_cloud_init = true;
    instance::create(&host, &options).unwrap();
    assert!(volume::show(&host, "cross-root").unwrap().available);
    instance::delete(&host, "cross").unwrap();
    volume::delete(&host, "cross-root").unwrap();
    image::delete(&host, "filebase").unwrap();
    image::delete(&host, "base").unwrap();
    repo::delete(&host, "local").unwrap();
    repo::delete(&host, "fast").unwrap();
    let zfs = chadm::command::executable(std::path::Path::new("zfs")).unwrap();
    // udev may remove symlinks asynchronously; assert the native object instead.
    assert!(chadm::command::probe(&zfs, &["list", "-H", "-o", "name", &parent]).is_ok());
    assert!(chadm::command::probe(&zfs, &["list", "-H", "-o", "name", &dataset]).is_err());
    assert!(native.path.is_dir()); // The empty mountpoint is deliberately retained.
    fs::remove_dir_all(root).unwrap();
}
