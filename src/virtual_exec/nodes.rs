use std::io;

use windows_sys::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_INVALID_PARAMETER, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp_wrs::{FileAttributes, FileInfo, U16CStr, filetime_now};

use crate::vfs::{LaneFs, LaneFsError};
use crate::{FilePath, LaneError, is_git_metadata_path, is_lane_state_path};

use super::types::VirtualChangeOutput;

pub(super) enum VirtualNode {
    File { path: FilePath, len: u64 },
    Directory { path: FilePath },
}

impl VirtualNode {
    pub(super) fn is_dir(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }

    pub(super) fn attributes(&self) -> FileAttributes {
        match self {
            Self::File { .. } => FileAttributes::ARCHIVE,
            Self::Directory { .. } => FileAttributes::DIRECTORY,
        }
    }

    pub(super) fn file_info(&self) -> FileInfo {
        match self {
            Self::File { path, len } => file_info(path, *len),
            Self::Directory { path } => dir_info(path),
        }
    }
}

pub(super) fn file_info(path: &str, size: u64) -> FileInfo {
    let mut info = FileInfo::default();
    info.set_file_attributes(FileAttributes::ARCHIVE)
        .set_file_size(size)
        .set_allocation_size(size)
        .set_time(filetime_now())
        .set_index_number(index_number(path))
        .set_hard_links(1);
    info
}

pub(super) fn dir_info(path: &str) -> FileInfo {
    let mut info = FileInfo::default();
    info.set_file_attributes(FileAttributes::DIRECTORY)
        .set_file_size(0)
        .set_allocation_size(0)
        .set_time(filetime_now())
        .set_index_number(index_number(path))
        .set_hard_links(1);
    info
}

fn index_number(path: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in path.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub(super) fn path_from_winfsp(file_name: &U16CStr) -> Result<FilePath, i32> {
    let label = file_name
        .to_string_lossy()
        .trim_start_matches(['\\', '/'])
        .replace('\\', "/");
    if is_lane_state_path(&label) || label.contains("/../") {
        return Err(STATUS_ACCESS_DENIED);
    }
    if label
        .split('/')
        .any(|part| part == ".." || part.contains('\0'))
    {
        return Err(STATUS_ACCESS_DENIED);
    }
    Ok(label)
}

pub(super) fn ensure_mutable_path(path: &str) -> Result<(), i32> {
    if is_git_metadata_path(path) {
        Err(STATUS_ACCESS_DENIED)
    } else {
        Ok(())
    }
}

pub(super) fn child_path(parent: &str, child: &str) -> FilePath {
    let child = child.trim_start_matches(['\\', '/']).replace('\\', "/");
    if parent.is_empty() {
        child
    } else {
        format!("{parent}/{child}")
    }
}

pub(super) fn rename_target_path(from: &str, target: &str) -> FilePath {
    let Some((from_parent, _)) = from.rsplit_once('/') else {
        return target.to_owned();
    };
    if let Some((target_parent, target_name)) = target.rsplit_once('/')
        && target_parent.eq_ignore_ascii_case(from_parent)
    {
        return child_path(from_parent, target_name);
    }
    if target.contains('/') {
        return target.to_owned();
    }
    child_path(from_parent, target)
}

pub(super) fn change_for_path(
    fs: &LaneFs,
    lane: &str,
    path: impl Into<String>,
) -> Result<Option<VirtualChangeOutput>, i32> {
    fs.change_for_path(lane, path)
        .map(|change| change.map(VirtualChangeOutput::from))
        .map_err(status_from_lane_fs_error)
}

pub(super) fn status_from_lane_fs_error(error: LaneFsError) -> i32 {
    match error {
        LaneFsError::BadPath(_) => STATUS_ACCESS_DENIED,
        LaneFsError::Io(error) => status_from_io_error(error),
        LaneFsError::Lane(error) => status_from_lane_error(error),
    }
}

fn status_from_lane_error(error: LaneError) -> i32 {
    match error {
        LaneError::LaneMissing(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        LaneError::BaseChanged { .. } => STATUS_ACCESS_DENIED,
        LaneError::ReservedLane(_) => STATUS_INVALID_PARAMETER,
        LaneError::OperationOutOfBounds { .. }
        | LaneError::OperationConflict { .. }
        | LaneError::EmptyOperationSelection
        | LaneError::OperationMissing { .. } => STATUS_INVALID_PARAMETER,
    }
}

fn status_from_io_error(error: io::Error) -> i32 {
    match error.kind() {
        io::ErrorKind::NotFound => STATUS_OBJECT_PATH_NOT_FOUND,
        io::ErrorKind::PermissionDenied => STATUS_ACCESS_DENIED,
        io::ErrorKind::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        _ => STATUS_ACCESS_DENIED,
    }
}
