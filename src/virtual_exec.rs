use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::Serialize;
use windows_sys::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_DIRECTORY_NOT_EMPTY,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_INVALID_PARAMETER, STATUS_NO_SUCH_FILE,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp_wrs::{
    CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights, FileAttributes,
    FileInfo, FileSystem, FileSystemInterface, OperationGuardStrategy, PSecurityDescriptor, Params,
    SecurityDescriptor, U16CStr, U16CString, VolumeInfo, WriteMode, filetime_now, u16cstr,
};

use crate::storage::{acquire_repo_lock, load_repo, persist_repo};
use crate::vfs::{DirEntryKind, FileWorktree, LaneFs, LaneFsError};
use crate::{FilePath, LaneError, LaneOpSummary};

const STORAGE_PATH: &str = ".lane/repo.lane";

pub(crate) fn run_virtual_lane(
    repo_root: &Path,
    lane: &str,
    command: &[String],
) -> Result<VirtualLaneRun, VirtualExecError> {
    let total_start = Instant::now();
    let (program, args) = command
        .split_first()
        .ok_or_else(|| VirtualExecError::message("missing command for lane exec"))?;
    let storage_path = repo_root.join(STORAGE_PATH);
    let metrics = Arc::new(VirtualFsMetrics::default());

    let setup_start = Instant::now();
    winfsp_wrs::init().map_err(|error| VirtualExecError::message(error.to_string()))?;
    let mount_start = Instant::now();
    let (mount_point, mount, state) = start_mount(repo_root, &storage_path, lane, metrics.clone())?;
    let mount_ms = elapsed_ms(mount_start);
    let pre_worker_lock_ms = elapsed_ms(setup_start);

    let worker_start = Instant::now();
    let worker = run_virtual_worker(program, args, lane, repo_root, &mount_point.workspace_path);
    let worker_ms = elapsed_ms(worker_start);

    let stop_start = Instant::now();
    mount.stop();
    let unmount_ms = elapsed_ms(stop_start);

    let collect_start = Instant::now();
    let projected_paths = state.projected_paths()?;
    let changed_paths = state.worker_changed_paths()?;
    state.flush()?;
    let changes = state.collect_changes()?;
    let post_worker_lock_ms = elapsed_ms(collect_start);
    let snapshot = metrics.snapshot();
    let failed = worker.exit_code != Some(0) || worker.worker_error.is_some();

    let output = VirtualExecOutput {
        lane: lane.to_owned(),
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        workspace_root: path_label(&mount_point.workspace_path),
        mount_path: path_label(&mount_point.workspace_path),
        mode: "virtual_mount",
        projected_paths,
        exit_code: worker.exit_code,
        stdout: worker.stdout,
        stderr: worker.stderr,
        worker_error: worker.worker_error,
        changed_paths,
        timings: VirtualExecTimings {
            total_ms: elapsed_ms(total_start),
            lock_wait_ms: snapshot.storage_lock_wait_ms,
            lock_held_ms: snapshot.storage_lock_held_ms,
            storage_lock_wait_ms: snapshot.storage_lock_wait_ms,
            storage_lock_held_ms: snapshot.storage_lock_held_ms,
            pre_worker_lock_ms,
            worker_ms,
            post_worker_lock_ms,
            mount_ms,
            unmount_ms,
            storage_write_ops: snapshot.storage_write_ops,
        },
        changes,
    };

    Ok(VirtualLaneRun { output, failed })
}

