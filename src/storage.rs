// Explicit paths keep the storage contract test able to recompile this facade
// from outside the normal src/ module tree.
#[path = "storage/atomic.rs"]
mod atomic;
#[path = "storage/blobs.rs"]
mod blobs;
#[path = "storage/doctor.rs"]
mod doctor;
#[path = "storage/lock.rs"]
mod lock;
#[path = "storage/manifest.rs"]
mod manifest;
#[path = "storage/paths.rs"]
mod paths;
#[path = "storage/serde_util.rs"]
mod serde_util;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::{LaneExecState, LaneId, LaneRepo};

#[allow(unused_imports)]
pub(crate) use atomic::persist_bytes;
#[allow(unused_imports)]
pub(crate) use doctor::{StorageDoctorReport, doctor_storage};
#[allow(unused_imports)]
pub(crate) use lock::{RepoLock, acquire_repo_lock, is_lock_contention};
#[allow(unused_imports)]
pub(crate) use paths::encode_path_component;

use manifest::{load_manifest_snapshot, persist_manifest_snapshot};
use paths::{last_exec_file_name, last_exec_path, manifest_path};
use serde_util::{invalid_storage, json_error};

pub(crate) fn load_repo(storage_root: &Path) -> io::Result<Option<LaneRepo>> {
    let manifest_path = manifest_path(storage_root);
    let snapshot = load_manifest_snapshot(storage_root, &manifest_path)?;
    LaneRepo::from_storage_snapshot(match snapshot {
        Some(mut snapshot) => {
            snapshot.last_exec = load_last_exec(storage_root, &snapshot.lanes);
            snapshot
        }
        None => return Ok(None),
    })
    .map(Some)
    .map_err(|error| invalid_storage(&manifest_path, error))
}

pub(crate) fn persist_repo(storage_root: &Path, repo: &LaneRepo) -> io::Result<()> {
    let snapshot = repo.storage_snapshot();
    fs::create_dir_all(storage_root)?;

    persist_manifest_snapshot(storage_root, &manifest_path(storage_root), &snapshot)?;
    prune_stale_last_exec_files(storage_root, &snapshot.lanes);
    Ok(())
}

pub(crate) fn persist_last_exec(
    storage_root: &Path,
    lane: &str,
    state: &LaneExecState,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).map_err(json_error)?;
    persist_bytes(&last_exec_path(storage_root, lane), &bytes)
}

fn load_last_exec(
    storage_root: &Path,
    lanes: &BTreeSet<LaneId>,
) -> BTreeMap<LaneId, LaneExecState> {
    lanes
        .iter()
        .filter_map(|lane| {
            let path = last_exec_path(storage_root, lane);
            let bytes = fs::read(path).ok()?;
            let state = serde_json::from_slice(&bytes).ok()?;
            Some((lane.clone(), state))
        })
        .collect()
}

fn prune_stale_last_exec_files(storage_root: &Path, lanes: &BTreeSet<LaneId>) {
    let last_exec_dir = storage_root.join("last_exec");
    let expected = lanes
        .iter()
        .map(|lane| last_exec_file_name(lane))
        .collect::<BTreeSet<_>>();

    // last_exec is advisory; failed cleanup must not block repo persistence.
    if let Ok(entries) = fs::read_dir(&last_exec_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !expected.contains(file_name) {
                let _ = fs::remove_file(path);
            }
        }
    }
}
