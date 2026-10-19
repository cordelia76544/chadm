use crate::config::HostConfig;
use crate::error::{Error, Result};
use crate::metadata::{self, directory, io_error, Lock};
use crate::output::json_string;
use crate::repo;
use crate::storage::StorageDriver;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeConfig {
    pub schema_version: u32,
    pub managed_by: String,
    pub id: String,
    pub name: String,
    pub repo: String,
    pub repo_id: String,
    pub size_bytes: u64,
    pub inode: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_guid"
    )]
    pub zfs_guid: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_image_id: Option<String>,
}

pub struct VolumeInfo {
    pub config: VolumeConfig,
    pub path: Option<PathBuf>,
    pub available: bool,
    pub allocated_bytes: Option<u64>,
    pub reason: Option<String>,
}

pub fn validate_size(size: u64) -> Result<()> {
    if size == 0 || size > i64::MAX as u64 {
        return Err(Error::Resource(
            "volume size must be between 1 and 9223372036854775807 bytes".into(),
        ));
    }
    Ok(())
}

pub fn parse_size(value: &str) -> Result<u64> {
    let count = value.bytes().take_while(u8::is_ascii_digit).count();
    let number: u64 = value.get(..count).unwrap_or("").parse().map_err(|_| {
        Error::Resource(format!(
            "invalid volume size {value:?}; use an integer such as 30G"
        ))
    })?;
    let shift = match &value[count..] {
        "" | "B" => 0,
        "K" | "KiB" => 10,
        "M" | "MiB" => 20,
        "G" | "GiB" => 30,
        "T" | "TiB" => 40,
        _ => {
            return Err(Error::Resource(format!(
                "invalid volume size {value:?}; supported units: B, K/KiB, M/MiB, G/GiB, T/TiB"
            )))
        }
    };
    let size = number
        .checked_mul(1u64 << shift)
        .ok_or_else(|| Error::Resource("volume size overflow".into()))?;
    validate_size(size)?;
    Ok(size)
}

pub fn display_size(size: u64) -> String {
    for (shift, suffix) in [(40, "TiB"), (30, "GiB"), (20, "MiB"), (10, "KiB")] {
        let unit = 1u64 << shift;
        if size >= unit && size % unit == 0 {
            return format!("{} {suffix}", size / unit);
        }
    }
    format!("{size} B")
}

fn validate(config: &VolumeConfig) -> Result<()> {
    metadata::validate_name("volume", &config.name)?;
    repo::validate_name(&config.repo)?;
    validate_size(config.size_bytes)?;
    match (&config.source_image, &config.source_image_id) {
        (Some(name), Some(id)) if metadata::valid_id(id) => metadata::validate_name("image", name)?,
        (None, None) => (),
        _ => {
            return Err(Error::Resource(
                "invalid volume source image reference".into(),
            ))
        }
    }
    if config.schema_version != 1
        || config.managed_by != "chadm"
        || match config.zfs_guid {
            Some(guid) => guid == 0 || config.inode != 0,
            None => config.inode == 0,
        }
        || !metadata::valid_id(&config.id)
        || !metadata::valid_id(&config.repo_id)
    {
        return Err(Error::Resource(format!(
            "volume {:?} has unsupported or invalid ownership metadata",
            config.name
        )));
    }
    Ok(())
}

fn serialize(volume: &VolumeConfig) -> Result<String> {
    toml::to_string_pretty(volume)
        .map_err(|error| Error::Resource(format!("cannot serialize volume: {error}")))
}

fn parse(path: &Path) -> Result<VolumeConfig> {
    let text = metadata::read(path)?;
    let volume: VolumeConfig = toml::from_str(&text).map_err(|source| Error::Toml {
        path: path.into(),
        source,
    })?;
    validate(&volume)?;
    Ok(volume)
}