fn start_mount(
    repo_root: &Path,
    storage_path: &Path,
    lane: &str,
    metrics: Arc<VirtualFsMetrics>,
) -> Result<(MountPoint, FileSystem, Arc<VirtualLaneState>), VirtualExecError> {
    let security =
        SecurityDescriptor::from_wstr(u16cstr!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"))
            .map_err(VirtualExecError::message)?;
    let session_fs = prepare_session_fs(repo_root, storage_path, lane, &metrics)?;
    let state = Arc::new(VirtualLaneState {
        repo_root: repo_root.to_path_buf(),
        storage_path: storage_path.to_path_buf(),
        lane: lane.to_owned(),
        fs: Mutex::new(session_fs),
        dirty: Mutex::new(BTreeMap::new()),
        versions: Mutex::new(BTreeMap::new()),
        next_version: AtomicU64::new(1),
        security,
        metrics,
    });
    let mut last_unavailable = None;
    for letter in (b'D'..=b'Z').rev().map(char::from) {
        let Some(mount_point) = try_allocate_mount_point(letter)? else {
            continue;
        };
        let context = VirtualLaneFs {
            state: state.clone(),
        };
        let params = winfsp_params()?;
        match FileSystem::start(params, Some(mount_point.mount_name.as_ucstr()), context) {
            Ok(file_system) => return Ok((mount_point, file_system, state)),
            Err(STATUS_OBJECT_NAME_COLLISION | STATUS_ACCESS_DENIED) => {
                last_unavailable = Some(letter);
            }
            Err(status) => {
                return Err(VirtualExecError::message(format!(
                    "WinFsp mount failed: {status:#x}"
                )));
            }
        }
    }

    let suffix = last_unavailable
        .map(|letter| format!("; last unavailable candidate was {letter}:"))
        .unwrap_or_default();
    Err(VirtualExecError::message(format!(
        "no free drive letter available for virtual lane mount{suffix}"
    )))
}

fn winfsp_params() -> Result<Params, VirtualExecError> {
    let mut params = Params {
        guard_strategy: OperationGuardStrategy::Fine,
        ..Params::default()
    };
    params
        .volume_params
        .set_case_sensitive_search(false)
        .set_case_preserved_names(true)
        .set_unicode_on_disk(true)
        .set_post_cleanup_when_modified_only(false)
        .set_file_info_timeout(0)
        .set_dir_info_timeout(0)
        .set_security_timeout(0)
        .set_file_system_name(u16cstr!("Lane"))
        .map_err(|_| VirtualExecError::message("WinFsp filesystem name is too long"))?;
    Ok(params)
}

fn try_allocate_mount_point(letter: char) -> io::Result<Option<MountPoint>> {
    let workspace_path = PathBuf::from(format!("{letter}:\\"));
    match workspace_path.try_exists() {
        Ok(true) => return Ok(None),
        Ok(false) => {}
        Err(_) => return Ok(None),
    }
    let lock_dir = env::temp_dir().join("lane").join("mounts");
    fs::create_dir_all(&lock_dir)?;
    let lock_path = lock_dir.join(format!("{letter}.lock"));
    let Ok(guard) = MountLetterGuard::try_acquire(lock_path) else {
        return Ok(None);
    };
    let mount_name = U16CString::from_str(format!("{letter}:"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad mount name"))?;
    Ok(Some(MountPoint {
        workspace_path,
        mount_name,
        _guard: guard,
    }))
}

struct MountPoint {
    workspace_path: PathBuf,
    mount_name: U16CString,
    _guard: MountLetterGuard,
}

struct MountLetterGuard {
    path: PathBuf,
    _file: File,
}

impl MountLetterGuard {
    fn try_acquire(path: PathBuf) -> io::Result<Self> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self { path, _file: file })
    }
}

impl Drop for MountLetterGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn run_virtual_worker(
    program: &str,
    args: &[String],
    lane: &str,
    repo_root: &Path,
    mount_path: &Path,
) -> WorkerOutput {
    match virtual_command(program, args, lane, repo_root, mount_path).output() {
        Ok(output) => WorkerOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            worker_error: None,
        },
        Err(error) => WorkerOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            worker_error: Some(error.to_string()),
        },
    }
}

fn virtual_command<'a>(
    program: &'a str,
    args: &'a [String],
    lane: &'a str,
    _repo_root: &'a Path,
    mount_path: &'a Path,
) -> ProcessCommand {
    let mount_label = path_label(mount_path);
    let safe_directory = git_safe_directory_label(mount_path);
    let mut command = ProcessCommand::new(resolve_program(program));
    command
        .args(args)
        .current_dir(mount_path)
        .env("LANE_ID", lane)
        .env("LANE_REPO_ROOT", &mount_label)
        .env("LANE_VIEW_ROOT", &mount_label)
        .env("LANE_EXEC_MODE", "virtual_mount")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "safe.directory")
        .env("GIT_CONFIG_VALUE_0", safe_directory)
        .env_remove("LANE_STORAGE_PATH");
    command
}

fn git_safe_directory_label(path: &Path) -> String {
    path_label(path).replace('\\', "/")
}

fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.components().any(|component| {
        matches!(
            component,
            PathComponent::RootDir | PathComponent::Prefix(_) | PathComponent::ParentDir
        )
    }) {
        return path.to_path_buf();
    }
    if path.components().count() > 1 {
        return path.to_path_buf();
    }

    let extensions = env::var_os("PATHEXT")
        .map(|value| {
            env::split_paths(&value)
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| {
            [".COM", ".EXE", ".BAT", ".CMD"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        });

    let Some(paths) = env::var_os("PATH") else {
        return path.to_path_buf();
    };
    let names = if path.extension().is_some() {
        vec![program.to_owned()]
    } else {
        extensions
            .iter()
            .map(|extension| format!("{program}{extension}"))
            .collect::<Vec<_>>()
    };
    for directory in env::split_paths(&paths) {
        for name in &names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}

#[derive(Clone)]
struct VirtualLaneFs {
    state: Arc<VirtualLaneState>,
}

struct VirtualLaneState {
    repo_root: PathBuf,
    storage_path: PathBuf,
    lane: String,
    fs: Mutex<LaneFs<FileWorktree>>,
    dirty: Mutex<BTreeMap<FilePath, DirtyEntry>>,
    versions: Mutex<BTreeMap<FilePath, u64>>,
    next_version: AtomicU64,
    security: SecurityDescriptor,
    metrics: Arc<VirtualFsMetrics>,
}

#[derive(Clone, Debug)]
enum DirtyEntry {
    File(Vec<u8>),
    Directory,
    Deleted,
}

struct VirtualFileHandle {
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

impl VirtualLaneState {
    fn node_for_name(&self, file_name: &U16CStr) -> Result<VirtualNode, i32> {
        let path = path_from_winfsp(file_name)?;
        self.node_for_path(&path)?
            .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    fn node_for_path(&self, path: &str) -> Result<Option<VirtualNode>, i32> {
        if path.is_empty() {
            return Ok(Some(VirtualNode::Directory {
                path: String::new(),
            }));
        }

        if let Some(entry) = self.dirty_entry(path)? {
            return Ok(match entry {
                DirtyEntry::File(bytes) => Some(VirtualNode::File {
                    path: path.to_owned(),
                    len: bytes.len() as u64,
                }),
                DirtyEntry::Directory => Some(VirtualNode::Directory {
                    path: path.to_owned(),
                }),
                DirtyEntry::Deleted => None,
            });
        }

        self.with_fs_read(|fs| {
            let overlay_paths = fs
                .changed_paths(&self.lane)
                .map_err(status_from_lane_fs_error)?;
            let has_overlay = overlay_paths.iter().any(|overlay| overlay == path);
            if let Some(bytes) = fs
                .read_file(&self.lane, path)
                .map_err(status_from_lane_fs_error)?
            {
                return Ok(Some(VirtualNode::File {
                    path: path.to_owned(),
                    len: bytes.len() as u64,
                }));
            }
            if has_overlay {
                return Ok(None);
            }
            if self.repo_root.join(path).is_dir()
                || self.path_has_visible_children(fs, path)?
                || self.dirty_has_visible_children(path)?
            {
                return Ok(Some(VirtualNode::Directory {
                    path: path.to_owned(),
                }));
            }
            Ok(None)
        })
    }

    fn path_has_visible_children(
        &self,
        fs: &LaneFs<FileWorktree>,
        path: &str,
    ) -> Result<bool, i32> {
        fs.list_dir(&self.lane, path)
            .map(|entries| !entries.is_empty())
            .map_err(status_from_lane_fs_error)
    }

    fn read_file(&self, path: &str) -> Result<Vec<u8>, i32> {
        if let Some(entry) = self.dirty_entry(path)? {
            return match entry {
                DirtyEntry::File(bytes) => Ok(bytes),
                DirtyEntry::Directory | DirtyEntry::Deleted => Err(STATUS_NO_SUCH_FILE),
            };
        }

        self.with_fs_read(|fs| {
            fs.read_file(&self.lane, path)
                .map_err(status_from_lane_fs_error)?
                .ok_or(STATUS_NO_SUCH_FILE)
        })
    }

    fn write_file(&self, path: &str, bytes: Vec<u8>) -> Result<u64, i32> {
        self.set_dirty_entry(path, DirtyEntry::File(bytes))
    }

    fn write_file_range(
        &self,
        path: &str,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, u64), i32> {
        let mut bytes = self.read_file_for_write(path)?;
        let offset = match mode {
            WriteMode::Normal { offset } | WriteMode::ConstrainedIO { offset } => {
                usize::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?
            }
            WriteMode::WriteToEOF => bytes.len(),
        };
        if offset > bytes.len() {
            bytes.resize(offset, 0);
        }
        if matches!(mode, WriteMode::ConstrainedIO { .. }) {
            let writable = buffer.len().min(bytes.len().saturating_sub(offset));
            bytes[offset..offset + writable].copy_from_slice(&buffer[..writable]);
        } else {
            let end = offset
                .checked_add(buffer.len())
                .ok_or(STATUS_INVALID_PARAMETER)?;
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[offset..end].copy_from_slice(buffer);
        }
        let len = bytes.len();
        let version = self.set_dirty_entry(path, DirtyEntry::File(bytes))?;
        Ok((len, version))
    }

    fn resize_file(&self, path: &str, size: usize) -> Result<(usize, u64), i32> {
        let mut bytes = self.read_file_for_write(path)?;
        bytes.resize(size, 0);
        let version = self.set_dirty_entry(path, DirtyEntry::File(bytes))?;
        Ok((size, version))
    }

    fn delete_file(&self, path: &str) -> Result<(), i32> {
        self.set_dirty_entry(path, DirtyEntry::Deleted).map(|_| ())
    }

    fn rename_file(&self, from: &str, to: &str, replace_if_exists: bool) -> Result<u64, i32> {
        if from == to {
            return self.path_version(from);
        }
        if self.node_for_path(to)?.is_some() && !replace_if_exists {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        if self.node_for_path(to)?.is_some_and(|node| node.is_dir()) {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }

        let bytes = self.read_file(from)?;
        self.set_dirty_entry(to, DirtyEntry::File(bytes))?;
        self.set_dirty_entry(from, DirtyEntry::Deleted)
    }

    fn rename_dir(&self, from: &str, to: &str, replace_if_exists: bool) -> Result<u64, i32> {
        if from == to {
            return self.path_version(from);
        }
        if to.starts_with(&child_prefix(from)) {
            return Err(STATUS_ACCESS_DENIED);
        }
        if self.node_for_path(to)?.is_some() {
            if !replace_if_exists {
                return Err(STATUS_OBJECT_NAME_COLLISION);
            }
            if self.dir_has_children(to)? {
                return Err(STATUS_DIRECTORY_NOT_EMPTY);
            }
        }

        let mut files = Vec::new();
        self.collect_state_visible_files(from, &mut files)?;
        for path in files {
            let target = child_path(to, path.strip_prefix(&child_prefix(from)).unwrap_or(&path));
            let bytes = self.read_file(&path)?;
            self.set_dirty_entry(&target, DirtyEntry::File(bytes))?;
            self.set_dirty_entry(&path, DirtyEntry::Deleted)?;
        }
        self.set_dirty_entry(to, DirtyEntry::Directory)?;
        self.set_dirty_entry(from, DirtyEntry::Deleted)
    }

    fn create_dir(&self, path: &str) -> Result<u64, i32> {
        self.set_dirty_entry(path, DirtyEntry::Directory)
    }

    fn delete_file_if_current(&self, path: &str, version: u64) -> Result<(), i32> {
        if self.path_version(path)? == version {
            self.delete_file(path)?;
        }
        Ok(())
    }

    fn delete_dir_if_current(&self, path: &str, version: u64) -> Result<(), i32> {
        if self.path_version(path)? == version {
            self.delete_dir(path)?;
        }
        Ok(())
    }

    fn delete_dir(&self, path: &str) -> Result<(), i32> {
        let mut deleted = Vec::new();
        self.with_fs_read(|fs| collect_visible_files(fs, &self.lane, path, &mut deleted))?;
        let prefix = child_prefix(path);
        {
            let dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
            deleted.extend(dirty.iter().filter_map(|(dirty_path, entry)| {
                matches!(entry, DirtyEntry::File(_) | DirtyEntry::Directory)
                    .then_some(dirty_path)
                    .filter(|dirty_path| dirty_path.starts_with(&prefix))
                    .cloned()
            }));
        }

        let mut dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        for deleted_path in deleted {
            self.set_dirty_entry_locked(&mut dirty, deleted_path, DirtyEntry::Deleted)?;
        }
        self.set_dirty_entry_locked(&mut dirty, path.to_owned(), DirtyEntry::Deleted)?;
        Ok(())
    }

    fn read_file_for_write(&self, path: &str) -> Result<Vec<u8>, i32> {
        match self.read_file(path) {
            Ok(bytes) => Ok(bytes),
            Err(STATUS_NO_SUCH_FILE | STATUS_OBJECT_NAME_NOT_FOUND) => Ok(Vec::new()),
            Err(status) => Err(status),
        }
    }

    fn dir_has_children(&self, path: &str) -> Result<bool, i32> {
        self.dir_entries(path)
            .map(|entries| entries.iter().any(|(name, _)| name != "." && name != ".."))
    }

    fn collect_state_visible_files(
        &self,
        path: &str,
        files: &mut Vec<FilePath>,
    ) -> Result<(), i32> {
        for (name, _) in self.dir_entries(path)? {
            if name == "." || name == ".." {
                continue;
            }
            let child = child_path(path, &name);
            match self.node_for_path(&child)? {
                Some(node) if node.is_dir() => self.collect_state_visible_files(&child, files)?,
                Some(_) => files.push(child),
                None => {}
            }
        }
        Ok(())
    }

    fn dir_entries(&self, path: &str) -> Result<Vec<(String, FileInfo)>, i32> {
        self.with_fs_read(|fs| {
            let mut entries: BTreeMap<String, FileInfo> = vec![
                (".".to_owned(), dir_info(path)),
                ("..".to_owned(), dir_info("")),
            ]
            .into_iter()
            .collect();
            for entry in fs
                .list_dir(&self.lane, path)
                .map_err(status_from_lane_fs_error)?
            {
                let child = child_path(path, &entry.name);
                let info = match entry.kind {
                    DirEntryKind::Directory => dir_info(&child),
                    DirEntryKind::File => {
                        let len = fs
                            .read_file(&self.lane, &child)
                            .map_err(status_from_lane_fs_error)?
                            .map(|bytes| bytes.len() as u64)
                            .unwrap_or(0);
                        file_info(&child, len)
                    }
                };
                entries.insert(entry.name, info);
            }
            for (dirty_path, dirty) in self.dirty_entries()? {
                let Some((name, kind)) = dir_entry_for_dirty_path(path, &dirty_path) else {
                    continue;
                };
                match (kind, dirty) {
                    (DirEntryKind::Directory, _) => {
                        entries.insert(name, dir_info(&dirty_path));
                    }
                    (DirEntryKind::File, DirtyEntry::File(bytes)) => {
                        entries.insert(name, file_info(&dirty_path, bytes.len() as u64));
                    }
                    (DirEntryKind::File, DirtyEntry::Directory) => {
                        entries.insert(name, dir_info(&dirty_path));
                    }
                    (DirEntryKind::File, DirtyEntry::Deleted) => {
                        entries.remove(&name);
                    }
                }
            }
            Ok(entries.into_iter().collect())
        })
    }

    fn with_fs_read<T>(
        &self,
        operation: impl FnOnce(&LaneFs<FileWorktree>) -> Result<T, i32>,
    ) -> Result<T, i32> {
        let fs = self.fs.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        operation(&fs)
    }

    fn flush(&self) -> Result<(), VirtualExecError> {
        let dirty = self
            .dirty
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane dirty map lock poisoned"))?
            .clone();
        if dirty.is_empty() {
            return Ok(());
        }

        with_lane_fs_write(
            &self.repo_root,
            &self.storage_path,
            &self.metrics,
            |latest| {
                latest
                    .create_lane(&self.lane)
                    .map_err(status_from_lane_fs_error)?;
                apply_dirty_entries(
                    latest,
                    &self.lane,
                    dirty.iter().map(|(path, entry)| (path.as_str(), entry)),
                )?;
                Ok(())
            },
        )
    }

    fn collect_changes(&self) -> Result<Vec<VirtualChangeOutput>, VirtualExecError> {
        let dirty = self
            .dirty
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane dirty map lock poisoned"))?
            .clone();
        let fs = self
            .fs
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane session lock poisoned"))?;
        let mut draft = LaneFs::new(fs.repo().clone(), FileWorktree::new(&self.repo_root));
        apply_dirty_entries(
            &mut draft,
            &self.lane,
            dirty.iter().map(|(path, entry)| (path.as_str(), entry)),
        )
        .map_err(|status| VirtualExecError::from_status("collect dirty virtual changes", status))?;
        draft
            .changed_paths(&self.lane)
            .map_err(status_from_lane_fs_error)
            .and_then(|paths| {
                paths
                    .into_iter()
                    .map(|path| change_for_path(&draft, &self.lane, path))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|changes| changes.into_iter().flatten().collect())
            })
            .map_err(|status| VirtualExecError::from_status("collect projected lane paths", status))
    }

    fn projected_paths(&self) -> Result<Vec<FilePath>, VirtualExecError> {
        self.fs
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane session lock poisoned"))?
            .changed_paths(&self.lane)
            .map_err(status_from_lane_fs_error)
            .map_err(|status| VirtualExecError::from_status("collect projected lane paths", status))
    }

    fn worker_changed_paths(&self) -> Result<Vec<FilePath>, VirtualExecError> {
        self.dirty
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane dirty map lock poisoned"))
            .map(|dirty| dirty.keys().cloned().collect())
    }

    fn dirty_entry(&self, path: &str) -> Result<Option<DirtyEntry>, i32> {
        self.dirty
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|dirty| dirty.get(path).cloned())
    }

    fn dirty_entries(&self) -> Result<Vec<(FilePath, DirtyEntry)>, i32> {
        self.dirty
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|dirty| {
                dirty
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect()
            })
    }

    fn dirty_has_visible_children(&self, path: &str) -> Result<bool, i32> {
        let prefix = child_prefix(path);
        self.dirty
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|dirty| {
                dirty.iter().any(|(dirty_path, entry)| {
                    dirty_path.starts_with(&prefix)
                        && matches!(entry, DirtyEntry::File(_) | DirtyEntry::Directory)
                })
            })
    }

    fn path_version(&self, path: &str) -> Result<u64, i32> {
        self.versions
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|versions| versions.get(path).copied().unwrap_or(0))
    }

    fn set_dirty_entry(&self, path: &str, entry: DirtyEntry) -> Result<u64, i32> {
        let mut dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        self.set_dirty_entry_locked(&mut dirty, path.to_owned(), entry)
    }

    fn set_dirty_entry_locked(
        &self,
        dirty: &mut BTreeMap<FilePath, DirtyEntry>,
        path: FilePath,
        entry: DirtyEntry,
    ) -> Result<u64, i32> {
        let version = self.next_version.fetch_add(1, Ordering::Relaxed);
        let mut versions = self.versions.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        dirty.insert(path.clone(), entry);
        versions.insert(path, version);
        Ok(version)
    }
}

