pub mod file;
pub(crate) mod file_image;
mod file_snapshot;
pub mod zfs;

use crate::error::Result;
use crate::volume::VolumeConfig;
use std::path::{Path, PathBuf};

pub struct VolumeStat {
    pub allocated_bytes: u64,
}
pub struct VolumeIdentity {
    pub inode: u64,
    pub zfs_guid: Option<u64>,
}
pub(crate) fn driver(repo: crate::repo::RepoConfig) -> Result<Box<dyn StorageDriver>> {
    match repo.driver {
        crate::repo::Driver::File => Ok(Box::new(file::FileDriver::new(repo)?)),
        crate::repo::Driver::Zfs => Ok(Box::new(zfs::ZfsDriver::new(repo)?)),
    }
}

/// Backend payload operations; callers own the metadata transaction and global lock.
/// Image import and clone are offline; callers verify source ownership.
pub trait StorageDriver {
    fn snapshot_exists(&self, volume: &VolumeConfig, id: &str) -> Result<bool>;
    fn snapshot_volume(&self, volume: &VolumeConfig, id: &str) -> Result<u64>;
    fn check_snapshot(
        &self,
        volume: &VolumeConfig,
        id: &str,
        identity: u64,
        restoring: bool,
    ) -> Result<()>;
    fn restore_snapshot(&self, volume: &VolumeConfig, id: &str, identity: u64) -> Result<()>;
    fn delete_snapshot(&self, volume: &VolumeConfig, id: &str, identity: u64) -> Result<()>;
    /// Create a new payload; return its persistent file identity.
    fn import_image(&self, image: &crate::image::ImageConfig, source: &Path) -> Result<u64>;
    fn get_image_path(&self, image: &crate::image::ImageConfig) -> Result<PathBuf>;
    fn check_image(&self, image: &crate::image::ImageConfig) -> Result<VolumeStat>;
    fn delete_image(&self, image: &crate::image::ImageConfig) -> Result<()>;
    fn clone_image(
        &self,
        volume: &VolumeConfig,
        image: &crate::image::ImageConfig,
        source: &Path,
    ) -> Result<VolumeIdentity>;
    fn create_volume(&self, volume: &VolumeConfig) -> Result<VolumeIdentity>;
    fn volume_exists(&self, volume: &VolumeConfig) -> Result<bool>;
    fn volume_marker_path(&self, volume: &VolumeConfig) -> Result<PathBuf>;
    fn sync_volume_storage(&self) -> Result<()>;
    fn delete_volume(&self, volume: &VolumeConfig) -> Result<()>;
    fn get_volume_path(&self, volume: &VolumeConfig) -> Result<PathBuf>;
    fn check_volume(&self, volume: &VolumeConfig) -> Result<VolumeStat>;
}
