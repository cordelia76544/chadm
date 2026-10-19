use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_CONFIG: &str = "/etc/chadm/config.toml";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostConfig {
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    /// Legacy input only; firmware boot always uses data_dir/CLOUDHV.fd.
    #[serde(skip_serializing)]
    pub firmware: Option<PathBuf>,
    pub cloud_hypervisor: PathBuf,
    pub default_bridge: Option<String>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            data_dir: "/var/lib/chadm".into(),
            runtime_dir: "/run/chadm".into(),
            firmware: None,
            default_bridge: None,
            cloud_hypervisor: "cloud-hypervisor".into(),
        }
    }
}

impl HostConfig {
    /// Only the missing implicit default is allowed. Help/version never call this.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = explicit.unwrap_or_else(|| Path::new(DEFAULT_CONFIG));
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(Error::Io {
                    action: "read",
                    path: path.into(),
                    source,
                })
            }
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Config(format!(
                "{path:?} must be a regular file, not a symlink"
            )));
        }
        let mut contents = String::new();
        File::open(path)
            .and_then(|file| {
                file.take(MAX_CONFIG_BYTES + 1)
                    .read_to_string(&mut contents)
            })
            .map_err(|source| Error::Io {
                action: "read",
                path: path.into(),
                source,
            })?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(Error::Config("configuration exceeds 64 KiB".into()));
        }
        let config: Self = toml::from_str(&contents).map_err(|source| Error::Toml {
            path: path.into(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(bridge) = &self.default_bridge {
            crate::instance_config::bridge_name(bridge)?;
        }
        for (label, path) in [
            ("data_dir", &self.data_dir),
            ("runtime_dir", &self.runtime_dir),
        ] {
            validate_absolute(label, path)?;
            if path == Path::new("/") {
                return Err(Error::Config(format!("{label} must not be /")));
            }
        }
        if self.data_dir.starts_with(&self.runtime_dir)
            || self.runtime_dir.starts_with(&self.data_dir)
        {
            return Err(Error::Config(
                "data_dir and runtime_dir must not overlap".into(),
            ));
        }
        if self.cloud_hypervisor != Path::new("cloud-hypervisor") {
            validate_absolute("cloud_hypervisor", &self.cloud_hypervisor)?;
        }
        Ok(())
    }

    pub fn firmware_path(&self) -> PathBuf {
        self.data_dir.join("CLOUDHV.fd")
    }

    pub fn layout(&self) -> Layout {
        Layout {
            instances: self.data_dir.join("instances"),
            volumes: self.data_dir.join("volumes"),
            images: self.data_dir.join("images"),
            repos: self.data_dir.join("repos"),
            runtime: self.runtime_dir.clone(),
        }
    }
}

fn validate_absolute(label: &str, path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(Error::Config(format!(
            "{label} must be an absolute path without '..'"
        )));
    }
    Ok(())
}

/// Metadata directories; storage payloads belong to their repo, not these paths.
#[derive(Debug, Clone)]
pub struct Layout {
    pub instances: PathBuf,
    pub volumes: PathBuf,
    pub images: PathBuf,
    pub repos: PathBuf,
    pub runtime: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_roundtrip() {
        let config = HostConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(
            config.layout().instances,
            Path::new("/var/lib/chadm/instances")
        );
        let encoded = toml::to_string(&config).unwrap();
        let decoded: HostConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.data_dir, config.data_dir);
    }

    #[test]
    fn rejects_unsafe_paths_and_unknown_keys() {
        for path in ["relative", "/", "/var/../tmp"] {
            let config = HostConfig {
                data_dir: path.into(),
                ..HostConfig::default()
            };
            assert!(config.validate().is_err(), "{path}");
        }
        let config = HostConfig {
            runtime_dir: "/var/lib/chadm/run".into(),
            ..HostConfig::default()
        };
        assert!(config.validate().is_err());
        for name in ["", "../bridge", "bridge-name-too-long", "br 0"] {
            let config = HostConfig {
                default_bridge: Some(name.into()),
                ..HostConfig::default()
            };
            assert!(config.validate().is_err());
        }
        assert!(toml::from_str::<HostConfig>("firmare = '/tmp/fw'").is_err());
    }
}
