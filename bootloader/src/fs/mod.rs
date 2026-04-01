use alloc::{string::String, vec::Vec};

use thiserror::Error;

use crate::fs::path::Path;

pub mod path;
pub mod simple;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("File or directory not found")]
    NotFound,
    #[error("I/O error occurred")]
    IoError,
    #[error("Invalid path specified")]
    InvalidPath,
    #[error("Target is a directory")]
    TargetIsDirectory,
    #[error("Target is a file")]
    TargetIsFile,
}

#[derive(Debug, Clone)]
pub enum FileType {
    File { name: String, size: usize },
    Directory { name: String },
}

pub trait FileSystem {
    fn read<T>(&mut self, path: T) -> Result<Vec<u8>, FsError>
    where
        T: AsRef<Path>;

    fn write<T, D>(&mut self, path: T, data: &[u8]) -> Result<(), FsError>
    where
        T: AsRef<Path>,
        D: AsRef<[u8]>;

    fn list_dir<T>(&mut self, path: T) -> Result<Vec<FileType>, FsError>
    where
        T: AsRef<Path>;
}
