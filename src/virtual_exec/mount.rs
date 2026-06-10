use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{STATUS_ACCESS_DENIED, STATUS_OBJECT_NAME_COLLISION};
use winfsp_wrs::{
    FileSystem, OperationGuardStrategy, Params, SecurityDescriptor, U16CString, u16cstr,
};

use super::fs::VirtualLaneFs;
use super::state::{VirtualLaneState, prepare_session_fs};
use super::types::{VirtualExecError, VirtualFsMetrics};

const MOUNT_READY_ATTEMPTS: usize = 40;
const MOUNT_READY_DELAY: Duration = Duration::from_millis(25);

pub(super) fn start_mount(
    repo_root: &Path,
    storage_path: &Path,
    lane: &str,
    metrics: Arc<VirtualFsMetrics>,
) -> Result<(MountPoint, FileSystem, Arc<VirtualLaneState>), VirtualExecError> {
    let security =
        SecurityDescriptor::from_wstr(u16cstr!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"))
            .map_err(VirtualExecError::message)?;
    let session_fs = prepare_session_fs(repo_root, storage_path, lane, &metrics)?;
    let state = Arc::new(VirtualLaneState::new(
        repo_root,
        storage_path,
        lane,
        session_fs,
        security,
        metrics,
    ));
    let mut last_unavailable = None;
    for letter in (b'D'..=b'Z').rev().map(char::from) {
        let Some(mount_point) = try_allocate_mount_point(letter)? else {
            continue;
        };
        let context = VirtualLaneFs::new(state.clone());
        let params = winfsp_params()?;
        match FileSystem::start(params, Some(mount_point.mount_name.as_ucstr()), context) {
            Ok(file_system) => {
                wait_for_mount_ready(&mount_point.workspace_path)?;
                return Ok((mount_point, file_system, state));
            }
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

fn wait_for_mount_ready(workspace_path: &Path) -> Result<(), VirtualExecError> {
    for _ in 0..MOUNT_READY_ATTEMPTS {
        match workspace_path.try_exists() {
            Ok(true) => return Ok(()),
            Ok(false) | Err(_) => thread::sleep(MOUNT_READY_DELAY),
        }
    }
    Err(VirtualExecError::message(format!(
        "WinFsp mount {} did not become visible",
        workspace_path.display()
    )))
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

pub(super) struct MountPoint {
    pub(super) workspace_path: PathBuf,
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
