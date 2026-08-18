use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("ext4: {0}")]
    Ext4(#[from] arcbox_ext4::error::FormatError),

    #[error("tar entry {path:?} escapes the image root")]
    PathEscape { path: PathBuf },

    #[error("tar entry {path:?} has an unrepresentable path")]
    BadPath { path: PathBuf },

    #[error(
        "{count} special file(s) in the source tar cannot be represented \
         (device nodes, FIFOs and sockets are unsupported): {sample:?}"
    )]
    SpecialFiles { count: usize, sample: Vec<PathBuf> },

    #[error("{tool} not found on PATH -- install e2fsprogs")]
    ToolMissing { tool: &'static str },

    #[error("{tool} failed ({status}): {stderr}")]
    ToolFailed {
        tool: &'static str,
        status: String,
        stderr: String,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
