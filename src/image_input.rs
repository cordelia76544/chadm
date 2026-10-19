//! Offline input snapshot and the small qcow2 header checks needed before conversion.
//! This is not a qcow2 reader: qemu-img handles the actual conversion.
use crate::error::{Error, Result};
use crate::image::ImageFormat;
use crate::metadata::io_error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

unsafe extern "C" {
    fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
}

/// Attempt reflink, then a bounded-memory copy that preserves zero-filled holes.
/// Source must be offline; reflink is a snapshot, fallback copying is not atomic.
pub(crate) fn copy_sparse(source: &File, target: &mut File) -> std::io::Result<()> {
    // SAFETY: both descriptors are live; FICLONE takes a valid source fd.
    let cloned = unsafe { ioctl(target.as_raw_fd(), 0x40049409, source.as_raw_fd()) };
    if cloned == 0 {
        return Ok(());
    }
    target.set_len(0)?;
    target.seek(SeekFrom::Start(0))?;
    let size = source.metadata()?.len();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut offset = 0;
    while offset < size {
        let length = (size - offset).min(buffer.len() as u64) as usize;
        source.read_exact_at(&mut buffer[..length], offset)?;
        if buffer[..length].iter().all(|byte| *byte == 0) {
            target.seek(SeekFrom::Current(length as i64))?;
        } else {
            target.write_all(&buffer[..length])?;
        }
        offset += length as u64;
    }
    target.set_len(size)
}

pub(crate) fn snapshot(path: &Path, target: &Path) -> Result<()> {
    // Input may belong to another user. Never open devices/FIFOs or follow the
    // final symlink; use the opened descriptor for every subsequent read.
    let source = OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800) // O_NOFOLLOW | O_NONBLOCK
        .open(path)
        .map_err(|error| io_error("open local image (symlinks are not accepted)", path, error))?;
    let before = source
        .metadata()
        .map_err(|error| io_error("inspect image input", path, error))?;
    if !before.is_file() || before.len() == 0 || before.len() > i64::MAX as u64 {
        return Err(Error::Resource(
            "image input must be a nonempty regular file; block devices are not accepted".into(),
        ));
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)
        .map_err(|error| io_error("create private image snapshot", target, error))?;
    let result = (|| {
        copy_sparse(&source, &mut output)
            .map_err(|error| io_error("copy offline image input", path, error))?;
        let after = source
            .metadata()
            .map_err(|error| io_error("inspect image input after copy", path, error))?;
        if before.len() != after.len()
            || before.mtime() != after.mtime()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Err(Error::Resource(
                "image input changed during import; provide an offline image".into(),
            ));
        }
        output
            .set_permissions(fs::Permissions::from_mode(0o400))
            .map_err(|error| io_error("protect private image snapshot", target, error))?;
        output
            .sync_all()
            .map_err(|error| io_error("sync private image snapshot", target, error))
    })();
    if let Err(error) = result {
        return match fs::remove_file(target) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(Error::Resource(format!(
                "{error}; incomplete input snapshot retained at {target:?}: {cleanup}"
            ))),
        };
    }
    Ok(())
}

pub(crate) fn inspect(path: &Path) -> Result<(ImageFormat, u64)> {
    let mut file = crate::metadata::open_regular(path, false)?;
    let size = file
        .metadata()
        .map_err(|error| io_error("inspect image snapshot", path, error))?
        .len();
    let mut header = [0u8; 104];
    let count = size.min(header.len() as u64) as usize;
    file.read_exact(&mut header[..count])
        .map_err(|error| io_error("read image header", path, error))?;
    inspect_header(&header[..count], size)
}

fn inspect_header(header: &[u8], file_size: u64) -> Result<(ImageFormat, u64)> {
    let bad = |reason: &str| Error::Resource(format!("cannot import qcow2 image: {reason}"));
    if !header.starts_with(b"QFI\xfb") {
        if header.starts_with(b"QFI\xfa")
            || header.starts_with(b"KDMV")
            || header.starts_with(b"# Disk DescriptorFile")
            || header.starts_with(b"vhdxfile")
            || header.get(64..68) == Some(&[0x7f, 0x10, 0xda, 0xbe])
        {
            return Err(Error::Resource(
                "unsupported image container; only raw and qcow2 are accepted".into(),
            ));
        }
        if file_size == 0 || file_size > i64::MAX as u64 {
            return Err(Error::Resource("invalid raw image size".into()));
        }
        return Ok((ImageFormat::Raw, file_size));
    }
    if header.len() < 72 {
        return Err(bad("truncated header"));
    }
    let u32_at = |offset| {
        u32::from_be_bytes([
            header[offset],
            header[offset + 1],
            header[offset + 2],
            header[offset + 3],
        ])
    };
    let u64_at = |offset| {
        u64::from_be_bytes([
            header[offset],
            header[offset + 1],
            header[offset + 2],
            header[offset + 3],
            header[offset + 4],
            header[offset + 5],
            header[offset + 6],
            header[offset + 7],
        ])
    };
    let version = u32_at(4);
    if version != 2 && version != 3 {
        return Err(bad("only qcow2 versions 2 and 3 are supported"));
    }
    if u64_at(8) != 0 || u32_at(16) != 0 {
        return Err(bad("external backing files are not supported"));
    }
    if u32_at(32) != 0 {
        return Err(bad("encrypted images are not supported"));
    }
    if !(9..=21).contains(&u32_at(20)) {
        return Err(bad("invalid cluster size"));
    }
    let size = u64_at(24);
    if size == 0 || size > i64::MAX as u64 {
        return Err(bad("invalid virtual disk size"));
    }
    if version == 3 {
        if header.len() < 104 {
            return Err(bad("truncated version 3 header"));
        }
        let flags = u64_at(72);
        if flags & 4 != 0 {
            return Err(bad("external data files are not supported"));
        }
        if flags & 3 != 0 {
            return Err(bad(
                "dirty or corrupt image; repair it separately before importing",
            ));
        }
        if flags & !0x1f != 0 {
            return Err(bad("unknown incompatible features"));
        }
        if u32_at(100) < 104 || u32_at(100) as u64 > (1u64 << u32_at(20)) {
            return Err(bad("invalid header length"));
        }
    }
    Ok((ImageFormat::Qcow2, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header() -> [u8; 104] {
        let mut h = [0; 104];
        h[..4].copy_from_slice(b"QFI\xfb");
        h[4..8].copy_from_slice(&3u32.to_be_bytes());
        h[20..24].copy_from_slice(&16u32.to_be_bytes());
        h[24..32].copy_from_slice(&(8u64 << 20).to_be_bytes());
        h[100..104].copy_from_slice(&104u32.to_be_bytes());
        h
    }
    #[test]
    fn checks_headers_without_following_external_references() {
        let h = header();
        assert_eq!(
            inspect_header(&h, 104).unwrap(),
            (ImageFormat::Qcow2, 8 << 20)
        );
        for index in [8, 16, 32, 72] {
            let mut bad = h;
            bad[index] = 1;
            assert!(inspect_header(&bad, 104).is_err());
        }
        let mut external = h;
        external[79] = 4;
        assert!(inspect_header(&external, 104)
            .unwrap_err()
            .to_string()
            .contains("external data"));
        assert!(inspect_header(&h[..71], 71).is_err());
        assert!(inspect_header(&h[..103], 103).is_err());
        assert_eq!(
            inspect_header(b"raw bytes", 123).unwrap(),
            (ImageFormat::Raw, 123)
        );
        assert!(inspect_header(b"KDMV", 123).is_err());
    }
}