pub(crate) fn load(host: &HostConfig, name: &str) -> Result<VolumeConfig> {
    metadata::validate_name("volume", name)?;
    let dir = host.layout().volumes.join(name);
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Resource(format!("volume {name:?} was not found")))
        }
        Err(source) => return Err(io_error("inspect volume", &dir, source)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let volume = parse(&dir.join("config.toml"))?;
    if volume.name != name {
        return Err(Error::Resource(format!(
            "volume {name:?} metadata name does not match its directory"
        )));
    }
    Ok(volume)
}

/// The caller must hold the data-directory lock, including repo deletion.
pub(crate) fn configs_unlocked(host: &HostConfig) -> Result<Vec<VolumeConfig>> {
    let dir = host.layout().volumes;
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error("inspect volumes", &dir, source)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let mut volumes = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|source| io_error("list volumes", &dir, source))? {
        let entry = entry.map_err(|source| io_error("list volumes", &dir, source))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Resource("volume metadata name is not UTF-8".into()))?;
        if name.starts_with('.') {
            return Err(Error::Resource(format!(
                "unfinished volume transaction {name:?}; inspect metadata before retrying"
            )));
        }
        volumes.push(load(host, &name)?);
    }
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(volumes)
}

fn driver(host: &HostConfig, volume: &VolumeConfig) -> Result<Box<dyn StorageDriver>> {
    let repo = repo::load(host, &volume.repo)?;
    if repo.id != volume.repo_id {
        return Err(Error::Resource(format!(
            "volume {:?} references a different generation of repo {:?}",
            volume.name, volume.repo
        )));
    }
    if (repo.driver == repo::Driver::Zfs) != volume.zfs_guid.is_some() {
        return Err(Error::Resource(
            "volume identity does not match repo driver".into(),
        ));
    }
    crate::storage::driver(repo)
        .map_err(|error| Error::Resource(format!("repo {:?} is unavailable: {error}", volume.repo)))
}

fn check_marker(path: &Path, volume: &VolumeConfig) -> Result<()> {
    let marker = parse(path)?;
    if marker != *volume {
        return Err(Error::Resource(format!(
            "volume {:?} ownership marker does not match metadata",
            volume.name
        )));
    }
    Ok(())
}

pub(crate) fn info(host: &HostConfig, config: VolumeConfig) -> VolumeInfo {
    let mut path = None;
    let result = (|| {
        let driver = driver(host, &config)?;
        let resolved = driver.get_volume_path(&config)?;
        path = Some(resolved.clone());
        check_marker(&driver.volume_marker_path(&config)?, &config)?;
        driver.check_volume(&config)
    })();
    let (allocated_bytes, reason) = match result {
        Ok(stat) => (Some(stat.allocated_bytes), None),
        Err(error) => (None, Some(error.to_string())),
    };
    VolumeInfo {
        config,
        path,
        available: reason.is_none(),
        allocated_bytes,
        reason,
    }
}

pub fn list(host: &HostConfig) -> Result<Vec<VolumeInfo>> {
    host.validate()?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Ok(Vec::new());
    };
    Ok(configs_unlocked(host)?
        .into_iter()
        .map(|config| info(host, config))
        .collect())
}

pub fn show(host: &HostConfig, name: &str) -> Result<VolumeInfo> {
    host.validate()?;
    metadata::validate_name("volume", name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("volume {name:?} was not found")));
    };
    Ok(info(host, load(host, name)?))
}

pub fn create(
    host: &HostConfig,
    name: &str,
    repo_name: &str,
    size_bytes: u64,
) -> Result<VolumeConfig> {
    host.validate()?;
    metadata::validate_name("volume", name)?;
    repo::validate_name(repo_name)?;
    validate_size(size_bytes)?;
    // A volume cannot initialize an absent repo/data directory.
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("repo {repo_name:?} was not found")));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    crate::instance::configs_unlocked(host)?;
    create_unlocked(host, name, repo_name, size_bytes, None, None)
}

