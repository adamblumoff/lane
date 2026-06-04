use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::storage::{acquire_repo_lock, load_repo, persist_repo};
use crate::vfs::{DirEntry, DirEntryKind, FileWorktree, LaneFs, LaneFsError};
use crate::{LaneError, LaneRepo};
use windows::Win32::Foundation::{
    NTSTATUS, STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_IS_A_DIRECTORY, STATUS_INTERNAL_ERROR,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirBuffer, DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
    VolumeInfo, WideNameInfo,
};
use winfsp::host::{CoarseGuard, FileSystemHost, VolumeParams};
use winfsp::{FspError, U16CStr};
use winfsp_sys::{FILE_ACCESS_RIGHTS, FILE_FLAGS_AND_ATTRIBUTES};

const STORAGE_PATH: &str = ".lane/repo.lane";
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;

pub struct MountOptions {
    pub repo_root: PathBuf,
    pub lane: String,
    pub mount_path: PathBuf,
}

pub struct MountedLane {
    _init: winfsp::FspInit,
    host: FileSystemHost<LaneWinFsp, CoarseGuard>,
    mount_path: PathBuf,
}

impl MountedLane {
    pub fn view_root(&self) -> PathBuf {
        if looks_like_drive_mount(&self.mount_path) {
            let label = self.mount_path.as_os_str().to_string_lossy();
            PathBuf::from(format!("{label}\\"))
        } else {
            self.mount_path.clone()
        }
    }
}

impl Drop for MountedLane {
    fn drop(&mut self) {
        self.host.stop();
        self.host.unmount();
    }
}

pub fn mount_hidden(options: MountOptions) -> winfsp::Result<MountedLane> {
    let init = winfsp::winfsp_init()?;
    let mount_path = options.mount_path.clone();
    if !looks_like_drive_mount(&options.mount_path) {
        fs::create_dir_all(&options.mount_path)?;
    }

    let context = LaneWinFsp::open(&options)?;
    let mut volume = VolumeParams::new();
    volume
        .filesystem_name("LaneFS")
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .post_cleanup_when_modified_only(false)
        .flush_and_purge_on_cleanup(true);

    let mut host = FileSystemHost::<LaneWinFsp, CoarseGuard>::new(volume, context)?;
    host.mount(&mount_path)?;
    host.start()?;
    Ok(MountedLane {
        _init: init,
        host,
        mount_path,
    })
}

struct LaneWinFsp {
    lane: String,
    storage_path: PathBuf,
    state: RefCell<LaneFs<FileWorktree>>,
    created_dirs: RefCell<BTreeSet<String>>,
}

struct LaneFileContext {
    path: String,
    kind: NodeKind,
    delete_on_cleanup: Cell<bool>,
    dir_buffer: DirBuffer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Directory,
    File,
}

impl LaneWinFsp {
    fn open(options: &MountOptions) -> winfsp::Result<Self> {
        let storage_path = options.repo_root.join(STORAGE_PATH);
        let _lock = acquire_repo_lock(&storage_path)?;
        let mut repo = match load_repo(&storage_path) {
            Ok(Some(repo)) => repo,
            Ok(None) => LaneRepo::new(),
            Err(error) if error.kind() == ErrorKind::InvalidData => LaneRepo::new(),
            Err(error) => return Err(error.into()),
        };
        repo.create_lane(options.lane.clone())
            .map_err(map_lane_error)?;
        persist_repo(&storage_path, &repo)?;
        let state = LaneFs::new(repo, FileWorktree::new(options.repo_root.clone()));

        Ok(Self {
            lane: options.lane.clone(),
            storage_path,
            state: RefCell::new(state),
            created_dirs: RefCell::new(BTreeSet::new()),
        })
    }

    fn node_kind(&self, path: &str) -> winfsp::Result<NodeKind> {
        if self.is_directory(path)? {
            return Ok(NodeKind::Directory);
        }
        if self
            .state
            .borrow()
            .read_file(&self.lane, path)
            .map_err(map_fs_error)?
            .is_some()
        {
            return Ok(NodeKind::File);
        }
        Err(nt(STATUS_OBJECT_NAME_NOT_FOUND))
    }