fn apply_dirty_entries<'a>(
    fs: &mut LaneFs<FileWorktree>,
    lane: &str,
    dirty: impl IntoIterator<Item = (&'a str, &'a DirtyEntry)>,
) -> Result<(), i32> {
    fs.create_lane(lane).map_err(status_from_lane_fs_error)?;
    for (path, entry) in dirty {
        match entry {
            DirtyEntry::File(bytes) => fs
                .write_file(lane, path, bytes.as_slice())
                .map_err(status_from_lane_fs_error)?,
            DirtyEntry::Directory => {
                if fs
                    .read_file(lane, path)
                    .map_err(status_from_lane_fs_error)?
                    .is_some()
                {
                    fs.delete_file(lane, path)
                        .map_err(status_from_lane_fs_error)?;
                }
            }
            DirtyEntry::Deleted => fs
                .delete_file(lane, path)
                .map_err(status_from_lane_fs_error)?,
        }
    }
    Ok(())
}

fn collect_visible_files(
    fs: &LaneFs<FileWorktree>,
    lane: &str,
    path: &str,
    files: &mut Vec<FilePath>,
) -> Result<(), i32> {
    for entry in fs.list_dir(lane, path).map_err(status_from_lane_fs_error)? {
        let child = child_path(path, &entry.name);
        match entry.kind {
            DirEntryKind::Directory => collect_visible_files(fs, lane, &child, files)?,
            DirEntryKind::File => files.push(child),
        }
    }
    Ok(())
}

