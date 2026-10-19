use crate::config::HostConfig;
use crate::error::{Error, Result};
use crate::metadata::{self, directory, io_error, Lock};
use crate::output::json_string;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = ".chadm-repo.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    pub schema_version: u32,
    pub managed_by: String,
    pub id: String,
    pub name: String,
    pub driver: Driver,
    #[serde(default)]
    pub kind: RepoKind,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_guid"
    )]
    pub zfs_guid: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "crate::serde_guid"
    )]
    pub volumes_guid: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Driver {
    File,
    Zfs,
}
impl Driver {
    pub fn label(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Zfs => "zfs",
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum RepoKind {
    #[default]
    Storage,
    Cloudinit,
}
impl RepoKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Storage => "storage",
            Self::Cloudinit => "cloudinit",
        }
    }
}

#[derive(Debug)]
pub struct RepoInfo {
    pub config: RepoConfig,
    pub available: bool,
    pub reason: Option<String>,
}

pub fn validate_name(name: &str) -> Result<()> {
    metadata::validate_name("repo", name)
}

fn validate_config(repo: &RepoConfig) -> Result<()> {
    match repo.driver {
        Driver::File
            if repo.pool.is_none() && repo.zfs_guid.is_none() && repo.volumes_guid.is_none() => {}
        Driver::Zfs
            if repo.pool.is_some()
                && repo.zfs_guid.is_some_and(|g| g > 0)
                && repo.volumes_guid.is_some_and(|g| g > 0) =>
        {
            crate::storage::zfs::validate_dataset(repo.pool.as_deref().unwrap_or(""))?;
        }
        _ => return Err(Error::Resource("invalid repo backend identity".into())),
    }
    if (repo.kind == RepoKind::Cloudinit) != (repo.name == "cloudinit")
        || (repo.kind == RepoKind::Cloudinit && repo.driver != Driver::File)
    {
        return Err(Error::Resource(
            "cloud-init repo must be named cloudinit and use the file driver".into(),
        ));
    }
    validate_name(&repo.name)?;
    metadata::validate_path(&repo.path)?;
    if repo.schema_version != 1
        || repo.managed_by != "chadm"
        || repo.id.len() != 32
        || !repo
            .id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Resource(format!(
            "repo {:?} has unsupported or invalid ownership metadata",
            repo.name
        )));
    }
    Ok(())
}

fn parse(path: &Path) -> Result<RepoConfig> {
    let text = metadata::read(path)?;
    let repo: RepoConfig = toml::from_str(&text).map_err(|source| Error::Toml {
        path: path.into(),
        source,
    })?;
    validate_config(&repo)?;
    Ok(repo)
}

fn serialize(repo: &RepoConfig) -> Result<String> {
    toml::to_string_pretty(repo)
        .map_err(|error| Error::Resource(format!("cannot serialize repo: {error}")))
}

pub(crate) fn load(host: &HostConfig, name: &str) -> Result<RepoConfig> {
    validate_name(name)?;
    let dir = host.layout().repos.join(name);
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Resource(format!("repo {name:?} was not found")))
        }
        Err(source) => return Err(io_error("inspect repo", &dir, source)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let repo = parse(&dir.join("config.toml"))?;
    if repo.name != name {
        return Err(Error::Resource(format!(
            "repo {name:?} metadata name does not match its directory"
        )));
    }
    match repo.driver {
        Driver::File => validate_repo_path(host, &repo.path)?,
        Driver::Zfs if repo.path == host.data_dir.join("zfs-repos").join(name) => (),
        Driver::Zfs => {
            return Err(Error::Resource(
                "ZFS repo mountpoint does not match managed metadata".into(),
            ))
        }
    }
    Ok(repo)
}

fn validate_repo_path(host: &HostConfig, path: &Path) -> Result<()> {
    metadata::validate_path(path)?;
    let layout = host.layout();
    if host.data_dir.starts_with(path)
        || host.runtime_dir.starts_with(path)
        || path.starts_with(&host.runtime_dir)
        || [
            &layout.repos,
            &layout.volumes,
            &layout.images,
            &layout.instances,
            &host.data_dir.join("operations"),
            &host.data_dir.join("console-sessions"),
            &host.data_dir.join("zfs-repos"),
            &host.data_dir.join("snapshots"),
            &host.firmware_path(),
        ]
        .iter()
        .any(|dir| path.starts_with(dir) || dir.starts_with(path))
    {
        return Err(Error::Resource(format!(
            "repo path {path:?} overlaps chadm metadata or runtime directories"
        )));
    }
    Ok(())
}

