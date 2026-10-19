use crate::error::{Error, Result};
use crate::metadata::{self, io_error};
use crate::repo::RepoConfig;
use crate::storage::{StorageDriver, VolumeIdentity, VolumeStat};
use crate::volume::VolumeConfig;
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

unsafe extern "C" {
    fn fsetxattr(
        fd: i32,
        name: *const std::ffi::c_char,
        value: *const std::ffi::c_void,
        size: usize,
        flags: i32,
    ) -> i32;
    fn fgetxattr(
        fd: i32,
        name: *const std::ffi::c_char,
        value: *mut std::ffi::c_void,
        size: usize,
    ) -> isize;
}

const ID_ATTRIBUTE: &std::ffi::CStr = c"user.chadm.volume-id";

pub(crate) fn mark_file_with(file: &fs::File, id: &str, attribute: &std::ffi::CStr) -> Result<()> {
    if !metadata::valid_id(id) {
        return Err(Error::Resource("invalid payload ownership ID".into()));
    }
    // SAFETY: fd is live, name is NUL terminated, and value is readable for id.len() bytes.
    // XATTR_CREATE (1) refuses to replace a preexisting ownership attribute.
    let result = unsafe {
        fsetxattr(
            file.as_raw_fd(),
            attribute.as_ptr(),
            id.as_ptr().cast(),
            id.len(),
            1,
        )
    };
    if result != 0 {
        return Err(Error::Resource(format!(
            "cannot mark raw payload ownership: {}; file backend requires user extended attributes",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

pub(crate) fn check_file_with(file: &fs::File, id: &str, attribute: &std::ffi::CStr) -> Result<()> {
    let mut value = [0u8; 32];
    // SAFETY: fd/name are valid and value provides the exact writable buffer length.
    let result = unsafe {
        fgetxattr(
            file.as_raw_fd(),
            attribute.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if result < 0 {
        return Err(Error::Resource(format!(
            "ownership attribute is unavailable: {}",
            std::io::Error::last_os_error()
        )));
    }
    if result as usize != value.len() || value.as_slice() != id.as_bytes() {
        return Err(Error::Resource(
            "ownership attribute does not match metadata".into(),
        ));
    }
    Ok(())
}

pub struct FileDriver {
    pub(crate) repo: RepoConfig,
}

impl FileDriver {
    pub fn new(repo: RepoConfig) -> Result<Self> {
        if repo.driver != crate::repo::Driver::File {
            return Err(Error::Resource(
                "file volume driver requires a file repo".into(),
            ));
        }
        crate::repo::verify(&repo)?;
        Ok(Self { repo })
    }
}

impl StorageDriver for FileDriver {
    fn snapshot_exists(&self, v: &VolumeConfig, id: &str) -> Result<bool> {
        self.snapshot_file_exists(v, id)
    }
    fn snapshot_volume(&self, v: &VolumeConfig, id: &str) -> Result<u64> {
        self.snapshot_file(v, id)
    }
    fn check_snapshot(&self, v: &VolumeConfig, id: &str, identity: u64, _: bool) -> Result<()> {
        self.check_volume(v)?;
        self.checked_snapshot_file(v, id, identity).map(|_| ())
    }
    fn restore_snapshot(&self, v: &VolumeConfig, id: &str, identity: u64) -> Result<()> {
        self.restore_snapshot_file(v, id, identity)
    }
    fn delete_snapshot(&self, v: &VolumeConfig, id: &str, identity: u64) -> Result<()> {
        self.delete_snapshot_file(v, id, identity)
    }
    fn import_image(
        &self,
        image: &crate::image::ImageConfig,
        source: &std::path::Path,
    ) -> Result<u64> {
        self.import_file_image(image, source)
    }
    fn get_image_path(&self, image: &crate::image::ImageConfig) -> Result<PathBuf> {
        self.image_path(image)
    }
    fn check_image(&self, image: &crate::image::ImageConfig) -> Result<VolumeStat> {
        self.check_file_image(image)
    }
    fn delete_image(&self, image: &crate::image::ImageConfig) -> Result<()> {
        self.delete_file_image(image)
    }

    fn get_volume_path(&self, volume: &VolumeConfig) -> Result<PathBuf> {
        metadata::validate_name("volume", &volume.name)?;
        if volume.repo != self.repo.name || volume.repo_id != self.repo.id {
            return Err(Error::Resource(format!(
                "volume {:?} does not belong to this repo",
                volume.name
            )));
        }
        Ok(self
            .repo
            .path
            .join("volumes")
            .join(format!("{}.raw", volume.name)))
    }

    fn create_volume(&self, volume: &VolumeConfig) -> Result<VolumeIdentity> {
        self.create_file_volume(volume, None)
            .map(|inode| VolumeIdentity {
                inode,
                zfs_guid: None,
            })
    }
    fn volume_exists(&self, volume: &VolumeConfig) -> Result<bool> {
        let path = self.get_volume_path(volume)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error("inspect volume payload", &path, e)),
        }
    }
    fn volume_marker_path(&self, volume: &VolumeConfig) -> Result<PathBuf> {
        Ok(self.get_volume_path(volume)?.with_extension("chadm.toml"))
    }
    fn sync_volume_storage(&self) -> Result<()> {
        metadata::sync_directory(&self.repo.path.join("volumes"))
    }

    fn clone_image(
        &self,
        volume: &VolumeConfig,
        image: &crate::image::ImageConfig,
        source: &Path,
    ) -> Result<VolumeIdentity> {
        self.create_file_volume(volume, Some((image, source)))
            .map(|inode| VolumeIdentity {
                inode,
                zfs_guid: None,
            })
    }

    fn check_volume(&self, volume: &VolumeConfig) -> Result<VolumeStat> {
        if volume.zfs_guid.is_some() {
            return Err(Error::Resource("file volume has a ZFS identity".into()));
        }
        let path = self.get_volume_path(volume)?;
        let file = metadata::open_regular(&path, false)?;
        check_file_with(&file, &volume.id, ID_ATTRIBUTE)?;
        let stat = file
            .metadata()
            .map_err(|source| io_error("inspect raw volume", &path, source))?;
        if stat.ino() != volume.inode || stat.len() != volume.size_bytes {
            return Err(Error::Resource(format!("volume {:?} file identity or size does not match metadata; refusing to treat it as managed", volume.name)));
        }
        let allocated_bytes = stat
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| Error::Resource("allocated byte count overflow".into()))?;
        Ok(VolumeStat { allocated_bytes })
    }

    fn delete_volume(&self, volume: &VolumeConfig) -> Result<()> {
        self.check_volume(volume)?;
        let path = self.get_volume_path(volume)?;
        fs::remove_file(&path)
            .map_err(|source| io_error("delete managed raw volume", &path, source))
        // Caller syncs storage after unlink; on failure its tombstone remains for inspection.
    }
}

impl FileDriver {
    fn create_file_volume(
        &self,
        volume: &VolumeConfig,
        source: Option<(&crate::image::ImageConfig, &Path)>,
    ) -> Result<u64> {
        crate::volume::validate_size(volume.size_bytes)?;
        let path = self.get_volume_path(volume)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| {
                io_error("create raw volume (refusing to overwrite)", &path, source)
            })?;
        let result = (|| {
            if let Some((image, source)) = source {
                if volume.size_bytes < image.virtual_size_bytes {
                    return Err(Error::Resource(
                        "root volume is smaller than its source image".into(),
                    ));
                }
                let input = metadata::open_regular(source, false)?;
                let stat = input
                    .metadata()
                    .map_err(|error| io_error("inspect clone source", source, error))?;
                check_file_with(&input, &image.id, c"user.chadm.image-id")?;
                if stat.ino() != image.inode
                    || stat.len() != image.virtual_size_bytes
                    || stat.mode() & 0o222 != 0
                {
                    return Err(Error::Resource(
                        "source image changed; refusing clone".into(),
                    ));
                }
                crate::image_input::copy_sparse(&input, &mut file)
                    .map_err(|error| io_error("clone image into root volume", &path, error))?;
            }
            // set_len extends the logical length without writing or preallocating blocks.
            file.set_len(volume.size_bytes)
                .map_err(|source| io_error("size raw volume", &path, source))?;
            mark_file_with(&file, &volume.id, ID_ATTRIBUTE)?;
            file.sync_all()
                .map_err(|source| io_error("sync raw volume", &path, source))?;
            let inode = file
                .metadata()
                .map_err(|source| io_error("inspect raw volume", &path, source))?
                .ino();
            metadata::sync_directory(&self.repo.path.join("volumes"))?;
            Ok(inode)
        })();
        if let Err(error) = result {
            drop(file);
            return match fs::remove_file(&path) {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Resource(format!(
                    "{error}; cannot remove incomplete raw volume {path:?}: {cleanup}"
                ))),
            };
        }
        result
    }
}
