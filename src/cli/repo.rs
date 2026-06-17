use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::storage::{RepoLock, acquire_repo_lock, load_last_run, load_repo, persist_repo};
use crate::vfs::{FileWorktree, LaneFs, LaneFsError};
use crate::{LaneId, LaneRepo, LaneRunState};

use super::error::{CliError, CliResult};

const STORAGE_PATH: &str = ".lane";

pub(super) use crate::path_label;

pub(super) struct LockedLaneFs {
    pub(super) storage_path: PathBuf,
    pub(super) fs: LaneFs,
    pub(super) last_run: BTreeMap<LaneId, LaneRunState>,
    _lock: RepoLock,
}

pub(super) fn persist_lane_repo(
    storage_path: &Path,
) -> impl FnOnce(&LaneRepo) -> Result<(), LaneFsError> + '_ {
    move |repo| persist_repo(storage_path, repo).map_err(LaneFsError::Io)
}

impl LockedLaneFs {
    pub(super) fn persist(&self) -> CliResult<()> {
        persist_repo(&self.storage_path, self.fs.repo())?;
        Ok(())
    }
}

pub(super) fn open_locked_lane_fs(repo_root: &Path) -> CliResult<LockedLaneFs> {
    let storage_path = storage_path(repo_root);
    let lock = acquire_repo_lock(&storage_path)?;
    let repo = load_lane_repo(&storage_path)?;
    let lanes = repo
        .lane_ids()
        .map(LaneId::parse)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let last_run = load_last_run(&storage_path, &lanes);
    Ok(LockedLaneFs {
        storage_path,
        fs: LaneFs::new(repo, FileWorktree::new(repo_root)),
        last_run,
        _lock: lock,
    })
}

pub(super) fn load_lane_repo(storage_path: &Path) -> CliResult<LaneRepo> {
    Ok(load_repo(storage_path)?.unwrap_or_default())
}

pub(super) fn repo_root(repo_root: PathBuf) -> CliResult<PathBuf> {
    let path = if repo_root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        repo_root
    };
    let root = fs::canonicalize(&path).map_err(|error| {
        CliError::message(format!(
            "repo root {} is not readable: {error}",
            path.display()
        ))
    })?;
    if !root.is_dir() {
        return Err(CliError::message(format!(
            "repo root {} is not a directory",
            root.display()
        )));
    }
    Ok(root)
}

pub(super) fn storage_path(repo_root: &Path) -> PathBuf {
    repo_root.join(STORAGE_PATH)
}

pub(super) fn print_json(output: &impl Serialize) -> CliResult<()> {
    println!("{}", serde_json::to_string(output)?);
    Ok(())
}