pub(crate) fn list_unlocked(host: &HostConfig) -> Result<Vec<RepoConfig>> {
    let dir = host.layout().repos;
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error("inspect repos", &dir, source)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let mut repos = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|source| io_error("list repos", &dir, source))? {
        let entry = entry.map_err(|source| io_error("list repos", &dir, source))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Resource("repo metadata name is not UTF-8".into()))?;
        if name.starts_with('.') {
            return Err(Error::Resource(format!(
                "unfinished repo transaction {name:?}; inspect metadata before retrying"
            )));
        }
        repos.push(load(host, &name)?);
    }
    repos.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(repos)
}

pub fn verify(repo: &RepoConfig) -> Result<()> {
    validate_config(repo)?;
    if repo.driver == Driver::Zfs {
        crate::storage::zfs::verify_repo(repo)?;
    }
    directory(&repo.path, false)?;
    let marker = parse(&repo.path.join(MARKER))?;
    if marker != *repo {
        return Err(Error::Resource(format!(
            "repo {:?} ownership marker does not match metadata",
            repo.name
        )));
    }
    directory(&repo.path.join("images"), false)?;
    directory(&repo.path.join("volumes"), false)?;
    Ok(())
}

fn info(config: RepoConfig) -> RepoInfo {
    let reason = verify(&config).err().map(|error| error.to_string());
    RepoInfo {
        available: reason.is_none(),
        config,
        reason,
    }
}

pub fn list(host: &HostConfig) -> Result<Vec<RepoInfo>> {
    host.validate()?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Ok(Vec::new());
    };
    Ok(list_unlocked(host)?.into_iter().map(info).collect())
}

pub fn show(host: &HostConfig, name: &str) -> Result<RepoInfo> {
    host.validate()?;
    validate_name(name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("repo {name:?} was not found")));
    };
    Ok(info(load(host, name)?))
}

fn entries(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    fs::read_dir(path)
        .map_err(|source| io_error("list directory", path, source))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|source| io_error("list directory", path, source))
        })
        .collect()
}

