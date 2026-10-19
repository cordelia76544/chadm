//! Small Linux metadata primitives. All callers hold the global data-directory lock.
//! Paths are protected by ownership and permissions, not merely lexical prefix checks.
use crate::error::{Error, Result};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path};

const O_NOFOLLOW: i32 = 0x20000;
const O_NONBLOCK: i32 = 0x800;
const LOCK_SH: i32 = 1;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

unsafe extern "C" {
    fn geteuid() -> u32;
    fn flock(fd: i32, operation: i32) -> i32;
}

pub(crate) fn uid() -> u32 {
    // SAFETY: geteuid has no arguments or preconditions on Linux.
    unsafe { geteuid() }
}

pub fn io_error(action: &'static str, path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        action,
        path: path.into(),
        source,
    }
}

pub fn validate_path(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| Error::Resource("paths must be valid UTF-8".into()))?;
    if !path.is_absolute()
        || path == Path::new("/")
        || path.components().any(|c| matches!(c, Component::ParentDir))
        || text.chars().any(char::is_control)
    {
        return Err(Error::Resource(format!("unsafe path {path:?}; use an absolute non-root path without '..' or control characters")));
    }
    Ok(())
}

/// Refuse symlinks anywhere in the path and untrusted writable ancestors.
/// A root-owned sticky directory such as /tmp is allowed only as an ancestor.
pub fn directory(path: &Path, create: bool) -> Result<()> {
    validate_path(path)?;
    // User namespaces may expose host root as an unmapped UID. Use the immutable
    // filesystem root's owner, never a globally special-cased "nobody" UID.
    let root_owner = fs::symlink_metadata("/")
        .map_err(|source| io_error("inspect filesystem root", Path::new("/"), source))?
        .uid();
    let mut current = Path::new("/").to_path_buf();
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        current.push(component);
        let info = match fs::symlink_metadata(&current) {
            Ok(info) => info,
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .mode(0o700)
                    .create(&current)
                    .map_err(|source| io_error("create directory", &current, source))?;
                fs::symlink_metadata(&current)
                    .map_err(|source| io_error("read directory", &current, source))?
            }
            Err(source) => return Err(io_error("read directory", &current, source)),
        };
        let trusted_owner = info.uid() == uid() || info.uid() == root_owner;
        let sticky_ancestor =
            current != path && info.uid() == root_owner && info.mode() & 0o1000 != 0;
        if !info.is_dir() || !trusted_owner || (info.mode() & 0o022 != 0 && !sticky_ancestor) {
            return Err(Error::Resource(format!("unsafe directory {current:?}; require a real directory owned by root or the current user, without group/other write access")));
        }
    }
    Ok(())
}

fn validate_file(file: &File, path: &Path) -> Result<()> {
    let info = file
        .metadata()
        .map_err(|source| io_error("inspect file", path, source))?;
    if !info.is_file() || info.uid() != uid() || info.mode() & 0o022 != 0 || info.nlink() != 1 {
        return Err(Error::Resource(format!("unsafe managed file {path:?}; require a regular file owned by the current user, without writable aliases")));
    }
    Ok(())
}

pub fn validate_name(resource: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 63
        || !name.as_bytes()[0].is_ascii_lowercase()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(Error::Resource(format!(
            "invalid {resource} name {name:?}; use [a-z][a-z0-9-]{{0,62}}"
        )));
    }
    Ok(())
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn open_regular(path: &Path, write: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .map_err(|source| io_error("open managed file", path, source))?;
    validate_file(&file, path)?;
    Ok(file)
}

pub fn read(path: &Path) -> Result<String> {
    let file = open_regular(path, false)?;
    let mut text = String::new();
    file.take(65537)
        .read_to_string(&mut text)
        .map_err(|source| io_error("read", path, source))?;
    if text.len() > 65536 {
        return Err(Error::Resource(format!("metadata {path:?} exceeds 64 KiB")));
    }
    Ok(text)
}

pub fn write_new(path: &Path, text: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(|source| io_error("create", path, source))?;
    if let Err(source) = file
        .write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let original = io_error("write", path, source);
        drop(file);
        return match fs::remove_file(path) {
            Ok(()) => Err(original),
            Err(cleanup) => Err(Error::Resource(format!(
                "{original}; cannot remove incomplete file {path:?}: {cleanup}"
            ))),
        };
    }
    Ok(())
}

pub fn create_directory(path: &Path) -> Result<()> {
    DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|source| io_error("create directory", path, source))
}

pub fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| io_error("sync directory", path, source))
}

/// Explicit unlock prevents incidental forked descriptors from prolonging the lock.
/// On process crash the kernel releases it when the descriptors close.
pub struct Lock {
    _file: File,
}

impl Drop for Lock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains live until after this destructor returns.
        // LOCK_UN is valid even if a previous unlock already released the lock.
        let _ = unsafe { flock(self._file.as_raw_fd(), LOCK_UN) };
    }
}

impl Lock {
    pub fn acquire(data_dir: &Path, write: bool) -> Result<Option<Self>> {
        validate_path(data_dir)?;
        if !write {
            match fs::symlink_metadata(data_dir) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(source) => return Err(io_error("inspect", data_dir, source)),
                Ok(_) => (),
            }
        }
        directory(data_dir, write)?;
        let path = data_dir.join(".lock");
        let file = if write {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(O_NOFOLLOW)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(O_NOFOLLOW)
                        .open(&path)
                        .map_err(|source| io_error("open lock", &path, source))?
                }
                Err(source) => return Err(io_error("create lock", &path, source)),
            }
        } else {
            OpenOptions::new()
                .read(true)
                .custom_flags(O_NOFOLLOW | O_NONBLOCK)
                .open(&path)
                .map_err(|source| {
                    io_error(
                        "open lock (run repo create to initialize metadata)",
                        &path,
                        source,
                    )
                })?
        };
        validate_file(&file, &path)?;
        // SAFETY: file owns a live fd; the Linux flock flags are valid constants.
        let result = unsafe {
            flock(
                file.as_raw_fd(),
                (if write { LOCK_EX } else { LOCK_SH }) | LOCK_NB,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(Error::Resource(
                    "another chadm command is using this data directory; retry later".into(),
                ));
            }
            return Err(io_error("lock", &path, error));
        }
        Ok(Some(Self { _file: file }))
    }
}

pub fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    let path = Path::new("/dev/urandom");
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|source| io_error("read random bytes", path, source))?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut id = String::with_capacity(32);
    for byte in bytes {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 15) as usize] as char);
    }
    Ok(id)
}
