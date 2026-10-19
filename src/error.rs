use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Resource(String),
    #[error("cannot {action} {path:?}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("invalid configuration in {path:?}: TOML syntax or schema error (contents omitted)")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