pub fn create(host: &HostConfig, name: &str, path: &Path) -> Result<RepoConfig> {
    host.validate()?;
    validate_name(name)?;
    validate_repo_path(host, path)?;
    let _lock = Lock::acquire(&host.data_dir, true)?;
    let repos = host.layout().repos;
    directory(&repos, true)?;
    for repo in list_unlocked(host)? {
        if repo.name == name {
            return Err(Error::Resource(format!("repo {name:?} already exists")));
        }
        if path.starts_with(&repo.path) || repo.path.starts_with(path) {
            return Err(Error::Resource(format!(
                "repo path {path:?} overlaps repo {:?}",
                repo.name
            )));
        }
    }
    // Parent must already exist; do not create a chain of arbitrary storage directories.
    let parent = path
        .parent()
        .ok_or_else(|| Error::Resource("repo path has no parent".into()))?;
    directory(parent, false)?;
    let existed = match fs::symlink_metadata(path) {
        Ok(_) => {
            directory(path, false)?;
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(source) => return Err(io_error("inspect repo path", path, source)),
    };
    if existed && !entries(path)?.is_empty() {
        return Err(Error::Resource(format!(
            "repo path {path:?} is not empty; refusing to adopt existing data"
        )));
    }
    let repo = RepoConfig {
        schema_version: 1,
        managed_by: "chadm".into(),
        id: metadata::random_id()?,
        name: name.into(),
        driver: Driver::File,
        kind: if name == "cloudinit" {
            RepoKind::Cloudinit
        } else {
            RepoKind::Storage
        },
        path: path.into(),
        pool: None,
        zfs_guid: None,
        volumes_guid: None,
    };
    let text = serialize(&repo)?;
    let stage = repos.join(format!(".create-{}", repo.id));
    let mut created_files: Vec<PathBuf> = Vec::new();
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    let result = (|| {
        // Record the intent before touching storage, so crashes are visible.
        metadata::create_directory(&stage)?;
        created_dirs.push(stage.clone());
        let config = stage.join("config.toml");
        metadata::write_new(&config, &text)?;
        created_files.push(config.clone());
        metadata::sync_directory(&stage)?;
        metadata::sync_directory(&repos)?;
        if !existed {
            metadata::create_directory(path)?;
            created_dirs.push(path.into());
        }
        for child in ["images", "volumes"] {
            let child = path.join(child);
            metadata::create_directory(&child)?;
            created_dirs.push(child);
        }
        let marker = path.join(MARKER);
        metadata::write_new(&marker, &text)?;
        created_files.push(marker.clone());
        metadata::sync_directory(path)?;
        metadata::sync_directory(parent)?;
        fs::rename(&stage, repos.join(name))
            .map_err(|source| io_error("publish repo metadata", &stage, source))?;
        Ok(())
    })();
    if let Err(error) = result {
        let mut cleanup_errors = Vec::new();
        for file in created_files.iter().rev() {
            if let Err(source) = fs::remove_file(file) {
                if source.kind() != std::io::ErrorKind::NotFound {
                    cleanup_errors.push(format!("{file:?}: {source}"));
                }
            }
        }
        for dir in created_dirs.iter().rev() {
            if let Err(source) = fs::remove_dir(dir) {
                cleanup_errors.push(format!("{dir:?}: {source}"));
            }
        }
        if !cleanup_errors.is_empty() {
            return Err(Error::Resource(format!(
                "{error}; rollback incomplete: {}",
                cleanup_errors.join("; ")
            )));
        }
        return Err(error);
    }
    metadata::sync_directory(&repos)?;
    Ok(repo)
}

/// Only new datasets are accepted; existing foreign namespaces are never adopted.
pub fn create_zfs(host: &HostConfig, name: &str, pool: &str) -> Result<RepoConfig> {
    host.validate()?;
    validate_name(name)?;
    crate::storage::zfs::validate_dataset(pool)?;
    if name == "cloudinit" {
        return Err(Error::Resource(
            "cloudinit repo only supports the file driver".into(),
        ));
    }
    crate::storage::zfs::require_root()?;
    crate::storage::zfs::preflight_repo(pool)?;
    let _lock = Lock::acquire(&host.data_dir, true)?;
    let repos = host.layout().repos;
    directory(&repos, true)?;
    for existing in list_unlocked(host)? {
        if existing.name == name {
            return Err(Error::Resource(format!("repo {name:?} already exists")));
        }
        if existing.pool.as_ref().is_some_and(|other| {
            pool == other
                || pool.starts_with(&format!("{other}/"))
                || other.starts_with(&format!("{pool}/"))
        }) {
            return Err(Error::Resource(
                "ZFS repo datasets overlap an existing managed repo".into(),
            ));
        }
    }
    let parent = host.data_dir.join("zfs-repos");
    directory(&parent, true)?;
    let path = parent.join(name);
    if fs::symlink_metadata(&path).is_ok() {
        directory(&path, false)?;
        if !entries(&path)?.is_empty() {
            return Err(Error::Resource(
                "ZFS mountpoint is not empty; refusing to cover existing files".into(),
            ));
        }
    } else {
        metadata::create_directory(&path)?;
    }
    let mut repo = RepoConfig {
        schema_version: 1,
        managed_by: "chadm".into(),
        id: metadata::random_id()?,
        name: name.into(),
        driver: Driver::Zfs,
        kind: RepoKind::Storage,
        path,
        pool: Some(pool.into()),
        zfs_guid: None,
        volumes_guid: None,
    };
    let stage = repos.join(format!(".create-{}", repo.id));
    metadata::create_directory(&stage)?;
    metadata::write_new(&stage.join("intent.toml"), &serialize(&repo)?)?;
    metadata::sync_directory(&stage)?;
    metadata::sync_directory(&repos)?;
    let result = (|| {
        repo.zfs_guid = Some(crate::storage::zfs::create_root(&repo)?);
        metadata::write_new(&stage.join("root.toml"), &serialize(&repo)?)?;
        metadata::sync_directory(&stage)?;
        repo.volumes_guid = Some(crate::storage::zfs::create_container(&repo)?);
        metadata::write_new(&stage.join("resolved.toml"), &serialize(&repo)?)?;
        metadata::sync_directory(&stage)?;
        // Root ownership/mount source was checked before modifying these permissions.
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&repo.path, fs::Permissions::from_mode(0o700))
            .map_err(|e| io_error("protect ZFS repo mountpoint", &repo.path, e))?;
        for child in ["images", "volumes"] {
            metadata::create_directory(&repo.path.join(child))?;
        }
        metadata::write_new(&repo.path.join(MARKER), &serialize(&repo)?)?;
        metadata::sync_directory(&repo.path)?;
        verify(&repo)?;
        metadata::write_new(&stage.join("config.toml"), &serialize(&repo)?)?;
        for file in ["intent.toml", "root.toml", "resolved.toml"] {
            fs::remove_file(stage.join(file))
                .map_err(|e| io_error("remove completed ZFS repo intent", &stage, e))?;
        }
        metadata::sync_directory(&stage)?;
        fs::rename(&stage, repos.join(name))
            .map_err(|e| io_error("publish ZFS repo metadata", &stage, e))?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup) = crate::storage::zfs::rollback_repo(&repo) {
            return Err(Error::Resource(format!("{error}; ZFS repo rollback incomplete: {cleanup}; transaction retained at {stage:?}")));
        }
        for file in ["config.toml", "resolved.toml", "root.toml", "intent.toml"] {
            let file = stage.join(file);
            match fs::remove_file(&file) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(io_error("remove ZFS repo transaction", &file, e)),
            }
        }
        fs::remove_dir(&stage).map_err(|e| io_error("remove ZFS repo transaction", &stage, e))?;
        metadata::sync_directory(&repos)?;
        return Err(error);
    }
    metadata::sync_directory(&repos)?;
    Ok(repo)
}

