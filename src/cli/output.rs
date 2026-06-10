use serde::Serialize;

use crate::vfs::{LaneFileChange, LaneFileChangeStatus};
use crate::{FilePath, LaneOpSummary};

#[derive(Serialize)]
pub(super) struct CreateOutput<'a> {
    pub(super) lane: &'a str,
    pub(super) created: bool,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
}

#[derive(Serialize)]
pub(super) struct ReviewOutput {
    pub(super) lane: Option<String>,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) summary: ReviewSummary,
    pub(super) lanes: Vec<ReviewLaneSummary>,
    pub(super) paths: Vec<ReviewPathOutput>,
}

#[derive(Serialize)]
pub(super) struct DoctorOutput {
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) healthy: bool,
    pub(super) report: crate::storage::StorageDoctorReport,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewSummary {
    pub(super) lanes: usize,
    pub(super) changed_paths: usize,
    pub(super) clean_ops: usize,
    pub(super) conflicted_ops: usize,
    pub(super) conflict_groups: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewLaneSummary {
    pub(super) lane: String,
    pub(super) changed_paths: usize,
    pub(super) clean_ops: usize,
    pub(super) conflicted_ops: usize,
    pub(super) last_exec: Option<crate::LaneExecState>,
    pub(super) actions: Vec<ReviewActionOutput>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewPathOutput {
    pub(super) path: FilePath,
    pub(super) lanes: Vec<ReviewLaneOutput>,
    pub(super) clean_ops: Vec<ReviewOpOutput>,
    pub(super) conflicts: Vec<ReviewConflictOutput>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewLaneOutput {
    pub(super) lane: String,
    pub(super) status: LaneFileChangeStatus,
    pub(super) base_size: Option<usize>,
    pub(super) lane_size: Option<usize>,
    pub(super) total_ops: usize,
    pub(super) clean_ops: usize,
    pub(super) conflicted_ops: usize,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewConflictOutput {
    pub(super) range_start: u64,
    pub(super) range_end: u64,
    pub(super) lanes: Vec<String>,
    pub(super) actions: Vec<ReviewActionOutput>,
    pub(super) ops: Vec<ReviewOpOutput>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewActionOutput {
    pub(super) kind: ReviewActionKind,
    pub(super) command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) lane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) path: Option<FilePath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) op_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) required_inputs: Vec<ReviewActionInput>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ReviewActionKind {
    PromoteClean,
    ShowOp,
    ResolveOp,
    Discard,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewActionInput {
    pub(super) name: &'static str,
    pub(super) placeholder: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ReviewOpOutput {
    pub(super) op: LaneOpSummary,
    pub(super) base: BytePreview,
    pub(super) inserted: BytePreview,
}

#[derive(Serialize)]
pub(super) struct ShowOpOutput<'a> {
    pub(super) lane: &'a str,
    pub(super) path: &'a str,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) op: LaneOpSummary,
    pub(super) base: BytePreview,
    pub(super) inserted: BytePreview,
}

#[derive(Serialize)]
pub(super) struct ResolveOpOutput<'a> {
    pub(super) lane: &'a str,
    pub(super) path: &'a str,
    pub(super) op_id: &'a str,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) replacement_file: String,
    pub(super) resolved_op: LaneOpSummary,
    pub(super) replacement: BytePreview,
    pub(super) remaining: Vec<ChangeOutput>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BytePreview {
    pub(super) len: usize,
    pub(super) sha256: String,
    pub(super) utf8: Option<String>,
    pub(super) truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ChangeOutput {
    pub(super) path: FilePath,
    pub(super) status: LaneFileChangeStatus,
    pub(super) base_size: Option<usize>,
    pub(super) lane_size: Option<usize>,
    pub(super) ops: Vec<LaneOpSummary>,
    #[serde(skip_serializing)]
    pub(super) base: Option<Vec<u8>>,
    #[serde(skip_serializing)]
    pub(super) lane: Option<Vec<u8>>,
}

impl From<LaneFileChange> for ChangeOutput {
    fn from(change: LaneFileChange) -> Self {
        Self {
            path: change.path,
            status: change.status,
            base_size: change.base_size,
            lane_size: change.lane_size,
            ops: change.ops,
            base: change.base_bytes,
            lane: change.lane_bytes,
        }
    }
}

#[derive(Serialize)]
pub(super) struct PromoteOpsOutput<'a> {
    pub(super) lane: &'a str,
    pub(super) path: &'a str,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) promoted_ops: Vec<String>,
    pub(super) promoted: Vec<ChangeOutput>,
}

#[derive(Serialize)]
pub(super) struct PromoteCleanOutput<'a> {
    pub(super) lane: &'a str,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
    pub(super) promoted_ops: Vec<PathOpsOutput>,
    pub(super) promoted: Vec<ChangeOutput>,
    pub(super) conflicts: Vec<ChangeOutput>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PathOpsOutput {
    pub(super) path: FilePath,
    pub(super) ops: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct DiscardOutput<'a> {
    pub(super) lane: &'a str,
    pub(super) removed: bool,
    pub(super) discarded_changes: usize,
    pub(super) repo_root: String,
    pub(super) storage_path: String,
}
