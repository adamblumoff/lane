// Explicit paths keep the storage contract test able to recompile this facade
// from outside the normal src/ module tree.
#[path = "storage/atomic.rs"]
mod atomic;
#[path = "storage/blobs.rs"]
mod blobs;
#[path = "storage/cleanup.rs"]
mod cleanup;
#[path = "storage/db.rs"]
mod db;
#[path = "storage/doctor.rs"]
mod doctor;
#[path = "storage/lock.rs"]
mod lock;
#[path = "storage/paths.rs"]
mod paths;
#[path = "storage/serde_util.rs"]
mod serde_util;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::{LaneId, LaneRepo, LaneRunState};

pub(crate) use atomic::persist_bytes;
pub(crate) use cleanup::cleanup_storage;
pub(crate) use doctor::{StorageDoctorReport, doctor_storage};
#[cfg(test)]
#[allow(
    unused_imports,
    reason = "unused in the crate test target but used when storage_contract recompiles this facade"
)]
pub(crate) use lock::is_lock_contention;
pub(crate) use lock::{RepoLock, acquire_repo_lock};
pub(crate) use paths::encode_path_component;

use db::{load_db_snapshot, persist_db_snapshot};
use paths::db_path;
use serde_util::{invalid_storage, json_error};

pub(crate) fn load_repo(storage_root: &Path) -> io::Result<Option<LaneRepo>> {
    let db_path = db_path(storage_root);
    let snapshot = load_db_snapshot(storage_root)?;
    LaneRepo::from_storage_snapshot(match snapshot {
        Some(snapshot) => snapshot,
        None => return Ok(None),
    })
    .map(Some)
    .map_err(|error| invalid_storage(&db_path, error))
}

pub(crate) fn persist_repo(storage_root: &Path, repo: &LaneRepo) -> io::Result<()> {
    let snapshot = repo.storage_snapshot();
    fs::create_dir_all(storage_root)?;

    persist_db_snapshot(storage_root, &snapshot)?;
    Ok(())
}

pub(crate) fn persist_last_run(
    storage_root: &Path,
    lane: &str,
    state: &LaneRunState,
) -> io::Result<()> {
    let state_json = serde_json::to_string(state).map_err(json_error)?;
    db::persist_last_run_json(storage_root, lane, &state_json)
}

pub(crate) fn load_last_run(
    storage_root: &Path,
    lanes: &BTreeSet<LaneId>,
) -> BTreeMap<LaneId, LaneRunState> {
    let Ok(rows) = db::load_last_run_jsons(storage_root, lanes) else {
        return BTreeMap::new();
    };
    rows.into_iter()
        .filter_map(|(lane, state_json)| {
            serde_json::from_str(&state_json)
                .ok()
                .map(|state| (lane, state))
        })
        .collect()
}

pub(crate) fn persist_run_record(
    storage_root: &Path,
    name: &str,
    record_json: &str,
) -> io::Result<()> {
    db::persist_run_record_json(storage_root, name, record_json)
}

pub(crate) fn load_run_record(storage_root: &Path, name: &str) -> io::Result<Option<String>> {
    db::load_run_record_json(storage_root, name)
}

pub(crate) fn load_run_records(storage_root: &Path) -> io::Result<BTreeMap<String, String>> {
    db::load_run_record_jsons(storage_root)
}

pub(crate) fn delete_run_record(storage_root: &Path, name: &str) -> io::Result<bool> {
    db::delete_run_record(storage_root, name)
}

pub(crate) fn run_record_exists(storage_root: &Path, name: &str) -> io::Result<bool> {
    db::run_record_exists(storage_root, name)
}
