use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Deref;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct FilePath(String);

impl FilePath {
    pub fn parse(raw: &str) -> Result<Self, LaneError> {
        let label = Self::normalize_label(raw).map_err(LaneError::InvalidPath)?;
        if label.is_empty() {
            Err(LaneError::InvalidPath("missing path".to_owned()))
        } else {
            Ok(Self(label))
        }
    }

    pub(crate) fn parse_label(raw: &str) -> Result<Self, String> {
        Self::normalize_label(raw).map(Self)
    }

    pub(crate) fn from_normalized(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        assert_eq!(
            Self::normalize_label(&raw).as_deref(),
            Ok(raw.as_str()),
            "FilePath::from_normalized requires a normalized repo-relative label"
        );
        Self(raw)
    }

    pub(crate) fn normalize_label(raw: &str) -> Result<String, String> {
        if raw.trim().is_empty() || raw == "." {
            return Ok(String::new());
        }
        if is_absolute_repo_path(raw) {
            return Err("path must be repo-relative".to_owned());
        }

        let normalized = raw.replace('\\', "/");
        let mut parts = Vec::new();
        for part in normalized.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err("path must stay inside the repo".to_owned()),
                part if part.contains('\0') => {
                    return Err("path must stay inside the repo".to_owned());
                }
                part => parts.push(part.to_owned()),
            }
        }

        let label = parts.join("/");
        if is_lane_state_path(&label) {
            return Err("cannot project lane state files".to_owned());
        }
        if is_git_metadata_path(&label) {
            return Err("cannot project git metadata files".to_owned());
        }
        Ok(label)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for FilePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for FilePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Deref for FilePath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for FilePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for FilePath {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for FilePath {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for FilePath {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for FilePath {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl From<FilePath> for String {
    fn from(path: FilePath) -> Self {
        path.0
    }
}

impl<'de> Deserialize<'de> for FilePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct LaneId(String);

impl LaneId {
    pub fn parse(raw: &str) -> Result<Self, LaneError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed != raw {
            Err(LaneError::InvalidLane(raw.to_owned()))
        } else if raw == "base" {
            Err(LaneError::ReservedLane(raw.to_owned()))
        } else {
            Ok(Self(trimmed.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for LaneId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Deref for LaneId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for LaneId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for LaneId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for LaneId {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for LaneId {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<LaneId> for String {
    fn from(lane: LaneId) -> Self {
        lane.0
    }
}

impl<'de> Deserialize<'de> for LaneId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

pub(crate) fn is_lane_state_path(path: &str) -> bool {
    has_repo_root_component(path, ".lane")
}

pub(crate) fn is_git_metadata_path(path: &str) -> bool {
    has_repo_root_component(path, ".git")
}

fn is_absolute_repo_path(path: &str) -> bool {
    Path::new(path).is_absolute() || path.starts_with(['/', '\\']) || has_windows_drive_prefix(path)
}

fn has_windows_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
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
pub struct LaneRunState {
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

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LaneError {
    #[error("invalid repo path: {0}")]
    InvalidPath(String),
    #[error("invalid lane name {0:?}")]
    InvalidLane(String),
    #[error("reserved lane name {0:?}")]
    ReservedLane(String),
    #[error("lane {0:?} does not exist")]
    LaneMissing(String),
    #[error("base file changed outside lane for {path}")]
    BaseChanged { path: String },
    #[error("operation is outside the current base file for {path}")]
    OperationOutOfBounds { path: String },
    #[error("operation conflicts with another selected operation for {path}")]
    OperationConflict { path: String },
    #[error("operation selection cannot be empty")]
    EmptyOperationSelection,
    #[error("invalid operation selection for {path}: {reason}")]
    InvalidOperationSelection { path: String, reason: String },
    #[error("operation {op_id:?} does not exist for {path}")]
    OperationMissing { path: String, op_id: String },
}

impl LaneRunState {
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

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("invalid operation order key")]
    InvalidOrderKey,
    #[error("stored operations conflict")]
    OperationConflict,
    #[error("stored operation is outside the base file")]
    OperationOutOfBounds,
    #[error("stored overlay references missing lane {0:?}")]
    OverlayLaneMissing(LaneId),
    #[error("stored manifest contains reserved lane name {0:?}")]
    ReservedLane(String),
}

pub fn ensure_user_lane(lane: &str) -> Result<(), LaneError> {
    LaneId::parse(lane).map(|_| ())
}

pub(super) fn base_fingerprint(bytes: &[u8]) -> BaseFingerprint {
    let digest = Sha256::digest(bytes);
    let mut fingerprint = [0; BASE_FINGERPRINT_LEN];
    fingerprint.copy_from_slice(&digest);
    fingerprint
}
