use super::{
    file::{check_file_with, mark_file_with, FileDriver},
    StorageDriver,
};
use crate::{
    error::{Error, Result},
    metadata::{self, io_error},
    volume::VolumeConfig,
};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

const ATTRIBUTE: &std::ffi::CStr = c"user.chadm.snapshot-id";
impl FileDriver {
    pub(crate) fn snapshot_file_exists(&self, v: &VolumeConfig, id: &str) -> Result<bool> {
        let path = self.snapshot_path(v, id)?;
        metadata::directory(&self.repo.path.join("volumes"), false)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_error("inspect file snapshot", &path, e)),
        }
    }
    fn snapshot_path(&self, v: &VolumeConfig, id: &str) -> Result<PathBuf> {
        self.get_volume_path(v)?;
        if !metadata::valid_id(id) {
            return Err(Error::Resource("invalid snapshot generation".into()));
        }
        Ok(self
            .repo
            .path
            .join("volumes")
            .join(format!(".chadm-snapshot-{id}-{}.raw", v.id)))
    }
    fn checked_live_file(&self, v: &VolumeConfig, write: bool) -> Result<File> {
        self.check_volume(v)?;
        let path = self.get_volume_path(v)?;
        let file = metadata::open_regular(&path, write)?;
        let m = file
            .metadata()
            .map_err(|e| io_error("inspect snapshot volume", &path, e))?;
        check_file_with(&file, &v.id, c"user.chadm.volume-id")?;
        if m.ino() != v.inode || m.len() != v.size_bytes {
            return Err(Error::Resource(
                "volume identity changed before snapshot operation".into(),
            ));
        }
        Ok(file)
    }
    pub(crate) fn snapshot_file(&self, v: &VolumeConfig, id: &str) -> Result<u64> {
        let input = self.checked_live_file(v, false)?;
        input
            .sync_all()
            .map_err(|e| Error::Resource(format!("sync volume before snapshot: {e}")))?;
        let path = self.snapshot_path(v, id)?;
        let mut output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0x20000)
            .open(&path)
            .map_err(|e| io_error("create file snapshot", &path, e))?;
        // Leave an interrupted payload as evidence; the enclosing manifest reserves its volume.
        mark_file_with(&output, id, ATTRIBUTE)?;
        crate::image_input::copy_sparse(&input, &mut output)
            .map_err(|e| io_error("reflink/copy volume snapshot", &path, e))?;
        output
            .set_permissions(fs::Permissions::from_mode(0o400))
            .map_err(|e| io_error("protect file snapshot", &path, e))?;
        output
            .sync_all()
            .map_err(|e| io_error("sync file snapshot", &path, e))?;
        metadata::sync_directory(&self.repo.path.join("volumes"))?;
        Ok(output
            .metadata()
            .map_err(|e| io_error("inspect file snapshot", &path, e))?
            .ino())
    }
    pub(crate) fn checked_snapshot_file(
        &self,
        v: &VolumeConfig,
        id: &str,
        identity: u64,
    ) -> Result<File> {
        let path = self.snapshot_path(v, id)?;
        let file = metadata::open_regular(&path, false)?;
        let m = file
            .metadata()
            .map_err(|e| io_error("inspect file snapshot", &path, e))?;
        check_file_with(&file, id, ATTRIBUTE)?;
        if m.ino() != identity || m.len() != v.size_bytes || m.mode() & 0o222 != 0 {
            return Err(Error::Resource(
                "file snapshot identity/size/permissions changed".into(),
            ));
        }
        Ok(file)
    }
    pub(crate) fn restore_snapshot_file(
        &self,
        v: &VolumeConfig,
        id: &str,
        identity: u64,
    ) -> Result<()> {
        let input = self.checked_snapshot_file(v, id, identity)?;
        let output = self.checked_live_file(v, true)?;
        use std::os::fd::AsRawFd;
        unsafe extern "C" {
            fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
            fn fallocate(fd: i32, mode: i32, offset: i64, length: i64) -> i32;
        }
        // SAFETY: both fds refer to verified owned regular files. FICLONE keeps
        // the destination inode and its ownership xattr; fallback never truncates.
        if unsafe { ioctl(output.as_raw_fd(), 0x40049409, input.as_raw_fd()) } == 0 {
            return output
                .sync_all()
                .map_err(|e| Error::Resource(format!("sync reflink restore: {e}")));
        }
        // Never truncate or replace the live inode: a partial restore must remain
        // identifiable and retryable. The manifest blocks boot until all disks finish.
        let mut buffer = vec![0u8; 1024 * 1024];
        let mut offset = 0;
        while offset < v.size_bytes {
            let count = (v.size_bytes - offset).min(buffer.len() as u64) as usize;
            input
                .read_exact_at(&mut buffer[..count], offset)
                .map_err(|e| Error::Resource(format!("read snapshot restore data: {e}")))?;
            // SAFETY: the pinned fd is the verified owned live file, ranges stay
            // inside its unchanged i64-sized capacity. PUNCH_HOLE|KEEP_SIZE turns
            // zero ranges into sparse holes without changing inode or length.
            let punched = buffer[..count].iter().all(|b| *b == 0)
                && unsafe { fallocate(output.as_raw_fd(), 0x03, offset as i64, count as i64) } == 0;
            if !punched {
                output.write_all_at(&buffer[..count], offset).map_err(|e| {
                    Error::Resource(format!(
                        "restore file snapshot: {e}; keep the restore manifest and retry"
                    ))
                })?;
            }
            offset += count as u64;
        }
        output
            .sync_all()
            .map_err(|e| Error::Resource(format!("sync restored volume: {e}")))
    }
    pub(crate) fn delete_snapshot_file(
        &self,
        v: &VolumeConfig,
        id: &str,
        identity: u64,
    ) -> Result<()> {
        self.checked_snapshot_file(v, id, identity)?;
        let path = self.snapshot_path(v, id)?;
        fs::remove_file(&path).map_err(|e| io_error("delete owned file snapshot", &path, e))?;
        metadata::sync_directory(&self.repo.path.join("volumes"))
    }
}
