use std::error::Error;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::vfs::{LaneFileChange, LaneFileChangeStatus};
use crate::{FilePath, LaneExecState, LaneOpSummary};

#[derive(Clone, Copy, Debug)]
pub(crate) struct VirtualExecOptions {
    pub(crate) observe: bool,
    pub(crate) persist_changes: bool,
}

impl Default for VirtualExecOptions {
    fn default() -> Self {
        Self {
            observe: false,
            persist_changes: true,
        }
    }
}

#[derive(Default)]
pub(super) struct VirtualFsMetrics {
    storage_lock_wait_ms: AtomicU64,
    storage_lock_held_ms: AtomicU64,
    storage_write_ops: AtomicU64,
}

impl VirtualFsMetrics {
    pub(super) fn record_write(&self, wait_ms: u64, held_ms: u64) {
        self.storage_lock_wait_ms
            .fetch_add(wait_ms, Ordering::Relaxed);
        self.storage_lock_held_ms
            .fetch_add(held_ms, Ordering::Relaxed);
        self.storage_write_ops.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> VirtualFsMetricsSnapshot {
        VirtualFsMetricsSnapshot {
            storage_lock_wait_ms: self.storage_lock_wait_ms.load(Ordering::Relaxed),
            storage_lock_held_ms: self.storage_lock_held_ms.load(Ordering::Relaxed),
            storage_write_ops: self.storage_write_ops.load(Ordering::Relaxed),
        }
    }
}

pub(super) struct VirtualFsMetricsSnapshot {
    pub(super) storage_lock_wait_ms: u64,
    pub(super) storage_lock_held_ms: u64,
    pub(super) storage_write_ops: u64,
}

pub(crate) struct VirtualLaneRun {
    pub(crate) output: VirtualExecOutput,
    pub(crate) failed: bool,
}

impl VirtualLaneRun {
    pub(crate) fn into_record(self) -> VirtualExecRecord {
        self.output.into_record()
    }
}

#[derive(Serialize)]
pub(crate) struct VirtualExecOutput {
    pub(super) lane: String,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) workspace_root: String,
    pub(super) mount_path: String,
    pub(super) mode: &'static str,
    pub(super) projected_paths: Vec<FilePath>,
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) worker_error: Option<String>,
    pub(super) changed_paths: Vec<FilePath>,
    pub(super) timings: VirtualExecTimings,
    pub(super) changes: Vec<VirtualChangeOutput>,
    pub(super) warnings: Vec<VirtualExecWarning>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct VirtualExecRecord {
    pub(crate) exec: LaneExecState,
    pub(crate) total_ms: u64,
    pub(crate) change_count: usize,
    pub(crate) warnings: Vec<String>,
}

impl VirtualExecOutput {
    fn into_record(self) -> VirtualExecRecord {
        VirtualExecRecord {
            exec: LaneExecState::new(
                self.exit_code,
                self.worker_error,
                &self.stdout,
                &self.stderr,
                self.changed_paths,
            ),
            total_ms: self.timings.total_ms,
            change_count: self.changes.len(),
            warnings: self
                .warnings
                .into_iter()
                .map(|warning| warning.message)
                .collect(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(super) struct VirtualExecWarning {
    pub(super) kind: &'static str,
    pub(super) message: String,
}

#[derive(Serialize)]
pub(super) struct VirtualExecTimings {
    pub(super) total_ms: u64,
    pub(super) lock_wait_ms: u64,
    pub(super) lock_held_ms: u64,
    pub(super) storage_lock_wait_ms: u64,
    pub(super) storage_lock_held_ms: u64,
    pub(super) pre_worker_lock_ms: u64,
    pub(super) worker_ms: u64,
    pub(super) post_worker_lock_ms: u64,
    pub(super) mount_ms: u64,
    pub(super) unmount_ms: u64,
    pub(super) storage_write_ops: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct VirtualChangeOutput {
    path: FilePath,
    status: LaneFileChangeStatus,
    base_size: Option<usize>,
    lane_size: Option<usize>,
    ops: Vec<LaneOpSummary>,
}

impl From<LaneFileChange> for VirtualChangeOutput {
    fn from(change: LaneFileChange) -> Self {
        Self {
            path: change.path,
            status: change.status,
            base_size: change.base_size,
            lane_size: change.lane_size,
            ops: change.ops,
        }
    }
}

#[derive(Debug)]
pub(crate) struct VirtualExecError {
    message: String,
}

impl VirtualExecError {
    pub(super) fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(super) fn from_status(operation: &str, status: i32) -> Self {
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
