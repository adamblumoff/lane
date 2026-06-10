use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type FilePath = String;
pub type LaneId = String;

pub(crate) fn is_lane_state_path(path: &str) -> bool {
    has_repo_root_component(path, ".lane")
}

pub(crate) fn is_git_metadata_path(path: &str) -> bool {
    has_repo_root_component(path, ".git")
}

fn has_repo_root_component(path: &str, component: &str) -> bool {
    path.split('/')
        .next()
        .is_some_and(|first| first.eq_ignore_ascii_case(component))
}

const BASE_FINGERPRINT_LEN: usize = 32;
const EXEC_OUTPUT_PREVIEW_LIMIT: usize = 4096;

pub type BaseFingerprint = [u8; BASE_FINGERPRINT_LEN];

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LaneExecState {
    pub exit_code: Option<i32>,
    pub worker_error: Option<String>,
    pub stdout: LaneTextPreview,
    pub stderr: LaneTextPreview,
    pub changed_paths: Vec<FilePath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct LaneTextPreview {
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LaneOpSummary {
    pub op_id: String,
    pub lane: LaneId,
    pub path: FilePath,
    pub kind: LaneOpKind,
    pub base_start: u64,
    pub base_end: u64,
    pub inserted_len: u64,
    pub order_key: String,
    pub conflicts_with: Vec<LaneId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneOpDetail {
    pub summary: LaneOpSummary,
    pub base: Vec<u8>,
    pub inserted: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneOpKind {
    Create,
    Insert,
    Delete,
    Replace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRepoStorageSnapshot {
    pub lanes: BTreeSet<LaneId>,
    pub last_exec: BTreeMap<LaneId, LaneExecState>,
    pub files: BTreeMap<FilePath, LaneFileStorageSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneFileStorageSnapshot {
    pub base: BaseStorageSnapshot,
    pub lanes: BTreeMap<LaneId, LaneEntryStorageSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaseStorageSnapshot {
    Present(BaseFingerprint),
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaneEntryStorageSnapshot {
    Present(Vec<FileOpStorageSnapshot>),
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileOpStorageSnapshot {
    pub id: u64,
    pub base_start: u64,
    pub base_len: u64,
    pub order_key: String,
    pub inserted: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaneError {
    ReservedLane(LaneId),
    LaneMissing(LaneId),
    BaseChanged { path: FilePath },
    OperationOutOfBounds { path: FilePath },
    OperationConflict { path: FilePath },
    EmptyOperationSelection,
    OperationMissing { path: FilePath, op_id: String },
}

impl LaneExecState {
    pub fn new(
        exit_code: Option<i32>,
        worker_error: Option<String>,
        stdout: &str,
        stderr: &str,
        changed_paths: Vec<FilePath>,
    ) -> Self {
        Self {
            exit_code,
            worker_error,
            stdout: LaneTextPreview::from_text(stdout),
            stderr: LaneTextPreview::from_text(stderr),
            changed_paths,
        }
    }
}

impl LaneTextPreview {
    fn from_text(text: &str) -> Self {
        let mut end = text.len();
        let mut truncated = false;
        if text.len() > EXEC_OUTPUT_PREVIEW_LIMIT {
            truncated = true;
            end = 0;
            for (index, character) in text.char_indices() {
                let next = index + character.len_utf8();
                if next > EXEC_OUTPUT_PREVIEW_LIMIT {
                    break;
                }
                end = next;
            }
        }

        Self {
            text: text[..end].to_owned(),
            truncated,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    InvalidOrderKey,
    OperationConflict,
    OperationOutOfBounds,
    OverlayLaneMissing(LaneId),
    ExecStateLaneMissing(LaneId),
    ReservedLane(LaneId),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for DecodeError {}

pub fn ensure_user_lane(lane: &str) -> Result<(), LaneError> {
    if lane.trim().is_empty() || lane == "base" {
        Err(LaneError::ReservedLane(lane.to_owned()))
    } else {
        Ok(())
    }
}

pub(super) fn base_fingerprint(bytes: &[u8]) -> BaseFingerprint {
    let digest = Sha256::digest(bytes);
    let mut fingerprint = [0; BASE_FINGERPRINT_LEN];
    fingerprint.copy_from_slice(&digest);
    fingerprint
}
