use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INVALID_PARAMETER, STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND,
};
use winfsp_wrs::{
    CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights, FileAttributes,
    FileInfo, FileSystemInterface, PSecurityDescriptor, SecurityDescriptor, U16CStr, VolumeInfo,
    WriteMode, u16cstr,
};

use crate::FilePath;

use super::nodes::{
    child_path, dir_info, ensure_mutable_path, file_info, path_from_winfsp, rename_target_path,
};
use super::state::VirtualLaneState;

#[derive(Clone)]
pub(super) struct VirtualLaneFs {
    state: Arc<VirtualLaneState>,
}

impl VirtualLaneFs {
    pub(super) fn new(state: Arc<VirtualLaneState>) -> Self {
        Self { state }
    }
}

pub(super) struct VirtualFileHandle {
    path: Mutex<FilePath>,
    is_dir: bool,
    version: AtomicU64,
    delete_on_close: AtomicBool,
}

impl VirtualFileHandle {
    fn new(path: FilePath, is_dir: bool, version: u64) -> Arc<Self> {
        Arc::new(Self {
            path: Mutex::new(path),
            is_dir,
            version: AtomicU64::new(version),
            delete_on_close: AtomicBool::new(false),
        })
    }

    fn path(&self) -> Result<FilePath, i32> {
        self.path
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|path| path.clone())
    }

    fn set_path(&self, path: FilePath) -> Result<(), i32> {
        self.path
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|mut current| {
                *current = path;
            })
    }
}

impl FileSystemInterface for VirtualLaneFs {
    type FileContext = Arc<VirtualFileHandle>;

    const GET_VOLUME_INFO_DEFINED: bool = true;
    const GET_SECURITY_BY_NAME_DEFINED: bool = true;
    const CREATE_DEFINED: bool = true;
    const OPEN_DEFINED: bool = true;
    const OVERWRITE_DEFINED: bool = true;
    const CLEANUP_DEFINED: bool = true;
    const CLOSE_DEFINED: bool = true;
    const READ_DEFINED: bool = true;
    const WRITE_DEFINED: bool = true;
    const FLUSH_DEFINED: bool = true;
    const GET_FILE_INFO_DEFINED: bool = true;
    const SET_BASIC_INFO_DEFINED: bool = true;
    const SET_FILE_SIZE_DEFINED: bool = true;
    const CAN_DELETE_DEFINED: bool = true;
    const RENAME_DEFINED: bool = true;
    const READ_DIRECTORY_DEFINED: bool = true;
    const GET_SECURITY_DEFINED: bool = true;
    const GET_DIR_INFO_BY_NAME_DEFINED: bool = true;
    const SET_DELETE_DEFINED: bool = true;

