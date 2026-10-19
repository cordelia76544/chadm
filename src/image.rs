use crate::config::HostConfig;
use crate::error::{Error, Result};
use crate::metadata::{self, directory, io_error, Lock};
use crate::output::json_string;
use crate::repo;
use crate::storage::StorageDriver;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Raw,
    Qcow2,
}
impl ImageFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Qcow2 => "qcow2",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageConfig {
    pub schema_version: u32,
    pub managed_by: String,
    pub id: String,
    pub name: String,
    pub repo: String,
    pub repo_id: String,
    pub virtual_size_bytes: u64,
    pub format: ImageFormat,
    pub source_format: ImageFormat,
    #[serde(default)]
    pub windows: bool,
    pub inode: u64,
}

pub struct ImageInfo {
    pub config: ImageConfig,
    pub path: Option<PathBuf>,
    pub available: bool,
    pub allocated_bytes: Option<u64>,
    pub reason: Option<String>,
}

fn validate(config: &ImageConfig) -> Result<()> {
    metadata::validate_name("image", &config.name)?;
    repo::validate_name(&config.repo)?;
    if config.windows && config.repo == crate::cloud_init::REPO {
        return Err(Error::Resource(
            "cloudinit seed cannot be marked as a Windows base image".into(),
        ));
    }
    if config.virtual_size_bytes == 0
        || config.virtual_size_bytes > i64::MAX as u64
        || config.format != ImageFormat::Raw
    {
        return Err(Error::Resource(
            "invalid image virtual size or stored format".into(),
        ));
    }
    if config.schema_version != 1
        || config.managed_by != "chadm"
        || config.inode == 0
        || !metadata::valid_id(&config.id)
        || !metadata::valid_id(&config.repo_id)
    {
        return Err(Error::Resource(format!(
            "image {:?} has unsupported or invalid ownership metadata",
            config.name
        )));
    }
    Ok(())
}

fn serialize(image: &ImageConfig) -> Result<String> {
    toml::to_string_pretty(image)
        .map_err(|error| Error::Resource(format!("cannot serialize image: {error}")))
}

fn parse(path: &Path) -> Result<ImageConfig> {
    let text = metadata::read(path)?;
    let image: ImageConfig = toml::from_str(&text).map_err(|source| Error::Toml {
        path: path.into(),
        source,
    })?;
    validate(&image)?;
    Ok(image)
}

pub(crate) fn load(host: &HostConfig, name: &str) -> Result<ImageConfig> {
    metadata::validate_name("image", name)?;
    let dir = host.layout().images.join(name);
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Resource(format!("image {name:?} was not found")))
        }
        Err(source) => return Err(io_error("inspect image", &dir, source)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let image = parse(&dir.join("config.toml"))?;
    if image.name != name {
        return Err(Error::Resource(format!(
            "image {name:?} metadata name does not match its directory"
        )));
    }
    Ok(image)
}

/// The caller must hold the data-directory lock, including repo deletion.
pub(crate) fn configs_unlocked(host: &HostConfig) -> Result<Vec<ImageConfig>> {
    let dir = host.layout().images;
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error("inspect images", &dir, source)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let mut images = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|source| io_error("list images", &dir, source))? {
        let entry = entry.map_err(|source| io_error("list images", &dir, source))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Resource("image metadata name is not UTF-8".into()))?;
        if name.starts_with('.') {
            return Err(Error::Resource(format!(
                "unfinished image transaction {name:?}; inspect metadata before retrying"
            )));
        }
        images.push(load(host, &name)?);
    }
    images.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(images)
}

fn driver(host: &HostConfig, image: &ImageConfig) -> Result<Box<dyn StorageDriver>> {
    let repo = repo::load(host, &image.repo)?;
    if repo.id != image.repo_id {
        return Err(Error::Resource(format!(
            "image {:?} references a different generation of repo {:?}",
            image.name, image.repo
        )));
    }
    crate::storage::driver(repo)
        .map_err(|error| Error::Resource(format!("repo {:?} is unavailable: {error}", image.repo)))
}

fn marker_path(path: &Path) -> PathBuf {
    path.with_extension("chadm.toml")
}

fn check_marker(path: &Path, image: &ImageConfig) -> Result<()> {
    let marker = parse(&marker_path(path))?;
    if marker != *image {
        return Err(Error::Resource(format!(
            "image {:?} ownership marker does not match metadata",
            image.name
        )));
    }
    Ok(())
}

pub(crate) fn info(host: &HostConfig, config: ImageConfig) -> ImageInfo {
    let mut path = None;
    let result = (|| {
        let driver = driver(host, &config)?;
        let resolved = driver.get_image_path(&config)?;
        path = Some(resolved.clone());
        check_marker(&resolved, &config)?;
        driver.check_image(&config)
    })();
    let (allocated_bytes, reason) = match result {
        Ok(stat) => (Some(stat.allocated_bytes), None),
        Err(error) => (None, Some(error.to_string())),
    };
    ImageInfo {
        config,
        path,
        available: reason.is_none(),
        allocated_bytes,
        reason,
    }
}