pub fn delete(host: &HostConfig, name: &str) -> Result<()> {
    host.validate()?;
    validate_name(name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("repo {name:?} was not found")));
    };
    drop(_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    // Re-read only after acquiring the exclusive lock.
    list_unlocked(host)?;
    let repo = load(host, name)?;
    verify(&repo)?;
    crate::checkpoint::ensure_repo_deletable(host, &repo)?;
    if crate::volume::configs_unlocked(host)?
        .iter()
        .any(|volume| volume.repo == name || volume.repo_id == repo.id)
    {
        return Err(Error::Resource(format!(
            "repo {name:?} is referenced by volumes; delete its volumes first"
        )));
    }
    if crate::image::configs_unlocked(host)?
        .iter()
        .any(|image| image.repo == name || image.repo_id == repo.id)
    {
        return Err(Error::Resource(format!(
            "repo {name:?} is referenced by images; delete its images first"
        )));
    }
    for child in ["images", "volumes"] {
        if !entries(&repo.path.join(child))?.is_empty() {
            return Err(Error::Resource(format!(
                "repo {name:?} is not empty; remove its images and volumes first"
            )));
        }
    }
    let mut names = entries(&repo.path)?;
    names.sort();
    let mut expected = vec![
        std::ffi::OsString::from(MARKER),
        "images".into(),
        "volumes".into(),
    ];
    expected.sort();
    if names != expected {
        return Err(Error::Resource(format!(
            "repo {name:?} contains unmanaged entries; refusing deletion"
        )));
    }
    if repo.driver == Driver::Zfs {
        crate::storage::zfs::ensure_empty_repo(&repo)?;
    }
    let repos = host.layout().repos;
    let dir = repos.join(name);
    if entries(&dir)? != vec![std::ffi::OsString::from("config.toml")] {
        return Err(Error::Resource(format!(
            "repo {name:?} metadata contains unexpected entries; refusing deletion"
        )));
    }
    // Keep the complete config in a tombstone until payload cleanup succeeds.
    let stage = repos.join(format!(".delete-{}", repo.id));
    if fs::symlink_metadata(&stage).is_ok() {
        return Err(Error::Resource(
            "unfinished repo deletion; inspect metadata before retrying".into(),
        ));
    }
    fs::rename(&dir, &stage).map_err(|source| io_error("stage repo deletion", &dir, source))?;
    metadata::sync_directory(&repos)?;
    if repo.driver == Driver::Zfs {
        crate::storage::zfs::delete_repo(&repo).map_err(|e|Error::Resource(format!("{e}; ZFS deletion transaction retained at {stage:?}; inspect partial cleanup before retrying")))?;
        fs::remove_file(stage.join("config.toml"))
            .map_err(|e| io_error("remove deleted ZFS repo metadata", &stage, e))?;
        fs::remove_dir(&stage)
            .map_err(|e| io_error("remove deleted ZFS repo metadata", &stage, e))?;
        return metadata::sync_directory(&repos);
    }
    let result = (|| {
        for child in ["images", "volumes"] {
            let path = repo.path.join(child);
            fs::remove_dir(&path)
                .map_err(|source| io_error("remove empty directory", &path, source))?;
        }
        let marker = repo.path.join(MARKER);
        fs::remove_file(&marker)
            .map_err(|source| io_error("remove ownership marker", &marker, source))?;
        metadata::sync_directory(&repo.path)
    })();
    if let Err(error) = result {
        let rollback = (|| {
            for child in ["images", "volumes"] {
                directory(&repo.path.join(child), true)?;
            }
            let marker = repo.path.join(MARKER);
            if !marker.exists() {
                metadata::write_new(&marker, &serialize(&repo)?)?;
            }
            fs::rename(&stage, &dir)
                .map_err(|source| io_error("restore repo metadata", &dir, source))?;
            metadata::sync_directory(&repos)
        })();
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback) => Err(Error::Resource(format!(
                "{error}; rollback incomplete: {rollback}"
            ))),
        };
    }
    // Never remove the storage root and never use recursive deletion.
    let config = stage.join("config.toml");
    fs::remove_file(&config).map_err(|source| io_error("remove repo metadata", &config, source))?;
    fs::remove_dir(&stage)
        .map_err(|source| io_error("remove repo metadata directory", &stage, source))?;
    metadata::sync_directory(&repos)
}

