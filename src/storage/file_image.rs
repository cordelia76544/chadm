use super::file::{check_file_with, mark_file_with, FileDriver};
use super::VolumeStat;
use crate::error::{Error, Result};
use crate::image::{ImageConfig, ImageFormat};
use crate::metadata::{self, io_error};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const ID_ATTRIBUTE: &std::ffi::CStr = c"user.chadm.image-id";

impl FileDriver {
    pub(crate) fn image_path(&self, image: &ImageConfig) -> Result<PathBuf> {
        metadata::validate_name("image", &image.name)?;
        if image.repo != self.repo.name || image.repo_id != self.repo.id {
            return Err(Error::Resource(format!(
                "image {:?} does not belong to this repo",
                image.name
            )));
        }
        Ok(self
            .repo
            .path
            .join("images")
            .join(format!("{}.raw", image.name)))
    }

    pub(crate) fn import_file_image(&self, image: &ImageConfig, source: &Path) -> Result<u64> {
        let path = self.image_path(image)?;
        let mut output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| io_error("create raw image (refusing to overwrite)", &path, error))?;
        let inode = output
            .metadata()
            .map_err(|error| io_error("inspect new image", &path, error))?
            .ino();
        let result = (|| {
            mark_file_with(&output, &image.id, ID_ATTRIBUTE)?;
            match image.source_format {
                ImageFormat::Raw => {
                    let input = metadata::open_regular(source, false)?;
                    crate::image_input::copy_sparse(&input, &mut output)
                        .map_err(|error| io_error("copy raw image", &path, error))?;
                }
                ImageFormat::Qcow2 => {
                    let program =
                        crate::command::executable(Path::new("qemu-img")).ok_or_else(|| {
                            Error::Resource(
                                "qemu-img was not found; it is required to convert qcow2 images"
                                    .into(),
                            )
                        })?;
                    output
                        .set_len(image.virtual_size_bytes)
                        .map_err(|error| io_error("size conversion target", &path, error))?;
                    let args = vec![
                        "convert".into(),
                        "-q".into(),
                        "-n".into(),
                        "-f".into(),
                        "qcow2".into(),
                        "-O".into(),
                        "raw".into(),
                        source.as_os_str().into(),
                        path.as_os_str().into(),
                    ];
                    crate::command::convert_image(&program, &args).map_err(|error| {
                        Error::Resource(format!("qcow2 conversion failed: {error}"))
                    })?;
                }
            }
            let stat = output
                .metadata()
                .map_err(|error| io_error("inspect imported image", &path, error))?;
            if stat.len() != image.virtual_size_bytes {
                return Err(Error::Resource(
                    "imported raw image size does not match virtual size".into(),
                ));
            }
            check_file_with(&output, &image.id, ID_ATTRIBUTE)?;
            output
                .set_permissions(fs::Permissions::from_mode(0o400))
                .map_err(|error| io_error("make base image read-only", &path, error))?;
            output
                .sync_all()
                .map_err(|error| io_error("sync base image", &path, error))?;
            let mut completed = image.clone();
            completed.inode = inode;
            self.check_file_image(&completed)?;
            metadata::sync_directory(&self.repo.path.join("images"))?;
            Ok(inode)
        })();
        if let Err(error) = result {
            // The just-created descriptor is the ownership evidence, even if
            // setting the xattr failed. Never unlink a replacement or an alias.
            let cleanup = (|| {
                let file = metadata::open_regular(&path, false)?;
                if file
                    .metadata()
                    .map_err(|error| io_error("inspect incomplete image", &path, error))?
                    .ino()
                    != inode
                {
                    return Err(Error::Resource(
                        "incomplete image was replaced; refusing cleanup".into(),
                    ));
                }
                fs::remove_file(&path)
                    .map_err(|error| io_error("remove incomplete image", &path, error))?;
                metadata::sync_directory(&self.repo.path.join("images"))
            })();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(Error::Resource(format!(
                    "{error}; rollback incomplete: {cleanup}; image intent must be retained"
                ))),
            };
        }
        result
    }

    pub(crate) fn check_file_image(&self, image: &ImageConfig) -> Result<VolumeStat> {
        let path = self.image_path(image)?;
        let file = metadata::open_regular(&path, false)?;
        check_file_with(&file, &image.id, ID_ATTRIBUTE)?;
        let stat = file
            .metadata()
            .map_err(|error| io_error("inspect base image", &path, error))?;
        if stat.ino() != image.inode
            || stat.len() != image.virtual_size_bytes
            || stat.mode() & 0o222 != 0
        {
            return Err(Error::Resource(format!(
                "image {:?} file identity, size or read-only permissions do not match metadata",
                image.name
            )));
        }
        Ok(VolumeStat {
            allocated_bytes: stat
                .blocks()
                .checked_mul(512)
                .ok_or_else(|| Error::Resource("allocated byte count overflow".into()))?,
        })
    }

    pub(crate) fn delete_file_image(&self, image: &ImageConfig) -> Result<()> {
        self.check_file_image(image)?;
        let path = self.image_path(image)?;
        fs::remove_file(&path).map_err(|error| io_error("delete managed base image", &path, error))
    }
}