/// Caller holds the data directory lock.
pub(crate) fn checked_unlocked(host: &HostConfig, name: &str) -> Result<ImageInfo> {
    let info = info(host, load(host, name)?);
    if !info.available {
        return Err(Error::Resource(format!(
            "image {name:?} is unavailable: {}",
            info.reason.as_deref().unwrap_or("unknown reason")
        )));
    }
    Ok(info)
}

pub fn list(host: &HostConfig) -> Result<Vec<ImageInfo>> {
    host.validate()?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Ok(Vec::new());
    };
    Ok(configs_unlocked(host)?
        .into_iter()
        .map(|config| info(host, config))
        .collect())
}

pub fn show(host: &HostConfig, name: &str) -> Result<ImageInfo> {
    host.validate()?;
    metadata::validate_name("image", name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("image {name:?} was not found")));
    };
    Ok(info(host, load(host, name)?))
}

pub fn create(
    host: &HostConfig,
    name: &str,
    repo_name: &str,
    source: &Path,
) -> Result<ImageConfig> {
    create_with_windows(host, name, repo_name, source, false)
}

pub fn create_with_windows(
    host: &HostConfig,
    name: &str,
    repo_name: &str,
    source: &Path,
    windows: bool,
) -> Result<ImageConfig> {
    host.validate()?;
    metadata::validate_name("image", name)?;
    repo::validate_name(repo_name)?;
    if windows && repo_name == crate::cloud_init::REPO {
        return Err(Error::Resource(
            "--windows is not allowed for cloudinit seed images".into(),
        ));
    }
    if repo_name == crate::cloud_init::REPO {
        crate::cloud_init::check_input(source)?;
    }
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("repo {repo_name:?} was not found")));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    repo::list_unlocked(host)?;
    if configs_unlocked(host)?
        .iter()
        .any(|image| image.name == name)
    {
        return Err(Error::Resource(format!("image {name:?} already exists")));
    }
    let repo = repo::load(host, repo_name)?;
    let mut image = ImageConfig {
        schema_version: 1,
        managed_by: "chadm".into(),
        id: metadata::random_id()?,
        name: name.into(),
        repo: repo_name.into(),
        repo_id: repo.id.clone(),
        virtual_size_bytes: 0,
        format: ImageFormat::Raw,
        source_format: ImageFormat::Raw,
        windows,
        inode: 0,
    };
    let driver = crate::storage::driver(repo)
        .map_err(|error| Error::Resource(format!("repo {repo_name:?} is unavailable: {error}")))?;
    let path = driver.get_image_path(&image)?;
    for candidate in [&path, &marker_path(&path)] {
        match fs::symlink_metadata(candidate) {
            Ok(_) => {
                return Err(Error::Resource(format!(
                    "image storage path {candidate:?} already exists; refusing to overwrite"
                )))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(source) => return Err(io_error("inspect image storage", candidate, source)),
        }
    }
    let images = host.layout().images;
    directory(&images, true)?;
    metadata::sync_directory(&host.data_dir)?;
    let stage = images.join(format!(".create-{}", image.id));
    metadata::create_directory(&stage)?;
    let snapshot = stage.join("input.snapshot");
    let mut created_files = Vec::new();
    let mut payload_created = false;
    let result = (|| {
        let intent = stage.join("intent.toml");
        metadata::write_new(&intent, &serialize(&image)?)?;
        created_files.push(intent.clone());
        metadata::sync_directory(&stage)?;
        metadata::sync_directory(&images)?;
        crate::image_input::snapshot(source, &snapshot)?;
        created_files.push(snapshot.clone());
        let (format, size) = crate::image_input::inspect(&snapshot)?;
        if repo_name == crate::cloud_init::REPO {
            crate::cloud_init::check_snapshot(&snapshot, format)?;
        }
        image.source_format = format;
        image.virtual_size_bytes = size;
        // Update the intent before any payload operation so interrupted imports
        // retain the actual source format and expected virtual size.
        let resolved = stage.join("resolved.toml");
        metadata::write_new(&resolved, &serialize(&image)?)?;
        created_files.push(resolved.clone());
        metadata::sync_directory(&stage)?;
        image.inode = driver.import_image(&image, &snapshot)?;
        payload_created = true;
        let text = serialize(&image)?;
        let marker = marker_path(&path);
        metadata::write_new(&marker, &text)?;
        created_files.push(marker);
        let config = stage.join("config.toml");
        metadata::write_new(&config, &text)?;
        created_files.push(config);
        for temporary in [&snapshot, &intent, &resolved] {
            fs::remove_file(temporary).map_err(|source| {
                io_error("remove completed import staging file", temporary, source)
            })?;
        }
        metadata::sync_directory(&stage)?;
        metadata::sync_directory(
            path.parent()
                .ok_or_else(|| Error::Resource("image path has no parent".into()))?,
        )?;
        fs::rename(&stage, images.join(name))
            .map_err(|source| io_error("publish image metadata", &stage, source))?;
        Ok(())
    })();
    if let Err(error) = result {
        if payload_created {
            if let Err(rollback) = driver.delete_image(&image) {
                return Err(Error::Resource(format!(
                    "{error}; rollback incomplete: {rollback}; transaction retained at {stage:?}"
                )));
            }
        } else {
            // Only a definite ENOENT proves the backend removed its partial file.
            // Permission/I/O failures must preserve the import intent as evidence.
            let missing = |candidate: &Path| {
                matches!(fs::symlink_metadata(candidate),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            };
            if !missing(&path) || (!created_files.contains(&snapshot) && !missing(&snapshot)) {
                return Err(Error::Resource(format!(
                    "{error}; incomplete import retained at {stage:?}; inspect it before retrying"
                )));
            }
        }
        let mut cleanup = Vec::new();
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
        metadata::sync_directory(&images)?;
        metadata::sync_directory(
            path.parent()
                .ok_or_else(|| Error::Resource("image path has no parent".into()))?,
        )?;
        return Err(error);
    }
    metadata::sync_directory(&images)?;
    Ok(image)
}

pub fn delete(host: &HostConfig, name: &str) -> Result<()> {
    host.validate()?;
    metadata::validate_name("image", name)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("image {name:?} was not found")));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    repo::list_unlocked(host)?;
    configs_unlocked(host)?;
    let image = load(host, name)?;
    let volumes = crate::volume::configs_unlocked(host)?;
    crate::checkpoint::ensure_image_deletable(host, &image)?;
    if let Some(volume) = volumes.iter().find(|volume| {
        volume.source_image.as_deref() == Some(name)
            || volume.source_image_id.as_deref() == Some(image.id.as_str())
    }) {
        return Err(Error::Resource(format!(
            "image {name:?} is referenced by volume {:?}",
            volume.name
        )));
    }
    let instances = crate::instance::configs_unlocked(host)
        .map_err(|error| Error::Resource(format!("cannot check image references: {error}")))?;
    if let Some(instance) = instances.iter().find(|instance| {
        instance.image == name
            || instance.image_id == image.id
            || instance
                .cloud_init
                .as_ref()
                .is_some_and(|seed| seed.image == name || seed.image_id == image.id)
    }) {
        return Err(Error::Resource(format!(
            "image {name:?} is referenced by instance {:?}",
            instance.name
        )));
    }
    let driver = driver(host, &image)?;
    let path = driver.get_image_path(&image)?;
    check_marker(&path, &image)?;
    driver.check_image(&image)?;
    let images = host.layout().images;
    let dir = images.join(name);
    let entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|source| io_error("inspect image metadata", &dir, source))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|source| io_error("inspect image metadata", &dir, source))
        })
        .collect::<Result<_>>()?;
    if entries != vec![std::ffi::OsString::from("config.toml")] {
        return Err(Error::Resource(format!(
            "image {name:?} metadata contains unexpected entries; refusing deletion"
        )));
    }
    let stage = images.join(format!(".delete-{}", image.id));
    // configs_unlocked rejected unfinished transactions while holding the lock.
    fs::rename(&dir, &stage).map_err(|source| io_error("stage image deletion", &dir, source))?;
    metadata::sync_directory(&images)?;
    if let Err(error) = driver.delete_image(&image) {
        if let Err(source) = fs::rename(&stage, &dir) {
            return Err(Error::Resource(format!("{error}; cannot restore image metadata: {source}; transaction retained at {stage:?}")));
        }
        metadata::sync_directory(&images)?;
        return Err(error);
    }
    // After unlink the disk cannot be reconstructed. Preserve the tombstone on
    // any subsequent failure rather than pretending the image was restored.
    let marker = marker_path(&path);
    fs::remove_file(&marker)
        .map_err(|source| io_error("remove image ownership marker", &marker, source))?;
    metadata::sync_directory(
        path.parent()
            .ok_or_else(|| Error::Resource("image path has no parent".into()))?,
    )?;
    let config = stage.join("config.toml");
    fs::remove_file(&config)
        .map_err(|source| io_error("remove image metadata", &config, source))?;
    fs::remove_dir(&stage)
        .map_err(|source| io_error("remove image metadata directory", &stage, source))?;
    metadata::sync_directory(&images)
}

impl ImageInfo {
    pub fn json(&self) -> String {
        format!("{{\"schema_version\":{},\"managed_by\":\"chadm\",\"id\":{},\"name\":{},\"repo\":{},\"repo_id\":{},\"format\":\"raw\",\"source_format\":{},\"windows\":{},\"virtual_size_bytes\":{},\"inode\":{},\"path\":{},\"available\":{},\"allocated_bytes\":{},\"reason\":{}}}",
            self.config.schema_version, json_string(&self.config.id), json_string(&self.config.name), json_string(&self.config.repo), json_string(&self.config.repo_id), json_string(self.config.source_format.label()), self.config.windows, self.config.virtual_size_bytes, self.config.inode,
            self.path.as_ref().map(|path| json_string(&path.to_string_lossy())).unwrap_or_else(|| "null".into()), self.available,
            self.allocated_bytes.map(|size| size.to_string()).unwrap_or_else(|| "null".into()), self.reason.as_ref().map(|reason| json_string(reason)).unwrap_or_else(|| "null".into()))
    }
}