impl RepoInfo {
    pub fn json(&self) -> String {
        format!("{{\"schema_version\":{},\"managed_by\":\"chadm\",\"id\":{},\"name\":{},\"driver\":{},\"kind\":{},\"path\":{},\"pool\":{},\"zfs_guid\":{},\"volumes_guid\":{},\"available\":{},\"reason\":{}}}",
            self.config.schema_version, json_string(&self.config.id), json_string(&self.config.name), json_string(self.config.driver.label()),
            json_string(self.config.kind.label()), json_string(&self.config.path.to_string_lossy()), self.config.pool.as_ref().map(|v|json_string(v)).unwrap_or_else(||"null".into()), self.config.zfs_guid.map(|g|json_string(&g.to_string())).unwrap_or_else(||"null".into()), self.config.volumes_guid.map(|g|json_string(&g.to_string())).unwrap_or_else(||"null".into()), self.available,
            self.reason.as_ref().map(|reason| json_string(reason)).unwrap_or_else(|| "null".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zfs_repo_guids_round_trip_and_read_legacy_metadata() {
        let legacy = r#"
schema_version = 1
managed_by = "chadm"
id = "test"
name = "fast"
driver = "zfs"
path = "/var/lib/chadm/zfs-repos/fast"
pool = "tank/chadm"
zfs_guid = 1
volumes_guid = 2
"#;
        let mut repo: RepoConfig = toml::from_str(legacy).unwrap();
        assert_eq!(repo.zfs_guid, Some(1));
        assert_eq!(repo.volumes_guid, Some(2));
        for (root, volumes) in [(u64::MAX, 2), (1, u64::MAX), (u64::MAX, u64::MAX)] {
            repo.zfs_guid = Some(root);
            repo.volumes_guid = Some(volumes);
            let encoded = serialize(&repo).unwrap();
            assert!(encoded.contains(&format!("zfs_guid = \"{root}\"")));
            assert!(encoded.contains(&format!("volumes_guid = \"{volumes}\"")));
            assert_eq!(toml::from_str::<RepoConfig>(&encoded).unwrap(), repo);
        }
    }

    #[test]
    fn names_reject_traversal_injection_and_invalid_lengths() {
        for name in [
            "", "../local", "a/b", "UPPER", "-local", "a;echo", "a\n", "a.b",
        ] {
            assert!(validate_name(name).is_err(), "{name:?}");
        }
        assert!(validate_name(&"a".repeat(64)).is_err());
        assert!(validate_name(&"a".repeat(63)).is_ok());
        assert!(validate_name("local-01").is_ok());
    }

    #[test]
    fn json_escapes_quotes_backslashes_and_control_characters() {
        assert_eq!(
            json_string("a\"\\\n\t\u{0}"),
            "\"a\\\"\\\\\\u000a\\u0009\\u0000\""
        );
        assert_eq!(json_string("本地"), "\"本地\"");
    }
}
