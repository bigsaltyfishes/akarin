use alloc::{
    string::{String, ToString},
    vec::Vec,
};

use uefi::{CStr16, proto::media::fs::SimpleFileSystem as UefiSimpleFileSystem};

use crate::fs::{FileSystem, FileType, FsError, path::Path};

#[derive(Debug)]
pub struct SimpleFileSystem {
    inner: uefi::fs::FileSystem,
}

impl SimpleFileSystem {
    /// Opens the EFI Simple File System protocol and returns an EfiFileSystem
    /// instance.
    pub fn open() -> Option<Self> {
        let sfs_handle = uefi::boot::get_handle_for_protocol::<UefiSimpleFileSystem>().ok()?;
        let sfs_proto =
            uefi::boot::open_protocol_exclusive::<UefiSimpleFileSystem>(sfs_handle).ok()?;

        let fs = uefi::fs::FileSystem::new(sfs_proto);

        Some(Self { inner: fs })
    }
}

impl FileSystem for SimpleFileSystem {
    fn read<T>(&mut self, path: T) -> Result<Vec<u8>, FsError>
    where
        T: AsRef<Path>,
    {
        let p = path.as_ref();
        let mut p_str = String::new();
        if p.is_absolute() {
            p_str.push('\\');
        }

        let comp = p.components();
        for (i, comp) in comp.iter().enumerate() {
            if i > 0 {
                p_str.push('\\');
            }
            p_str.push_str(comp);
        }

        let mut buffer = [0u16; 256];
        let cstr = CStr16::from_str_with_buf(&p_str, &mut buffer)
            .map_err(|_| super::FsError::InvalidPath)?;
        let content = self.inner.read(cstr).map_err(|e| match e {
            uefi::fs::Error::Io(io) => match io.context {
                uefi::fs::IoErrorContext::NotAFile => FsError::TargetIsDirectory,
                uefi::fs::IoErrorContext::OpenError => FsError::NotFound,
                _ => FsError::IoError,
            },
            uefi::fs::Error::Path(_) => FsError::InvalidPath,
            _ => FsError::IoError,
        })?;

        Ok(content)
    }

    fn write<T, D>(&mut self, path: T, data: &[u8]) -> Result<(), FsError>
    where
        T: AsRef<Path>,
        D: AsRef<[u8]>,
    {
        let p = path.as_ref();
        let mut p_str = String::new();
        if p.is_absolute() {
            p_str.push('\\');
        }

        let comp = p.components();
        for (i, comp) in comp.iter().enumerate() {
            if i > 0 {
                p_str.push('\\');
            }
            p_str.push_str(comp);
        }

        let mut buffer = [0u16; 256];
        let cstr = CStr16::from_str_with_buf(&p_str, &mut buffer)
            .map_err(|_| super::FsError::InvalidPath)?;

        self.inner.write(cstr, data).map_err(|e| match e {
            uefi::fs::Error::Io(io) => match io.context {
                uefi::fs::IoErrorContext::NotAFile => FsError::TargetIsDirectory,
                uefi::fs::IoErrorContext::OpenError => FsError::NotFound,
                _ => FsError::IoError,
            },
            uefi::fs::Error::Path(_) => FsError::InvalidPath,
            _ => FsError::IoError,
        })?;

        Ok(())
    }

    fn list_dir<T>(&mut self, path: T) -> Result<Vec<FileType>, FsError>
    where
        T: AsRef<Path>,
    {
        let p = path.as_ref();
        let mut p_str = String::new();
        if p.is_absolute() {
            p_str.push('\\');
        }

        let comp = p.components();
        for (i, comp) in comp.iter().enumerate() {
            if i > 0 {
                p_str.push('\\');
            }
            p_str.push_str(comp);
        }

        let mut buffer = [0u16; 256];
        let cstr = CStr16::from_str_with_buf(&p_str, &mut buffer)
            .map_err(|_| super::FsError::InvalidPath)?;

        let entries = self.inner.read_dir(cstr).map_err(|e| match e {
            uefi::fs::Error::Io(io) => match io.context {
                uefi::fs::IoErrorContext::NotADirectory => FsError::TargetIsFile,
                uefi::fs::IoErrorContext::OpenError => FsError::NotFound,
                _ => FsError::IoError,
            },
            uefi::fs::Error::Path(_) => FsError::InvalidPath,
            _ => FsError::IoError,
        })?;

        let mut file_types = Vec::new();
        for entry in entries {
            let ent = entry.map_err(|_| FsError::IoError)?;
            if ent.is_directory() {
                file_types.push(FileType::Directory {
                    name: ent.file_name().to_string(),
                });
            } else {
                file_types.push(FileType::File {
                    name: ent.file_name().to_string(),
                    size: ent.file_size() as usize,
                });
            }
        }

        Ok(file_types)
    }
}