    fn is_directory(&self, path: &str) -> winfsp::Result<bool> {
        if path.is_empty() || self.created_dirs.borrow().contains(path) {
            return Ok(true);
        }
        Ok(!self.list_dir(path)?.is_empty())
    }

    fn list_dir(&self, path: &str) -> winfsp::Result<Vec<DirEntry>> {
        let mut entries = self
            .state
            .borrow()
            .list_dir(&self.lane, path)
            .map_err(map_fs_error)?;
        for dir in self.created_dirs.borrow().iter() {
            if let Some(name) = direct_child(path, dir)
                && !entries.iter().any(|entry| entry.name == name)
            {
                entries.push(DirEntry {
                    name,
                    kind: DirEntryKind::Directory,
                });
            }
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn read_file(&self, path: &str) -> winfsp::Result<Vec<u8>> {
        self.state
            .borrow()
            .read_file(&self.lane, path)
            .map_err(map_fs_error)?
            .ok_or_else(|| nt(STATUS_OBJECT_NAME_NOT_FOUND))
    }

    fn write_file(&self, path: &str, bytes: Vec<u8>) -> winfsp::Result<()> {
        let mut state = self.state.borrow_mut();
        state
            .write_file(&self.lane, path, bytes)
            .map_err(map_fs_error)?;
        self.persist_lane_state(&state)?;
        Ok(())
    }

    fn delete_file(&self, path: &str) -> winfsp::Result<()> {
        let mut state = self.state.borrow_mut();
        state.delete_file(&self.lane, path).map_err(map_fs_error)?;
        self.persist_lane_state(&state)?;
        Ok(())
    }

    fn rename_file(&self, from: &str, to: &str) -> winfsp::Result<()> {
        let mut state = self.state.borrow_mut();
        state
            .rename_file(&self.lane, from, to)
            .map_err(map_fs_error)?;
        self.persist_lane_state(&state)?;
        Ok(())
    }

    fn persist_lane_state(&self, state: &LaneFs<FileWorktree>) -> winfsp::Result<()> {
        let _lock = acquire_repo_lock(&self.storage_path)?;
        let mut repo = match load_repo(&self.storage_path) {
            Ok(Some(repo)) => repo,
            Ok(None) => LaneRepo::new(),
            Err(error) if error.kind() == ErrorKind::InvalidData => LaneRepo::new(),
            Err(error) => return Err(error.into()),
        };
        repo.create_lane(self.lane.clone())
            .map_err(map_lane_error)?;

        let mut merged = LaneFs::new(
            repo,
            FileWorktree::new(state.worktree().root_path().to_path_buf()),
        );
        let paths = merged
            .changed_paths(&self.lane)
            .map_err(map_fs_error)?
            .into_iter()
            .chain(state.changed_paths(&self.lane).map_err(map_fs_error)?)
            .collect::<BTreeSet<_>>();

        for path in paths {
            match state.read_file(&self.lane, &path).map_err(map_fs_error)? {
                Some(bytes) => merged
                    .write_file(&self.lane, &path, bytes)
                    .map_err(map_fs_error)?,
                None => merged
                    .delete_file(&self.lane, &path)
                    .map_err(map_fs_error)?,
            }
        }

        persist_repo(&self.storage_path, merged.repo())?;
        Ok(())
    }

    fn ensure_parent_directory(&self, path: &str) -> winfsp::Result<()> {
        let Some(parent) = parent_path(path) else {
            return Ok(());
        };
        if self.is_directory(&parent)? {
            Ok(())
        } else {
            Err(nt(STATUS_OBJECT_PATH_NOT_FOUND))
        }
    }

    fn info_for_path(&self, path: &str, kind: NodeKind) -> winfsp::Result<FileInfo> {
        let size = match kind {
            NodeKind::Directory => 0,
            NodeKind::File => self.read_file(path)?.len() as u64,
        };
        Ok(file_info(kind, size))
    }
}

impl FileSystemContext for LaneWinFsp {
    type FileContext = LaneFileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let path = mount_path(file_name)?;
        let kind = self.node_kind(&path)?;
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: file_info(kind, 0).file_attributes,
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        open_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = mount_path(file_name)?;
        let kind = self.node_kind(&path)?;
        ensure_open_kind(kind, create_options)?;
        *open_info.as_mut() = self.info_for_path(&path, kind)?;
        open_info.set_normalized_name(file_name.as_slice(), None);
        Ok(file_context(path, kind))
    }

    fn close(&self, _context: Self::FileContext) {}

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        open_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = mount_path(file_name)?;
        if self.node_kind(&path).is_ok() {
            return Err(nt(STATUS_OBJECT_NAME_COLLISION));
        }
        self.ensure_parent_directory(&path)?;

        let kind = if create_options & FILE_DIRECTORY_FILE != 0 {
            self.created_dirs.borrow_mut().insert(path.clone());
            NodeKind::Directory
        } else {
            self.write_file(&path, Vec::new())?;
            NodeKind::File
        };
        *open_info.as_mut() = self.info_for_path(&path, kind)?;
        open_info.set_normalized_name(file_name.as_slice(), None);
        Ok(file_context(path, kind))
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        if !context.delete_on_cleanup.get() && !FspCleanupFlags::FspCleanupDelete.is_flagged(flags)
        {
            return;
        }

        match context.kind {
            NodeKind::Directory => {
                self.created_dirs.borrow_mut().remove(&context.path);
            }
            NodeKind::File => {
                let _ = self.delete_file(&context.path);
            }
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if let Some(context) = context {
            *file_info = self.info_for_path(&context.path, context.kind)?;
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        *file_info = self.info_for_path(&context.path, context.kind)?;
        Ok(())
    }

    fn get_security(
        &self,
        _context: &Self::FileContext,
        _security_descriptor: Option<&mut [c_void]>,
    ) -> winfsp::Result<u64> {
        Ok(0)
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        ensure_file(context.kind)?;
        self.write_file(&context.path, Vec::new())?;
        *file_info = self.info_for_path(&context.path, context.kind)?;
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        ensure_directory(context.kind)?;
        let reset = marker.is_none();
        let lock = context.dir_buffer.acquire(reset, None)?;
        if reset {
            write_dir_info(&lock, ".", NodeKind::Directory, 0)?;
            write_dir_info(&lock, "..", NodeKind::Directory, 0)?;
            for entry in self.list_dir(&context.path)? {
                let kind = match entry.kind {
                    DirEntryKind::Directory => NodeKind::Directory,
                    DirEntryKind::File => NodeKind::File,
                };
                let size = match kind {
                    NodeKind::Directory => 0,
                    NodeKind::File => self
                        .state
                        .borrow()
                        .read_file(&self.lane, &join_path(&context.path, &entry.name))
                        .map_err(map_fs_error)?
                        .map_or(0, |bytes| bytes.len() as u64),
                };
                write_dir_info(&lock, &entry.name, kind, size)?;
            }
        }
        drop(lock);
        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        ensure_file(context.kind)?;
        let to = mount_path(new_file_name)?;
        if !replace_if_exists && self.node_kind(&to).is_ok() {
            return Err(nt(STATUS_OBJECT_NAME_COLLISION));
        }
        self.ensure_parent_directory(&to)?;
        self.rename_file(&context.path, &to)
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        *file_info = self.info_for_path(&context.path, context.kind)?;
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file
            && context.kind == NodeKind::Directory
            && !self.list_dir(&context.path)?.is_empty()
        {
            return Err(nt(STATUS_DIRECTORY_NOT_EMPTY));
        }
        context.delete_on_cleanup.set(delete_file);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        ensure_file(context.kind)?;
        let mut bytes = self.read_file(&context.path)?;
        bytes.resize(new_size as usize, 0);
        self.write_file(&context.path, bytes)?;
        *file_info = self.info_for_path(&context.path, context.kind)?;
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        ensure_file(context.kind)?;
        let bytes = self.read_file(&context.path)?;
        let offset = offset as usize;
        if offset >= bytes.len() {
            return Ok(0);
        }
        let end = (offset + buffer.len()).min(bytes.len());
        let slice = &bytes[offset..end];
        buffer[..slice.len()].copy_from_slice(slice);
        Ok(slice.len() as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        ensure_file(context.kind)?;
        let mut bytes = self.read_file(&context.path).unwrap_or_default();
        let offset = if write_to_eof {
            bytes.len()
        } else {
            offset as usize
        };
        if constrained_io && offset > bytes.len() {
            return Ok(0);
        }
        let end = offset.saturating_add(buffer.len());
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[offset..end].copy_from_slice(buffer);
        self.write_file(&context.path, bytes)?;
        *file_info = self.info_for_path(&context.path, context.kind)?;
        Ok(buffer.len() as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        out_volume_info.total_size = 1024 * 1024 * 1024 * 1024;
        out_volume_info.free_size = 1024 * 1024 * 1024 * 1024;
        out_volume_info.set_volume_label("LaneFS");
        Ok(())
    }
}

fn file_context(path: String, kind: NodeKind) -> LaneFileContext {
    LaneFileContext {
        path,
        kind,
        delete_on_cleanup: Cell::new(false),
        dir_buffer: DirBuffer::new(),
    }
}

fn file_info(kind: NodeKind, size: u64) -> FileInfo {
    FileInfo {
        file_attributes: match kind {
            NodeKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
            NodeKind::File => FILE_ATTRIBUTE_ARCHIVE,
        },
        allocation_size: size,
        file_size: size,
        ..FileInfo::default()
    }
}

fn write_dir_info(
    lock: &winfsp::filesystem::DirBufferLock<'_>,
    name: &str,
    kind: NodeKind,
    size: u64,
) -> winfsp::Result<()> {
    let mut info = DirInfo::<255>::new();
    *info.file_info_mut() = file_info(kind, size);
    let name = name.encode_utf16().collect::<Vec<_>>();
    info.set_name_raw(name.as_slice())?;
    lock.write(&mut info)
}

fn mount_path(file_name: &U16CStr) -> winfsp::Result<String> {
    let path = file_name
        .to_string_lossy()
        .trim_start_matches(['\\', '/'])
        .replace('\\', "/");
    if path.eq_ignore_ascii_case(".lane") || path.to_ascii_lowercase().starts_with(".lane/") {
        return Err(nt(STATUS_OBJECT_NAME_NOT_FOUND));
    }
    Ok(path)
}

fn parent_path(path: &str) -> Option<String> {
    path.rsplit_once('/').map(|(parent, _)| parent.to_owned())
}

fn direct_child(directory: &str, child: &str) -> Option<String> {
    let tail = if directory.is_empty() {
        child
    } else {
        child.strip_prefix(&format!("{directory}/"))?
    };
    if tail.is_empty() || tail.contains('/') {
        None
    } else {
        Some(tail.to_owned())
    }
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

fn ensure_open_kind(kind: NodeKind, create_options: u32) -> winfsp::Result<()> {
    if create_options & FILE_DIRECTORY_FILE != 0 && kind != NodeKind::Directory {
        return Err(nt(STATUS_NOT_A_DIRECTORY));
    }
    if create_options & FILE_NON_DIRECTORY_FILE != 0 && kind == NodeKind::Directory {
        return Err(nt(STATUS_FILE_IS_A_DIRECTORY));
    }
    Ok(())
}

fn ensure_file(kind: NodeKind) -> winfsp::Result<()> {
    if kind == NodeKind::File {
        Ok(())
    } else {
        Err(nt(STATUS_FILE_IS_A_DIRECTORY))
    }
}

fn ensure_directory(kind: NodeKind) -> winfsp::Result<()> {
    if kind == NodeKind::Directory {
        Ok(())
    } else {
        Err(nt(STATUS_NOT_A_DIRECTORY))
    }
}

fn map_fs_error(error: LaneFsError) -> FspError {
    match error {
        LaneFsError::BadPath(_) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
        LaneFsError::FileMissing { .. } => nt(STATUS_OBJECT_NAME_NOT_FOUND),
        LaneFsError::Io(error) => error.into(),
        LaneFsError::Lane(LaneError::LaneMissing(_)) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
        LaneFsError::Lane(_) => nt(STATUS_INTERNAL_ERROR),
    }
}

fn map_lane_error(error: LaneError) -> FspError {
    match error {
        LaneError::LaneMissing(_) => nt(STATUS_OBJECT_NAME_NOT_FOUND),
        _ => nt(STATUS_INTERNAL_ERROR),
    }
}

fn nt(status: NTSTATUS) -> FspError {
    status.into()
}

fn looks_like_drive_mount(path: &Path) -> bool {
    let label = path.as_os_str().to_string_lossy();
    label.len() == 2 && label.ends_with(':')
}
