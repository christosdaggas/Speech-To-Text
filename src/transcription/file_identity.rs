use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Detect replacement or modification without rehashing large model weights per take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileIdentity {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = path.metadata()?;
        if !metadata.is_file() {
            return Err(std::io::Error::other("Model/runtime path is not a file"));
        }
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}