    fn get_volume_info(&self) -> Result<VolumeInfo, i32> {
        VolumeInfo::new(1 << 40, 1 << 40, u16cstr!("Lane").as_ustr())
            .map_err(|_| STATUS_INVALID_PARAMETER)
    }

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _find_reparse_point: impl Fn() -> Option<FileAttributes>,
    ) -> Result<(FileAttributes, PSecurityDescriptor, bool), i32> {
        let node = self.state.node_for_name(file_name)?;
        Ok((node.attributes(), self.state.security.as_ptr(), false))
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_file_info: CreateFileInfo,
        _security_descriptor: SecurityDescriptor,
    ) -> Result<(Self::FileContext, FileInfo), i32> {
        let path = path_from_winfsp(file_name)?;
        ensure_mutable_path(&path)?;
        let is_dir = create_file_info
            .create_options
            .is(CreateOptions::FILE_DIRECTORY_FILE);
        if self.state.node_for_path(&path)?.is_some() {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        if is_dir {
            let version = self.state.create_dir(&path)?;
            return Ok((
                VirtualFileHandle::new(path.clone(), true, version),
                dir_info(&path),
            ));
        }

        let version = self.state.write_file(&path, Vec::new())?;
        Ok((
            VirtualFileHandle::new(path.clone(), false, version),
            file_info(&path, 0),
        ))
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: CreateOptions,
        _granted_access: FileAccessRights,
    ) -> Result<(Self::FileContext, FileInfo), i32> {
        let path = path_from_winfsp(file_name)?;
        let Some(node) = self.state.node_for_path(&path)? else {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        };
        if create_options.is(CreateOptions::FILE_DIRECTORY_FILE) && !node.is_dir() {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        if create_options.is(CreateOptions::FILE_NON_DIRECTORY_FILE) && node.is_dir() {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let version = self.state.path_version(&path)?;
        Ok((
            VirtualFileHandle::new(path, node.is_dir(), version),
            node.file_info(),
        ))
    }

    fn overwrite(
        &self,
        file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _replace_file_attributes: bool,
        allocation_size: u64,
    ) -> Result<FileInfo, i32> {
        if file_context.is_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let path = file_context.path()?;
        ensure_mutable_path(&path)?;
        let size = usize::try_from(allocation_size).map_err(|_| STATUS_INVALID_PARAMETER)?;
        let version = self.state.write_file(&path, vec![0; size])?;
        file_context.version.store(version, Ordering::Relaxed);
        Ok(file_info(&path, size as u64))
    }

    fn cleanup(
        &self,
        file_context: Self::FileContext,
        _file_name: Option<&U16CStr>,
        flags: CleanupFlags,
    ) {
        if flags.is(CleanupFlags::DELETE) || file_context.delete_on_close.load(Ordering::Relaxed) {
            let Ok(path) = file_context.path() else {
                return;
            };
            if ensure_mutable_path(&path).is_err() {
                return;
            }
            let version = file_context.version.load(Ordering::Relaxed);
            let result = if file_context.is_dir {
                self.state.delete_dir_if_current(&path, version)
            } else {
                self.state.delete_file_if_current(&path, version)
            };
            let _ = result;
        }
    }

    fn close(&self, _file_context: Self::FileContext) {}

    fn read(
        &self,
        file_context: Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<usize, i32> {
        if file_context.is_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let path = file_context.path()?;
        let bytes = self.state.read_file(&path)?;
        let start = usize::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?;
        if start >= bytes.len() {
            return Ok(0);
        }
        let end = bytes.len().min(start + buffer.len());
        let slice = &bytes[start..end];
        buffer[..slice.len()].copy_from_slice(slice);
        Ok(slice.len())
    }

    fn write(
        &self,
        file_context: Self::FileContext,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, FileInfo), i32> {
        if file_context.is_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let path = file_context.path()?;
        ensure_mutable_path(&path)?;
        let (bytes, version) = self.state.write_file_range(&path, buffer, mode)?;
        file_context.version.store(version, Ordering::Relaxed);
        Ok((buffer.len(), file_info(&path, bytes as u64)))
    }

    fn flush(&self, file_context: Self::FileContext) -> Result<FileInfo, i32> {
        let path = file_context.path()?;
        self.state
            .node_for_path(&path)?
            .map(|node| node.file_info())
            .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    fn get_file_info(&self, file_context: Self::FileContext) -> Result<FileInfo, i32> {
        let path = file_context.path()?;
        self.state
            .node_for_path(&path)?
            .map(|node| node.file_info())
            .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    fn set_basic_info(
        &self,
        file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _change_time: u64,
    ) -> Result<FileInfo, i32> {
        self.get_file_info(file_context)
    }

    fn set_file_size(
        &self,
        file_context: Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
    ) -> Result<FileInfo, i32> {
        if file_context.is_dir {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let path = file_context.path()?;
        ensure_mutable_path(&path)?;
        let size = usize::try_from(new_size).map_err(|_| STATUS_INVALID_PARAMETER)?;
        let (bytes, version) = self.state.resize_file(&path, size)?;
        file_context.version.store(version, Ordering::Relaxed);
        Ok(file_info(&path, bytes as u64))
    }

    fn can_delete(&self, file_context: Self::FileContext, file_name: &U16CStr) -> Result<(), i32> {
        ensure_mutable_path(&file_context.path()?)?;
        ensure_mutable_path(&path_from_winfsp(file_name)?)?;
        Ok(())
    }

    fn rename(
        &self,
        file_context: Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<(), i32> {
        let from = file_context.path()?;
        let to = rename_target_path(&from, &path_from_winfsp(new_file_name)?);
        ensure_mutable_path(&from)?;
        ensure_mutable_path(&to)?;
        let version = if file_context.is_dir {
            self.state.rename_dir(&from, &to, replace_if_exists)?
        } else {
            self.state.rename_file(&from, &to, replace_if_exists)?
        };
        file_context.set_path(to)?;
        file_context.version.store(version, Ordering::Relaxed);
        Ok(())
    }

    fn get_security(&self, _file_context: Self::FileContext) -> Result<PSecurityDescriptor, i32> {
        Ok(self.state.security.as_ptr())
    }

    fn read_directory(
        &self,
        file_context: Self::FileContext,
        marker: Option<&U16CStr>,
        mut add_dir_info: impl FnMut(DirInfo) -> bool,
    ) -> Result<(), i32> {
        if !file_context.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        let path = file_context.path()?;
        let marker = marker.map(|marker| marker.to_string_lossy());
        let entries = self.state.dir_entries(&path)?;
        for (name, info) in entries {
            if marker
                .as_ref()
                .is_some_and(|marker| name.as_str() <= marker)
            {
                continue;
            }
            if !add_dir_info(DirInfo::from_str(info, &name)) {
                return Err(STATUS_BUFFER_OVERFLOW);
            }
        }
        Ok(())
    }

    fn get_dir_info_by_name(
        &self,
        file_context: Self::FileContext,
        file_name: &U16CStr,
    ) -> Result<FileInfo, i32> {
        if !file_context.is_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        let path = file_context.path()?;
        let child = child_path(&path, &path_from_winfsp(file_name)?);
        self.state
            .node_for_path(&child)?
            .map(|node| node.file_info())
            .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    fn set_delete(
        &self,
        file_context: Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<(), i32> {
        let path = file_context.path()?;
        ensure_mutable_path(&path)?;
        if delete_file {
            if file_context.is_dir {
                self.state.delete_dir(&path)?;
            } else {
                self.state.delete_file(&path)?;
            }
            file_context
                .version
                .store(self.state.path_version(&path)?, Ordering::Relaxed);
        }
        file_context
            .delete_on_close
            .store(delete_file, Ordering::Relaxed);
        Ok(())
    }
}