fn child_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    }
}

fn dir_entry_for_dirty_path(directory: &str, dirty_path: &str) -> Option<(String, DirEntryKind)> {
    let tail = if directory.is_empty() {
        dirty_path
    } else {
        dirty_path.strip_prefix(&format!("{directory}/"))?
    };
    if tail.is_empty() || tail == dirty_path && !directory.is_empty() {
        return None;
    }
    match tail.split_once('/') {
        Some((name, _)) => Some((name.to_owned(), DirEntryKind::Directory)),
        None => Some((tail.to_owned(), DirEntryKind::File)),
    }
}

fn prepare_session_fs(
    repo_root: &Path,
    storage_path: &Path,
    lane: &str,
    metrics: &VirtualFsMetrics,
) -> Result<LaneFs<FileWorktree>, VirtualExecError> {
    with_lane_fs_write(repo_root, storage_path, metrics, |fs| {
        fs.create_lane(lane).map_err(status_from_lane_fs_error)?;
        Ok(LaneFs::new(fs.repo().clone(), FileWorktree::new(repo_root)))
    })
}

fn with_lane_fs_write<T>(
    repo_root: &Path,
    storage_path: &Path,
    metrics: &VirtualFsMetrics,
    operation: impl FnOnce(&mut LaneFs<FileWorktree>) -> Result<T, i32>,
) -> Result<T, VirtualExecError> {
    let wait_start = Instant::now();
    let lock = acquire_repo_lock(storage_path).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to acquire lane storage lock {}: {error}",
            storage_path.display()
        ))
    })?;
    let wait_ms = elapsed_ms(wait_start);
    let held_start = Instant::now();
    let repo = load_repo(storage_path)
        .map_err(|error| {
            VirtualExecError::message(format!(
                "failed to load lane storage {}: {error}",
                storage_path.display()
            ))
        })?
        .unwrap_or_default();
    let mut fs = LaneFs::new(repo, FileWorktree::new(repo_root));
    let result = operation(&mut fs);
    if result.is_ok() {
        persist_repo(storage_path, fs.repo()).map_err(|error| {
            VirtualExecError::message(format!(
                "failed to persist lane storage {}: {error}",
                storage_path.display()
            ))
        })?;
    }
    let held_ms = elapsed_ms(held_start);
    drop(lock);
    metrics.record_write(wait_ms, held_ms);
    result.map_err(|status| VirtualExecError::from_status("apply lane storage update", status))
}