/// Caller holds the exclusive lock; instance creation keeps it across all disks.
pub(crate) fn create_unlocked(
    host: &HostConfig,
    name: &str,
    repo_name: &str,
    size_bytes: u64,
    source: Option<&crate::image::ImageConfig>,
    id: Option<&str>,
) -> Result<VolumeConfig> {
    metadata::validate_name("volume", name)?;
    repo::validate_name(repo_name)?;
    validate_size(size_bytes)?;
    repo::list_unlocked(host)?;
    if configs_unlocked(host)?
        .iter()
        .any(|volume| volume.name == name)
    {
        return Err(Error::Resource(format!("volume {name:?} already exists")));
    }
    let repo = repo::load(host, repo_name)?;
    if repo.name == crate::cloud_init::REPO {
        return Err(Error::Resource(
            "repo cloudinit is reserved for raw seed images; volumes are not allowed".into(),
        ));
    }
    let mut volume = VolumeConfig {
        schema_version: 1,
        managed_by: "chadm".into(),
        id: match id {
            Some(id) if metadata::valid_id(id) => id.into(),
            Some(_) => return Err(Error::Resource("invalid planned volume ID".into())),
            None => metadata::random_id()?,
        },
        name: name.into(),
        repo: repo_name.into(),
        repo_id: repo.id.clone(),
        size_bytes,
        inode: 0,
        zfs_guid: None,
        source_image: source.map(|image| image.name.clone()),
        source_image_id: source.map(|image| image.id.clone()),
    };
    let driver = crate::storage::driver(repo)
        .map_err(|error| Error::Resource(format!("repo {repo_name:?} is unavailable: {error}")))?;
    let source_path = if let Some(image) = source {
        let checked = crate::image::checked_unlocked(host, &image.name)?;
        if checked.config != *image || size_bytes < image.virtual_size_bytes {
            return Err(Error::Resource(
                "root volume size must not be smaller than the unchanged source image".into(),
            ));
        }
        checked.path
    } else {
        None
    };
    let path = driver.get_volume_path(&volume)?;
    let marker_path = driver.volume_marker_path(&volume)?;
    if driver.volume_exists(&volume)? {
        return Err(Error::Resource(format!(
            "volume payload for {name:?} already exists; refusing to overwrite"
        )));
    }
    for candidate in [&path, &marker_path] {
        match fs::symlink_metadata(candidate) {
            Ok(_) => {
                return Err(Error::Resource(format!(
                    "volume storage path {candidate:?} already exists; refusing to overwrite"
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(source) => return Err(io_error("inspect volume storage", candidate, source)),
        }
    }
    let volumes = host.layout().volumes;
    directory(&volumes, true)?;
    metadata::sync_directory(&host.data_dir)?;
    let stage = volumes.join(format!(".create-{}", volume.id));
    metadata::create_directory(&stage)?;
    let mut created_files = Vec::new();
    let mut payload_created = false;
    let result = (|| {
        let intent = stage.join("intent.toml");
        metadata::write_new(&intent, &serialize(&volume)?)?;
        created_files.push(intent.clone());
        metadata::sync_directory(&stage)?;
        metadata::sync_directory(&volumes)?;
        let identity = match (source, source_path.as_deref()) {
            (Some(image), Some(path)) => driver.clone_image(&volume, image, path)?,
            (None, None) => driver.create_volume(&volume)?,
            _ => return Err(Error::Resource("cannot resolve source image".into())),
        };
        volume.inode = identity.inode;
        volume.zfs_guid = identity.zfs_guid;
        payload_created = true;
        let text = serialize(&volume)?;
        let marker = driver.volume_marker_path(&volume)?;
        metadata::write_new(&marker, &text)?;
        created_files.push(marker);
        let config = stage.join("config.toml");
        metadata::write_new(&config, &text)?;
        created_files.push(config);
        fs::remove_file(&intent)
            .map_err(|source| io_error("remove completed intent", &intent, source))?;
        metadata::sync_directory(&stage)?;
        driver.sync_volume_storage()?;
        fs::rename(&stage, volumes.join(name))
            .map_err(|source| io_error("publish volume metadata", &stage, source))?;
        Ok(())
    })();
    if let Err(error) = result {
        let mut cleanup = Vec::new();
        // Do not discard ownership evidence if removing the raw file fails.
        if payload_created {
            if let Err(rollback) = driver.delete_volume(&volume) {
                return Err(Error::Resource(format!(
                    "{error}; rollback incomplete: {rollback}; transaction retained at {stage:?}"
                )));
            }
        }
        if !payload_created && driver.volume_exists(&volume).unwrap_or(true) {
            return Err(Error::Resource(format!(
                "{error}; incomplete volume retained at {stage:?}; inspect it before retrying"
            )));
        }
        for file in created_files.iter().rev() {
            if let Err(source) = fs::remove_file(file) {
                if source.kind() != std::io::ErrorKind::NotFound {
                    cleanup.push(format!("{file:?}: {source}"));
                }
            }
        }
        if let Err(source) = fs::remove_dir(&stage) {
            cleanup.push(format!("{stage:?}: {source}"));
        }
        if !cleanup.is_empty() {
            return Err(Error::Resource(format!(
                "{error}; rollback incomplete: {}",
                cleanup.join("; ")
            )));
        }
        return Err(error);
    }
    metadata::sync_directory(&volumes)?;
    Ok(volume)
}

pub fn attach(host: &HostConfig, name: &str, instance: &str) -> Result<()> {
    crate::instance::edit_volume(host, instance, name, true)
}
pub fn detach(host: &HostConfig, name: &str, instance: &str) -> Result<()> {
    crate::instance::edit_volume(host, instance, name, false)
}

pub fn delete(host: &HostConfig, name: &str) -> Result<()> {
    host.validate()?;
    metadata::validate_name("volume", name)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("volume {name:?} was not found")));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    delete_unlocked(host, name, None)
}

/// Expected metadata permits rollback of only a volume created by this command.
pub(crate) fn delete_unlocked(
    host: &HostConfig,
    name: &str,
    expected: Option<&VolumeConfig>,
) -> Result<()> {
    repo::list_unlocked(host)?;
    configs_unlocked(host)?;
    let volume = load(host, name)?;
    crate::checkpoint::ensure_volume_deletable(host, &volume)?;
    if let Some(expected) = expected {
        if expected != &volume {
            return Err(Error::Resource(
                "volume identity changed; refusing instance rollback".into(),
            ));
        }
    } else {
        let instances = crate::instance::configs_unlocked(host)
            .map_err(|error| Error::Resource(format!("cannot check volume references: {error}")))?;
        for instance in instances {
            if instance
                .disk
                .iter()
                .any(|disk| disk.volume == name || disk.volume_id == volume.id)
            {
                return Err(Error::Resource(format!(
                    "volume {name:?} is attached to instance {:?}",
                    instance.name
                )));
            }
        }
    }
    let driver = driver(host, &volume)?;
    check_marker(&driver.volume_marker_path(&volume)?, &volume)?;
    driver.check_volume(&volume)?;
    let volumes = host.layout().volumes;
    let dir = volumes.join(name);
    let entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|source| io_error("inspect volume metadata", &dir, source))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|source| io_error("inspect volume metadata", &dir, source))
        })
        .collect::<Result<_>>()?;
    if entries != vec![std::ffi::OsString::from("config.toml")] {
        return Err(Error::Resource(format!(
            "volume {name:?} metadata contains unexpected entries; refusing deletion"
        )));
    }
    let stage = volumes.join(format!(".delete-{}", volume.id));
    // configs_unlocked rejected unfinished transactions while holding the lock.
    fs::rename(&dir, &stage).map_err(|source| io_error("stage volume deletion", &dir, source))?;
    metadata::sync_directory(&volumes)?;
    if let Err(error) = driver.delete_volume(&volume) {
        if let Err(source) = fs::rename(&stage, &dir) {
            return Err(Error::Resource(format!("{error}; cannot restore volume metadata: {source}; transaction retained at {stage:?}")));
        }
        metadata::sync_directory(&volumes)?;
        return Err(error);
    }
    // After unlink the disk cannot be reconstructed. Preserve the tombstone on
    // any subsequent failure rather than pretending the volume was restored.
    let marker = driver.volume_marker_path(&volume)?;
    fs::remove_file(&marker)
        .map_err(|source| io_error("remove volume ownership marker", &marker, source))?;
    driver.sync_volume_storage()?;
    let config = stage.join("config.toml");
    fs::remove_file(&config)
        .map_err(|source| io_error("remove volume metadata", &config, source))?;
    fs::remove_dir(&stage)
        .map_err(|source| io_error("remove volume metadata directory", &stage, source))?;
    metadata::sync_directory(&volumes)
}

