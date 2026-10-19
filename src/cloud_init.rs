//! The reserved `cloudinit` file repo stores immutable raw seed images.
use crate::{
    config::HostConfig,
    error::{Error, Result},
    image,
    instance_config::CloudInitConfig,
    repo,
};
use std::{
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
};

pub const REPO: &str = "cloudinit";

pub(crate) fn check_input(path: &Path) -> Result<()> {
    if path.extension().and_then(|s| s.to_str()) != Some("raw") {
        return Err(Error::Resource(
            "cloud-init accepts only .raw files; ISO and qcow2 are not supported".into(),
        ));
    }
    Ok(())
}

// Inspect the private snapshot, never the caller's mutable source again.
pub(crate) fn check_snapshot(path: &Path, format: image::ImageFormat) -> Result<()> {
    if format != image::ImageFormat::Raw {
        return Err(Error::Resource(
            "cloud-init seed must be raw; qcow2 conversion is not allowed".into(),
        ));
    }
    let file = File::open(path)
        .map_err(|e| crate::metadata::io_error("open cloud-init snapshot", path, e))?;
    let size = file
        .metadata()
        .map_err(|e| crate::metadata::io_error("inspect cloud-init snapshot", path, e))?
        .len();
    // ISO 9660 descriptors start at sector 16. Reject renamed .iso files too.
    if size >= 32774 {
        let mut magic = [0; 5];
        file.read_exact_at(&mut magic, 32769)
            .map_err(|e| crate::metadata::io_error("inspect cloud-init format", path, e))?;
        if &magic == b"CD001" {
            return Err(Error::Resource(
                "ISO cloud-init seeds are not supported, including files renamed to .raw".into(),
            ));
        }
    }
    Ok(())
}

/// Caller holds the metadata lock. Selection is persisted only at creation.
pub(crate) fn select(
    host: &HostConfig,
    name: Option<&str>,
    disabled: bool,
) -> Result<Option<CloudInitConfig>> {
    if disabled {
        if name.is_some() {
            return Err(Error::Resource(
                "--cloud-init conflicts with --no-cloud-init".into(),
            ));
        }
        return Ok(None);
    }
    if name.is_none() && !repo::list_unlocked(host)?.iter().any(|r| r.name == REPO) {
        return Ok(None);
    }
    repo::verify(&repo::load(host, REPO)?)?;
    let image = image::checked_unlocked(host, name.unwrap_or("default"))?;
    let seed = CloudInitConfig {
        image: image.config.name.clone(),
        image_id: image.config.id.clone(),
    };
    resolve(host, &seed)?;
    Ok(Some(seed))
}

pub(crate) fn resolve(host: &HostConfig, seed: &CloudInitConfig) -> Result<PathBuf> {
    let image = image::checked_unlocked(host, &seed.image)?;
    if image.config.repo != REPO
        || image.config.source_format != image::ImageFormat::Raw
        || image.config.id != seed.image_id
    {
        return Err(Error::Resource("cloud-init seed must reference the same managed raw image generation in repo cloudinit".into()));
    }
    let repo = repo::load(host, REPO)?;
    if repo.driver != repo::Driver::File {
        return Err(Error::Resource(
            "cloudinit repo only supports the file driver".into(),
        ));
    }
    image
        .path
        .ok_or_else(|| Error::Resource("cloud-init seed path is unavailable".into()))
}