enum VirtualNode {
    File { path: FilePath, len: u64 },
    Directory { path: FilePath },
}

impl VirtualNode {
    fn is_dir(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }

    fn attributes(&self) -> FileAttributes {
        match self {
            Self::File { .. } => FileAttributes::ARCHIVE,
            Self::Directory { .. } => FileAttributes::DIRECTORY,
        }
    }

    fn file_info(&self) -> FileInfo {
        match self {
            Self::File { path, len } => file_info(path, *len),
            Self::Directory { path } => dir_info(path),
        }
    }
}

fn file_info(path: &str, size: u64) -> FileInfo {
    let mut info = FileInfo::default();
    info.set_file_attributes(FileAttributes::ARCHIVE)
        .set_file_size(size)
        .set_allocation_size(size)
        .set_time(filetime_now())
        .set_index_number(index_number(path))
        .set_hard_links(1);
    info
}

fn dir_info(path: &str) -> FileInfo {
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

fn path_from_winfsp(file_name: &U16CStr) -> Result<FilePath, i32> {
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

fn ensure_mutable_path(path: &str) -> Result<(), i32> {
    if is_git_metadata_path(path) {
        Err(STATUS_ACCESS_DENIED)
    } else {
        Ok(())
    }
}

fn is_lane_state_path(path: &str) -> bool {
    path == ".lane" || path.starts_with(".lane/")
}

fn is_git_metadata_path(path: &str) -> bool {
    path == ".git" || path.starts_with(".git/")
}

fn child_path(parent: &str, child: &str) -> FilePath {
    let child = child.trim_start_matches(['\\', '/']).replace('\\', "/");
    if parent.is_empty() {
        child
    } else {
        format!("{parent}/{child}")
    }
}

fn rename_target_path(from: &str, target: &str) -> FilePath {
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

fn change_for_path(
    fs: &LaneFs<FileWorktree>,
    lane: &str,
    path: impl Into<String>,
) -> Result<Option<VirtualChangeOutput>, i32> {
    let path = path.into();
    let base = fs.base_file(&path).map_err(status_from_lane_fs_error)?;
    let lane_bytes = fs
        .read_file(lane, &path)
        .map_err(status_from_lane_fs_error)?;
    if base == lane_bytes {
        return Ok(None);
    }
    let status = match (&base, &lane_bytes) {
        (None, Some(_)) => VirtualChangeStatus::Created,
        (Some(_), None) => VirtualChangeStatus::Deleted,
        (Some(_), Some(_)) => VirtualChangeStatus::Modified,
        (None, None) => return Ok(None),
    };
    let ops = fs
        .change_ops(lane, &path)
        .map_err(status_from_lane_fs_error)?;
    Ok(Some(VirtualChangeOutput {
        path,
        status,
        base_size: base.as_ref().map(Vec::len),
        lane_size: lane_bytes.as_ref().map(Vec::len),
        ops,
    }))
}

fn status_from_lane_fs_error(error: LaneFsError) -> i32 {
    match error {
        LaneFsError::BadPath(_) => STATUS_ACCESS_DENIED,
        LaneFsError::FileMissing { .. } => STATUS_NO_SUCH_FILE,
        LaneFsError::Io(error) => status_from_io_error(error),
        LaneFsError::Lane(error) => status_from_lane_error(error),
    }
}

fn status_from_lane_error(error: LaneError) -> i32 {
    match error {
        LaneError::LaneMissing(_) | LaneError::BaseMissing { .. } => STATUS_OBJECT_NAME_NOT_FOUND,
        LaneError::BaseChanged { .. } => STATUS_ACCESS_DENIED,
        LaneError::ReservedLane(_) => STATUS_INVALID_PARAMETER,
        LaneError::RangeOutOfBounds { .. }
        | LaneError::OperationOutOfBounds { .. }
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

#[derive(Default)]
struct VirtualFsMetrics {
    storage_lock_wait_ms: AtomicU64,
    storage_lock_held_ms: AtomicU64,
    storage_write_ops: AtomicU64,
}

impl VirtualFsMetrics {
    fn record_write(&self, wait_ms: u64, held_ms: u64) {
        self.storage_lock_wait_ms
            .fetch_add(wait_ms, Ordering::Relaxed);
        self.storage_lock_held_ms
            .fetch_add(held_ms, Ordering::Relaxed);
        self.storage_write_ops.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> VirtualFsMetricsSnapshot {
        VirtualFsMetricsSnapshot {
            storage_lock_wait_ms: self.storage_lock_wait_ms.load(Ordering::Relaxed),
            storage_lock_held_ms: self.storage_lock_held_ms.load(Ordering::Relaxed),
            storage_write_ops: self.storage_write_ops.load(Ordering::Relaxed),
        }
    }
}

struct VirtualFsMetricsSnapshot {
    storage_lock_wait_ms: u64,
    storage_lock_held_ms: u64,
    storage_write_ops: u64,
}

pub(crate) struct VirtualLaneRun {
    pub(crate) output: VirtualExecOutput,
    pub(crate) failed: bool,
}

#[derive(Serialize)]
pub(crate) struct VirtualExecOutput {
    lane: String,
    repo_root: String,
    storage_path: String,
    workspace_root: String,
    mount_path: String,
    mode: &'static str,
    projected_paths: Vec<FilePath>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    worker_error: Option<String>,
    changed_paths: Vec<FilePath>,
    timings: VirtualExecTimings,
    changes: Vec<VirtualChangeOutput>,
}

struct WorkerOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    worker_error: Option<String>,
}

#[derive(Serialize)]
struct VirtualExecTimings {
    total_ms: u64,
    lock_wait_ms: u64,
    lock_held_ms: u64,
    storage_lock_wait_ms: u64,
    storage_lock_held_ms: u64,
    pre_worker_lock_ms: u64,
    worker_ms: u64,
    post_worker_lock_ms: u64,
    mount_ms: u64,
    unmount_ms: u64,
    storage_write_ops: u64,
}

#[derive(Clone, Debug, Serialize)]
struct VirtualChangeOutput {
    path: FilePath,
    status: VirtualChangeStatus,
    base_size: Option<usize>,
    lane_size: Option<usize>,
    ops: Vec<LaneOpSummary>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum VirtualChangeStatus {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug)]
pub(crate) struct VirtualExecError {
    message: String,
}

impl VirtualExecError {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn from_status(operation: &str, status: i32) -> Self {
        Self::message(format!(
            "virtual lane filesystem failed while trying to {operation} with NTSTATUS {status:#x}"
        ))
    }
}

impl fmt::Display for VirtualExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for VirtualExecError {}

impl From<io::Error> for VirtualExecError {
    fn from(error: io::Error) -> Self {
        Self::message(error.to_string())
    }
}

#[cfg(windows)]
fn path_label(path: impl AsRef<Path>) -> String {
    let label = path.as_ref().display().to_string();
    if let Some(path) = label.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = label.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        label
    }
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}