impl VolumeInfo {
    pub fn json(&self) -> String {
        format!("{{\"schema_version\":{},\"managed_by\":\"chadm\",\"id\":{},\"name\":{},\"repo\":{},\"repo_id\":{},\"size_bytes\":{},\"inode\":{},\"zfs_guid\":{},\"path\":{},\"available\":{},\"allocated_bytes\":{},\"reason\":{},\"source_image\":{},\"source_image_id\":{}}}",
            self.config.schema_version, json_string(&self.config.id), json_string(&self.config.name), json_string(&self.config.repo), json_string(&self.config.repo_id), self.config.size_bytes, self.config.inode, self.config.zfs_guid.map(|g|json_string(&g.to_string())).unwrap_or_else(||"null".into()),
            self.path.as_ref().map(|path| json_string(&path.to_string_lossy())).unwrap_or_else(|| "null".into()), self.available,
            self.allocated_bytes.map(|size| size.to_string()).unwrap_or_else(|| "null".into()), self.reason.as_ref().map(|reason| json_string(reason)).unwrap_or_else(|| "null".into()), self.config.source_image.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into()), self.config.source_image_id.as_ref().map(|value| json_string(value)).unwrap_or_else(|| "null".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zvol_guid_round_trips_and_reads_legacy_metadata() {
        let legacy = r#"
schema_version = 1
managed_by = "chadm"
id = "test"
name = "data"
repo = "fast"
repo_id = "test-repo"
size_bytes = 1073741824
inode = 0
zfs_guid = 3
"#;
        let mut volume: VolumeConfig = toml::from_str(legacy).unwrap();
        assert_eq!(volume.zfs_guid, Some(3));
        volume.zfs_guid = Some(u64::MAX);
        let encoded = serialize(&volume).unwrap();
        assert!(encoded.contains("zfs_guid = \"18446744073709551615\""));
        assert_eq!(toml::from_str::<VolumeConfig>(&encoded).unwrap(), volume);
        volume.zfs_guid = None;
        let encoded = serialize(&volume).unwrap();
        assert!(!encoded.contains("zfs_guid"));
        assert_eq!(toml::from_str::<VolumeConfig>(&encoded).unwrap(), volume);
    }

    #[test]
    fn sizes_are_binary_and_checked() {
        assert_eq!(parse_size("30G").unwrap(), 30 << 30);
        assert_eq!(parse_size("1KiB").unwrap(), 1024);
        assert_eq!(parse_size("512").unwrap(), 512);
        for value in [
            "",
            "0",
            "0G",
            "-1",
            "1.5G",
            "1GB",
            "1 g",
            "18446744073709551616",
            "18446744073709551615T",
            "9223372036854775808",
            "中文",
        ] {
            assert!(parse_size(value).is_err(), "{value:?}");
        }
    }
}
