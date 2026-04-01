use alloc::{string::String, vec::Vec};

use libakarin_dyld::{ImageParserError, MachOImage};
use log::info;
use thiserror::Error;

use crate::{
    config::eval::Evaluator,
    fs::{FileSystem, path::Path, simple::SimpleFileSystem},
    loader::loadable::Loadable,
};

#[derive(Debug, Error)]
pub enum BootError {
    #[error("Invalid kernel path: {0}")]
    InvalidPath(String),
    #[error("Failed to read kernel file: {0}")]
    FileReadError(String),
    #[error("No kernel specified")]
    NoKernelSpecified,
    #[error("No bootstrap program specified")]
    NoBootstrapSpecified,
    #[error("Invalid kernel format: {0}")]
    InvaildKernelFormat(#[from] ImageParserError),
    #[error("Loadable error: {0}")]
    Loadable(#[from] crate::loader::loadable::LoadableError),
}

pub struct Session<'a> {
    pub fs: &'a mut SimpleFileSystem,
    pub evaluator: Evaluator,
    pub kernel_path: Option<String>,
    pub kernel_args: Vec<String>,
    pub bootstrap_path: Option<String>,
}

impl<'a> Session<'a> {
    pub fn new(fs: &'a mut SimpleFileSystem, evaluator: Evaluator) -> Self {
        Self {
            fs,
            evaluator,
            kernel_path: None,
            kernel_args: Vec::new(),
            bootstrap_path: None,
        }
    }

    pub fn set_kernel(&mut self, path: String, args: Vec<String>) {
        self.kernel_path = Some(path);
        self.kernel_args = args;
    }

    pub fn set_bootstrap(&mut self, path: String) {
        self.bootstrap_path = Some(path);
    }

    pub fn boot(&mut self) -> Result<(), BootError> {
        let path_str = self
            .kernel_path
            .as_ref()
            .ok_or(BootError::NoKernelSpecified)?;
        let bootstrap_str = self
            .bootstrap_path
            .as_ref()
            .ok_or(BootError::NoBootstrapSpecified)?;

        let kernel_path =
            Path::try_from(path_str).ok_or(BootError::InvalidPath(path_str.clone()))?;
        let bootstrap_path =
            Path::try_from(bootstrap_str).ok_or(BootError::InvalidPath(bootstrap_str.clone()))?;

        let kernel_content = self
            .fs
            .read(kernel_path)
            .map_err(|_| BootError::FileReadError(path_str.clone()))?;
        let bootstrap_content = self
            .fs
            .read(bootstrap_path)
            .map_err(|_| BootError::FileReadError(bootstrap_str.clone()))?;

        let image = MachOImage::from_bytes(kernel_content)?;
        let mut loadable = Loadable::new(image)?;
        loadable.load()?;

        info!("Kernel loaded successfully.");

        loadable.enter_kernel(self.kernel_args.clone(), bootstrap_content)?;

        Ok(())
    }
}
