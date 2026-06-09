use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::{Algorithm, DiffTag, capture_diff_slices};

mod cli;
pub mod storage;
mod vfs;
#[cfg(windows)]
pub mod virtual_exec;

pub use cli::{CliError, run};

pub type FilePath = String;
pub type LaneId = String;

const BASE_FINGERPRINT_LEN: usize = 32;
const EXEC_OUTPUT_PREVIEW_LIMIT: usize = 4096;
const ORDER_ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub type BaseFingerprint = [u8; BASE_FINGERPRINT_LEN];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRepo {
    lanes: BTreeSet<LaneId>,
    last_exec: BTreeMap<LaneId, LaneExecState>,
    files: BTreeMap<FilePath, LaneFile>,
}

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
struct LaneFile {
    base: BaseState,
    lanes: BTreeMap<LaneId, LaneEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BaseState {
    Present(BaseFingerprint),
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LaneEntry {
    Present(LaneView),
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LaneView {
    ops: Vec<FileOp>,
}

struct LanePromotionSnapshot {
    lane: LaneId,
    entry: LaneEntry,
    bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileOp {
    id: u64,
    base_start: u64,
    base_len: u64,
    order_key: String,
    inserted: Vec<u8>,
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

impl LaneRepo {
    pub fn new() -> Self {
        Self {
            lanes: BTreeSet::new(),
            last_exec: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    pub fn lane_ids(&self) -> impl Iterator<Item = &str> {
        self.lanes.iter().map(String::as_str)
    }

    pub fn overlay_paths(&self, lane: &str) -> Result<Vec<&str>, LaneError> {
        self.ensure_lane(lane)?;
        Ok(self
            .files
            .iter()
            .filter_map(|(path, file)| file.has_lane(lane).then_some(path.as_str()))
            .collect())
    }

    pub fn create_lane(&mut self, lane: impl Into<LaneId>) -> Result<bool, LaneError> {
        let lane = lane.into();
        ensure_user_lane(&lane)?;
        Ok(self.lanes.insert(lane))
    }

    pub fn record_last_exec(&mut self, lane: &str, state: LaneExecState) -> Result<(), LaneError> {
        self.ensure_lane(lane)?;
        self.last_exec.insert(lane.to_owned(), state);
        Ok(())
    }

    pub fn last_exec(&self, lane: &str) -> Result<Option<&LaneExecState>, LaneError> {
        self.ensure_lane(lane)?;
        Ok(self.last_exec.get(lane))
    }

    pub fn discard_lane(&mut self, lane: &str) -> bool {
        let removed = self.lanes.remove(lane);
        self.last_exec.remove(lane);
        for file in self.files.values_mut() {
            file.discard_lane(lane);
        }
        self.files.retain(|_, file| !file.is_empty());
        removed
    }

    pub fn read_path(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        if lane == "base" {
            return Ok(base.map(<[u8]>::to_vec));
        }
        self.ensure_lane(lane)?;
        match self.files.get(path) {
            Some(file) => file.read(path, lane, base),
            None => Ok(base.map(<[u8]>::to_vec)),
        }
    }

    pub fn change_ops(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Vec<LaneOpSummary>, LaneError> {
        self.ensure_lane(lane)?;
        let Some(file) = self.files.get(path) else {
            return Ok(Vec::new());
        };
        file.change_ops(path, lane, base)
    }

    pub fn op_detail(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        op_id: &str,
    ) -> Result<LaneOpDetail, LaneError> {
        self.ensure_lane(lane)?;
        let Some(file) = self.files.get(path) else {
            return Err(operation_missing(path, op_id));
        };
        file.op_detail(path, lane, base, op_id)
    }

    pub fn replace_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        content: Option<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.ensure_lane(lane)?;
        if let Some(file) = self.files.get_mut(path) {
            file.replace(path, lane, base, content)?;
            if file.is_empty() {
                self.files.remove(path);
            }
            return Ok(());
        }

        let mut file = LaneFile::new(base);
        file.replace(path, lane, base, content)?;
        if !file.is_empty() {
            self.files.insert(path.to_owned(), file);
        }
        Ok(())
    }

    pub fn delete_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<(), LaneError> {
        self.replace_path(path, lane, base, None)
    }

    pub fn promote_ops_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        op_ids: &[String],
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_lane(lane)?;
        if op_ids.is_empty() {
            return Err(LaneError::EmptyOperationSelection);
        }
        let Some(file) = self.files.get_mut(path) else {
            return Err(LaneError::OperationMissing {
                path: path.to_owned(),
                op_id: op_ids[0].clone(),
            });
        };

        let promoted = file.promote_ops(path, lane, base, op_ids)?;
        if file.is_empty() {
            self.files.remove(path);
        }
        Ok(promoted)
    }

    pub fn resolve_op_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        op_id: &str,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_lane(lane)?;
        let Some(file) = self.files.get_mut(path) else {
            return Err(operation_missing(path, op_id));
        };

        let promoted = file.resolve_op(path, lane, base, op_id, replacement.into())?;
        if file.is_empty() {
            self.files.remove(path);
        }
        Ok(promoted)
    }

    pub fn storage_snapshot(&self) -> LaneRepoStorageSnapshot {
        LaneRepoStorageSnapshot {
            lanes: self.lanes.clone(),
            last_exec: self.last_exec.clone(),
            files: self
                .files
                .iter()
                .map(|(path, file)| (path.clone(), file.storage_snapshot()))
                .collect(),
        }
    }

    pub fn from_storage_snapshot(snapshot: LaneRepoStorageSnapshot) -> Result<Self, DecodeError> {
        for lane in &snapshot.lanes {
            ensure_user_lane(lane).map_err(|_| DecodeError::ReservedLane(lane.clone()))?;
        }

        let repo = Self {
            lanes: snapshot.lanes,
            last_exec: snapshot.last_exec,
            files: snapshot
                .files
                .into_iter()
                .map(|(path, file)| file.into_lane_file().map(|file| (path, file)))
                .collect::<Result<_, _>>()?,
        };
        repo.validate()?;
        Ok(repo)
    }

    fn ensure_lane(&self, lane: &str) -> Result<(), LaneError> {
        if self.lanes.contains(lane) {
            Ok(())
        } else {
            Err(LaneError::LaneMissing(lane.to_owned()))
        }
    }

    fn validate(&self) -> Result<(), DecodeError> {
        for lane in self.last_exec.keys() {
            if !self.lanes.contains(lane) {
                return Err(DecodeError::ExecStateLaneMissing(lane.clone()));
            }
        }
        for file in self.files.values() {
            for lane in file.lanes.keys() {
                if !self.lanes.contains(lane) {
                    return Err(DecodeError::OverlayLaneMissing(lane.clone()));
                }
            }
            file.validate()?;
        }
        Ok(())
    }
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

impl BaseState {
    fn for_content(content: Option<&[u8]>) -> Self {
        match content {
            Some(bytes) => Self::Present(base_fingerprint(bytes)),
            None => Self::Missing,
        }
    }
}

impl Default for LaneRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl LaneFile {
    fn new(base: Option<&[u8]>) -> Self {
        Self {
            base: BaseState::for_content(base),
            lanes: BTreeMap::new(),
        }
    }

    fn read(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_base(path, base)?;
        match self.lanes.get(lane) {
            Some(LaneEntry::Present(view)) => {
                render_ops(path, base.unwrap_or_default(), &view.ops).map(Some)
            }
            Some(LaneEntry::Deleted) => Ok(None),
            None => Ok(base.map(<[u8]>::to_vec)),
        }
    }

    fn change_ops(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Vec<LaneOpSummary>, LaneError> {
        self.ensure_base(path, base)?;
        let Some(entry) = self.lanes.get(lane) else {
            return Ok(Vec::new());
        };
        Ok(self.summarize_entry(path, lane, entry, base))
    }

    fn op_detail(
        &self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        op_id: &str,
    ) -> Result<LaneOpDetail, LaneError> {
        self.ensure_base(path, base)?;
        let Some(entry) = self.lanes.get(lane) else {
            return Err(operation_missing(path, op_id));
        };

        match entry {
            LaneEntry::Present(view) => {
                let Some(ParsedOpId::Present(id)) = parse_lane_op_id(lane, op_id) else {
                    return Err(operation_missing(path, op_id));
                };
                let Some(op) = view.ops.iter().find(|op| op.id == id) else {
                    return Err(operation_missing(path, op_id));
                };
                Ok(LaneOpDetail {
                    summary: self.summarize_op(path, lane, op, base),
                    base: base_slice_for_op(path, base, op)?,
                    inserted: op.inserted.clone(),
                })
            }
            LaneEntry::Deleted => {
                if parse_lane_op_id(lane, op_id) != Some(ParsedOpId::Delete) {
                    return Err(operation_missing(path, op_id));
                }
                let summary = self
                    .summarize_entry(path, lane, entry, base)
                    .into_iter()
                    .next()
                    .ok_or_else(|| operation_missing(path, op_id))?;
                Ok(LaneOpDetail {
                    summary,
                    base: base.unwrap_or_default().to_vec(),
                    inserted: Vec::new(),
                })
            }
        }
    }

    fn replace(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        content: Option<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.ensure_base(path, base)?;
        let entry = entry_for_content(base, content);
        match entry {
            Some(entry) => {
                self.lanes.insert(lane.to_owned(), entry);
            }
            None => {
                self.lanes.remove(lane);
            }
        };
        Ok(())
    }

    fn promote_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        op_ids: &[String],
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_base(path, base)?;
        if op_ids.is_empty() {
            return Err(LaneError::EmptyOperationSelection);
        }
        let Some(entry) = self.lanes.get(lane).cloned() else {
            return Err(LaneError::OperationMissing {
                path: path.to_owned(),
                op_id: op_ids[0].clone(),
            });
        };

        let selected_ops = match entry {
            LaneEntry::Present(view) => selected_present_ops(path, lane, &view.ops, op_ids)?,
            LaneEntry::Deleted => {
                ensure_delete_selection(path, lane, op_ids)?;
                return self.promote_resolved_content(path, lane, base, None);
            }
        };
        let selected_ids = selected_ops.iter().map(|op| op.id).collect::<BTreeSet<_>>();
        self.promote_selected_present_ops(path, lane, base, selected_ops, &selected_ids)
    }

    fn resolve_op(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        op_id: &str,
        replacement: Vec<u8>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_base(path, base)?;
        let Some(entry) = self.lanes.get(lane).cloned() else {
            return Err(operation_missing(path, op_id));
        };
        let LaneEntry::Present(view) = entry else {
            if parse_lane_op_id(lane, op_id) != Some(ParsedOpId::Delete) {
                return Err(operation_missing(path, op_id));
            }
            return self.promote_resolved_content(path, lane, base, Some(replacement));
        };
        let Some(ParsedOpId::Present(id)) = parse_lane_op_id(lane, op_id) else {
            return Err(operation_missing(path, op_id));
        };
        let Some(target) = view.ops.iter().find(|op| op.id == id).cloned() else {
            return Err(operation_missing(path, op_id));
        };

        let mut resolved = target;
        resolved.id = next_file_op_id(&view.ops);
        resolved.inserted = replacement;

        let selected_ids = [id].into_iter().collect::<BTreeSet<_>>();
        self.promote_selected_present_ops(path, lane, base, vec![resolved], &selected_ids)
    }

    fn promote_resolved_content(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        promoted: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.rebuild_after_base_promotion(
            path,
            base,
            promoted,
            |lane_id, _entry, old_bytes, promoted_base| {
                if lane_id == lane {
                    Ok(None)
                } else {
                    Ok(entry_for_content(promoted_base, old_bytes))
                }
            },
        )
    }

    fn promote_selected_present_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        selected_ops: Vec<FileOp>,
        selected_ids: &BTreeSet<u64>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        let promoted = Some(render_ops(path, base.unwrap_or_default(), &selected_ops)?);
        self.rebuild_after_base_promotion(
            path,
            base,
            promoted,
            |lane_id, entry, old_bytes, promoted_base| match entry {
                LaneEntry::Present(view) if lane_id == lane => {
                    let retained_ops = view
                        .ops
                        .iter()
                        .filter(|op| !selected_ids.contains(&op.id))
                        .cloned()
                        .collect::<Vec<_>>();
                    if retained_ops.is_empty() {
                        Ok(None)
                    } else {
                        rebased_entry_for_present_ops(
                            path,
                            base,
                            promoted_base,
                            old_bytes,
                            RebaseOpSet {
                                lane,
                                ops: &selected_ops,
                            },
                            RebaseOpSet {
                                lane: lane_id,
                                ops: &retained_ops,
                            },
                        )
                    }
                }
                LaneEntry::Present(view) => rebased_entry_for_present_ops(
                    path,
                    base,
                    promoted_base,
                    old_bytes,
                    RebaseOpSet {
                        lane,
                        ops: &selected_ops,
                    },
                    RebaseOpSet {
                        lane: lane_id,
                        ops: &view.ops,
                    },
                ),
                LaneEntry::Deleted => Ok(entry_for_content(promoted_base, old_bytes)),
            },
        )
    }

    fn rebuild_after_base_promotion(
        &mut self,
        path: &str,
        base: Option<&[u8]>,
        promoted: Option<Vec<u8>>,
        mut next_entry: impl FnMut(
            &str,
            LaneEntry,
            Option<Vec<u8>>,
            Option<&[u8]>,
        ) -> Result<Option<LaneEntry>, LaneError>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        let snapshots = self.promotion_snapshots(path, base)?;
        let promoted_base = promoted.as_deref();

        self.base = BaseState::for_content(promoted_base);
        self.lanes.clear();

        for snapshot in snapshots {
            if let Some(entry) = next_entry(
                &snapshot.lane,
                snapshot.entry,
                snapshot.bytes,
                promoted_base,
            )? {
                self.lanes.insert(snapshot.lane, entry);
            }
        }

        Ok(promoted)
    }

    fn promotion_snapshots(
        &self,
        path: &str,
        base: Option<&[u8]>,
    ) -> Result<Vec<LanePromotionSnapshot>, LaneError> {
        self.lanes
            .iter()
            .map(|(lane, entry)| {
                self.read(path, lane, base)
                    .map(|bytes| LanePromotionSnapshot {
                        lane: lane.clone(),
                        entry: entry.clone(),
                        bytes,
                    })
            })
            .collect()
    }

    fn discard_lane(&mut self, lane: &str) {
        self.lanes.remove(lane);
    }

    fn has_lane(&self, lane: &str) -> bool {
        self.lanes.contains_key(lane)
    }

    fn is_empty(&self) -> bool {
        self.lanes.is_empty()
    }

    fn ensure_base(&self, path: &str, base: Option<&[u8]>) -> Result<(), LaneError> {
        if self.base == BaseState::for_content(base) {
            Ok(())
        } else {
            Err(LaneError::BaseChanged {
                path: path.to_owned(),
            })
        }
    }

    fn validate(&self) -> Result<(), DecodeError> {
        for entry in self.lanes.values() {
            let LaneEntry::Present(view) = entry else {
                continue;
            };
            validate_ops(&view.ops)?;
        }
        Ok(())
    }

    fn summarize_entry(
        &self,
        path: &str,
        lane: &str,
        entry: &LaneEntry,
        base: Option<&[u8]>,
    ) -> Vec<LaneOpSummary> {
        match entry {
            LaneEntry::Present(view) => view
                .ops
                .iter()
                .map(|op| self.summarize_op(path, lane, op, base))
                .collect(),
            LaneEntry::Deleted => vec![LaneOpSummary {
                op_id: delete_op_id_for(lane),
                lane: lane.to_owned(),
                path: path.to_owned(),
                kind: LaneOpKind::Delete,
                base_start: 0,
                base_end: base.map(|bytes| bytes.len() as u64).unwrap_or(0),
                inserted_len: 0,
                order_key: format!("00000000000000000000:{lane}:delete"),
                conflicts_with: self
                    .lanes
                    .iter()
                    .filter_map(|(other_lane, other_entry)| {
                        (other_lane != lane && entry_conflicts_with_delete(other_entry, base))
                            .then_some(other_lane.clone())
                    })
                    .collect(),
            }],
        }
    }

    fn summarize_op(
        &self,
        path: &str,
        lane: &str,
        op: &FileOp,
        base: Option<&[u8]>,
    ) -> LaneOpSummary {
        let base_missing = base.is_none();
        LaneOpSummary {
            op_id: op_id_for(lane, op),
            lane: lane.to_owned(),
            path: path.to_owned(),
            kind: op_kind(op, base_missing),
            base_start: op.base_start,
            base_end: op.base_start + op.base_len,
            inserted_len: op.inserted.len() as u64,
            order_key: order_key(lane, op),
            conflicts_with: self.conflicts_for_op(lane, op, base_missing),
        }
    }

    fn conflicts_for_op(&self, lane: &str, op: &FileOp, base_missing: bool) -> Vec<LaneId> {
        self.lanes
            .iter()
            .filter_map(|(other_lane, other_entry)| {
                if other_lane == lane {
                    return None;
                }
                entry_conflicts_with_op(other_entry, op, base_missing).then_some(other_lane.clone())
            })
            .collect()
    }
}

impl LaneFile {
    fn storage_snapshot(&self) -> LaneFileStorageSnapshot {
        LaneFileStorageSnapshot {
            base: match self.base {
                BaseState::Present(fingerprint) => BaseStorageSnapshot::Present(fingerprint),
                BaseState::Missing => BaseStorageSnapshot::Missing,
            },
            lanes: self
                .lanes
                .iter()
                .map(|(lane, entry)| (lane.clone(), entry.storage_snapshot()))
                .collect(),
        }
    }
}

impl LaneFileStorageSnapshot {
    fn into_lane_file(self) -> Result<LaneFile, DecodeError> {
        let file = LaneFile {
            base: match self.base {
                BaseStorageSnapshot::Present(fingerprint) => BaseState::Present(fingerprint),
                BaseStorageSnapshot::Missing => BaseState::Missing,
            },
            lanes: self
                .lanes
                .into_iter()
                .map(|(lane, entry)| entry.into_lane_entry().map(|entry| (lane, entry)))
                .collect::<Result<_, _>>()?,
        };
        file.validate()?;
        Ok(file)
    }
}

impl LaneEntry {
    fn storage_snapshot(&self) -> LaneEntryStorageSnapshot {
        match self {
            LaneEntry::Present(view) => LaneEntryStorageSnapshot::Present(
                view.ops.iter().map(FileOp::storage_snapshot).collect(),
            ),
            LaneEntry::Deleted => LaneEntryStorageSnapshot::Deleted,
        }
    }
}

impl LaneEntryStorageSnapshot {
    fn into_lane_entry(self) -> Result<LaneEntry, DecodeError> {
        match self {
            LaneEntryStorageSnapshot::Present(ops) => Ok(LaneEntry::Present(LaneView {
                ops: normalize_ops_checked(
                    ops.into_iter().map(FileOp::from_storage_snapshot).collect(),
                )?,
            })),
            LaneEntryStorageSnapshot::Deleted => Ok(LaneEntry::Deleted),
        }
    }
}

impl FileOp {
    fn storage_snapshot(&self) -> FileOpStorageSnapshot {
        FileOpStorageSnapshot {
            id: self.id,
            base_start: self.base_start,
            base_len: self.base_len,
            order_key: self.order_key.clone(),
            inserted: self.inserted.clone(),
        }
    }

    fn from_storage_snapshot(snapshot: FileOpStorageSnapshot) -> Self {
        Self {
            id: snapshot.id,
            base_start: snapshot.base_start,
            base_len: snapshot.base_len,
            order_key: snapshot.order_key,
            inserted: snapshot.inserted,
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

fn entry_for_content(base: Option<&[u8]>, content: Option<Vec<u8>>) -> Option<LaneEntry> {
    if content.as_deref() == base {
        return None;
    }
    match content {
        Some(bytes) => Some(LaneEntry::Present(LaneView {
            ops: diff_to_ops(base.unwrap_or_default(), &bytes, base.is_none()),
        })),
        None if base.is_some() => Some(LaneEntry::Deleted),
        None => None,
    }
}

fn diff_to_ops(base: &[u8], content: &[u8], base_missing: bool) -> Vec<FileOp> {
    if base_missing || is_probably_binary(base) || is_probably_binary(content) {
        return coarse_replace_ops(base, content);
    }

    let mut ops = Vec::new();
    let mut order_key = None;
    for diff_op in capture_diff_slices(Algorithm::Myers, base, content) {
        let (tag, old_range, new_range) = diff_op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let id = ops.len() as u64 + 1;
        let next_order_key = next_fractional_key(order_key.as_deref());
        ops.push(FileOp {
            id,
            base_start: old_range.start as u64,
            base_len: (old_range.end - old_range.start) as u64,
            order_key: next_order_key.clone(),
            inserted: content[new_range].to_vec(),
        });
        order_key = Some(next_order_key);
    }
    ops
}

fn coarse_replace_ops(base: &[u8], content: &[u8]) -> Vec<FileOp> {
    if base == content {
        Vec::new()
    } else {
        vec![FileOp {
            id: 1,
            base_start: 0,
            base_len: base.len() as u64,
            order_key: first_fractional_key(),
            inserted: content.to_vec(),
        }]
    }
}

fn render_ops(path: &str, base: &[u8], ops: &[FileOp]) -> Result<Vec<u8>, LaneError> {
    let mut rendered = Vec::new();
    let mut cursor = 0usize;
    for op in sorted_ops(ops) {
        let start =
            usize::try_from(op.base_start).map_err(|_| LaneError::OperationOutOfBounds {
                path: path.to_owned(),
            })?;
        let len = usize::try_from(op.base_len).map_err(|_| LaneError::OperationOutOfBounds {
            path: path.to_owned(),
        })?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| LaneError::OperationOutOfBounds {
                path: path.to_owned(),
            })?;
        if start < cursor || end > base.len() {
            return Err(LaneError::OperationConflict {
                path: path.to_owned(),
            });
        }
        rendered.extend_from_slice(&base[cursor..start]);
        rendered.extend_from_slice(&op.inserted);
        cursor = end;
    }
    rendered.extend_from_slice(&base[cursor..]);
    Ok(rendered)
}

fn sorted_ops(ops: &[FileOp]) -> Vec<&FileOp> {
    let mut sorted = ops.iter().collect::<Vec<_>>();
    sorted.sort_by(|left, right| {
        left.base_start
            .cmp(&right.base_start)
            .then(left.base_len.cmp(&right.base_len))
            .then(left.order_key.cmp(&right.order_key))
            .then(left.id.cmp(&right.id))
    });
    sorted
}

fn rebased_entry_for_present_ops(
    path: &str,
    old_base: Option<&[u8]>,
    promoted_base: Option<&[u8]>,
    fallback_bytes: Option<Vec<u8>>,
    promoted: RebaseOpSet<'_>,
    retained: RebaseOpSet<'_>,
) -> Result<Option<LaneEntry>, LaneError> {
    let Some(promoted_base) = promoted_base else {
        return Ok(entry_for_content(promoted_base, fallback_bytes));
    };
    if old_base.is_none()
        || retained.ops.is_empty()
        || entries_conflict(promoted.ops, retained.ops, false)
    {
        return Ok(entry_for_content(Some(promoted_base), fallback_bytes));
    }

    let rebased_ops = rebase_ops_after_promotion(path, retained, promoted)?;
    render_ops(path, promoted_base, &rebased_ops)?;

    Ok(Some(LaneEntry::Present(LaneView { ops: rebased_ops })))
}

#[derive(Clone, Copy)]
struct RebaseOpSet<'a> {
    lane: &'a str,
    ops: &'a [FileOp],
}

fn rebase_ops_after_promotion(
    path: &str,
    retained: RebaseOpSet<'_>,
    promoted: RebaseOpSet<'_>,
) -> Result<Vec<FileOp>, LaneError> {
    let promoted_ops = sorted_ops(promoted.ops);
    retained
        .ops
        .iter()
        .map(|op| {
            let mut base_start = i128::from(op.base_start);
            for promoted_op in &promoted_ops {
                if promoted_op_shifts_start(retained.lane, op, promoted.lane, promoted_op) {
                    base_start +=
                        promoted_op.inserted.len() as i128 - i128::from(promoted_op.base_len);
                }
            }

            let mut rebased = op.clone();
            rebased.base_start =
                u64::try_from(base_start).map_err(|_| LaneError::OperationOutOfBounds {
                    path: path.to_owned(),
                })?;
            Ok(rebased)
        })
        .collect()
}

fn promoted_op_shifts_start(
    retained_lane: &str,
    op: &FileOp,
    promoted_lane: &str,
    promoted_op: &FileOp,
) -> bool {
    if promoted_op.base_len == 0 {
        match promoted_op.base_start.cmp(&op.base_start) {
            Ordering::Less => true,
            Ordering::Equal => {
                compare_ops_for_render(promoted_lane, promoted_op, retained_lane, op)
                    == Ordering::Less
            }
            Ordering::Greater => false,
        }
    } else {
        promoted_op.base_start + promoted_op.base_len <= op.base_start
    }
}

fn compare_ops_for_render(
    left_lane: &str,
    left: &FileOp,
    right_lane: &str,
    right: &FileOp,
) -> Ordering {
    left.base_start
        .cmp(&right.base_start)
        .then(left.base_len.cmp(&right.base_len))
        .then(left.order_key.cmp(&right.order_key))
        .then(left_lane.cmp(right_lane))
        .then(left.id.cmp(&right.id))
}

fn selected_present_ops(
    path: &str,
    lane: &str,
    ops: &[FileOp],
    op_ids: &[String],
) -> Result<Vec<FileOp>, LaneError> {
    let mut selected = BTreeSet::new();
    for op_id in op_ids {
        match parse_lane_op_id(lane, op_id) {
            Some(ParsedOpId::Present(id)) if ops.iter().any(|op| op.id == id) => {
                selected.insert(id);
            }
            _ => {
                return Err(operation_missing(path, op_id));
            }
        }
    }

    Ok(ops
        .iter()
        .filter(|op| selected.contains(&op.id))
        .cloned()
        .collect())
}

fn base_slice_for_op(path: &str, base: Option<&[u8]>, op: &FileOp) -> Result<Vec<u8>, LaneError> {
    let base = base.unwrap_or_default();
    let start = usize::try_from(op.base_start).map_err(|_| LaneError::OperationOutOfBounds {
        path: path.to_owned(),
    })?;
    let len = usize::try_from(op.base_len).map_err(|_| LaneError::OperationOutOfBounds {
        path: path.to_owned(),
    })?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| LaneError::OperationOutOfBounds {
            path: path.to_owned(),
        })?;
    if end > base.len() {
        return Err(LaneError::OperationOutOfBounds {
            path: path.to_owned(),
        });
    }
    Ok(base[start..end].to_vec())
}

fn next_file_op_id(ops: &[FileOp]) -> u64 {
    ops.iter().map(|op| op.id).max().unwrap_or(0) + 1
}

fn ensure_delete_selection(path: &str, lane: &str, op_ids: &[String]) -> Result<(), LaneError> {
    for op_id in op_ids {
        if parse_lane_op_id(lane, op_id) != Some(ParsedOpId::Delete) {
            return Err(operation_missing(path, op_id));
        }
    }
    Ok(())
}

fn operation_missing(path: &str, op_id: &str) -> LaneError {
    LaneError::OperationMissing {
        path: path.to_owned(),
        op_id: op_id.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ParsedOpId {
    Present(u64),
    Delete,
}

fn parse_lane_op_id(lane: &str, op_id: &str) -> Option<ParsedOpId> {
    let suffix = match op_id.rsplit_once(':') {
        Some((op_lane, suffix)) if op_lane == lane => suffix,
        Some(_) => return None,
        None => op_id,
    };
    if suffix == "delete" {
        Some(ParsedOpId::Delete)
    } else {
        suffix.parse().ok().map(ParsedOpId::Present)
    }
}

fn entries_conflict(left: &[FileOp], right: &[FileOp], base_missing: bool) -> bool {
    left.iter().any(|left_op| {
        right
            .iter()
            .any(|right_op| ops_conflict(left_op, right_op, base_missing))
    })
}

fn entry_conflicts_with_op(entry: &LaneEntry, op: &FileOp, base_missing: bool) -> bool {
    match entry {
        LaneEntry::Deleted => !base_missing,
        LaneEntry::Present(view) => view
            .ops
            .iter()
            .any(|other| ops_conflict(op, other, base_missing)),
    }
}

fn entry_conflicts_with_delete(entry: &LaneEntry, base: Option<&[u8]>) -> bool {
    match entry {
        LaneEntry::Deleted => false,
        LaneEntry::Present(view) => base.is_some() && !view.ops.is_empty(),
    }
}

fn ops_conflict(left: &FileOp, right: &FileOp, base_missing: bool) -> bool {
    if base_missing {
        return true;
    }
    let left_range = op_range(left);
    let right_range = op_range(right);
    if left.base_len == 0 && right.base_len == 0 {
        return false;
    }
    if left.base_len == 0 {
        return right_range.start < left_range.start && left_range.start < right_range.end;
    }
    if right.base_len == 0 {
        return left_range.start < right_range.start && right_range.start < left_range.end;
    }
    left_range.start < right_range.end && right_range.start < left_range.end
}

fn op_range(op: &FileOp) -> Range<u64> {
    op.base_start..op.base_start + op.base_len
}

fn op_kind(op: &FileOp, base_missing: bool) -> LaneOpKind {
    if base_missing {
        LaneOpKind::Create
    } else if op.base_len == 0 {
        LaneOpKind::Insert
    } else if op.inserted.is_empty() {
        LaneOpKind::Delete
    } else {
        LaneOpKind::Replace
    }
}

fn op_id_for(lane: &str, op: &FileOp) -> String {
    format!("{lane}:{}", op.id)
}

fn delete_op_id_for(lane: &str) -> String {
    format!("{lane}:delete")
}

fn order_key(lane: &str, op: &FileOp) -> String {
    format!(
        "{:020}:{}:{lane}:{:020}",
        op.base_start, op.order_key, op.id
    )
}

fn first_fractional_key() -> String {
    fractional_key_between(None, None)
}

fn next_fractional_key(left: Option<&str>) -> String {
    fractional_key_between(left, None)
}

fn fractional_key_between(left: Option<&str>, right: Option<&str>) -> String {
    let left = left.map(order_key_digits).unwrap_or_default();
    let right = right.map(order_key_digits).unwrap_or_default();
    let mut prefix = Vec::new();
    let mut index = 0;
    let max_digit = ORDER_ALPHABET.len() - 1;

    loop {
        let left_digit = left.get(index).copied().unwrap_or(0);
        let right_digit = right.get(index).copied().unwrap_or(max_digit);
        if right_digit > left_digit + 1 {
            prefix.push((left_digit + right_digit) / 2);
            return digits_to_order_key(&prefix);
        }

        prefix.push(left_digit);
        index += 1;
    }
}

fn order_key_digits(key: &str) -> Vec<usize> {
    key.bytes()
        .map(|byte| {
            ORDER_ALPHABET
                .iter()
                .position(|candidate| *candidate == byte)
                .expect("order key must be validated before use")
        })
        .collect()
}

fn digits_to_order_key(digits: &[usize]) -> String {
    digits
        .iter()
        .map(|digit| char::from(ORDER_ALPHABET[*digit]))
        .collect()
}

fn is_valid_order_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|byte| ORDER_ALPHABET.contains(&byte))
}

fn is_probably_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn validate_ops(ops: &[FileOp]) -> Result<(), DecodeError> {
    let mut cursor = 0u64;
    for op in sorted_ops(ops) {
        let end = op
            .base_start
            .checked_add(op.base_len)
            .ok_or(DecodeError::OperationOutOfBounds)?;
        if !is_valid_order_key(&op.order_key) {
            return Err(DecodeError::InvalidOrderKey);
        }
        if op.base_start < cursor {
            return Err(DecodeError::OperationConflict);
        }
        cursor = end;
    }
    Ok(())
}

fn normalize_ops_checked(ops: Vec<FileOp>) -> Result<Vec<FileOp>, DecodeError> {
    validate_ops(&ops)?;
    Ok(ops
        .into_iter()
        .filter(|op| op.base_len > 0 || !op.inserted.is_empty())
        .collect())
}

pub fn ensure_user_lane(lane: &str) -> Result<(), LaneError> {
    if lane.trim().is_empty() || lane == "base" {
        Err(LaneError::ReservedLane(lane.to_owned()))
    } else {
        Ok(())
    }
}

fn base_fingerprint(bytes: &[u8]) -> BaseFingerprint {
    let digest = Sha256::digest(bytes);
    let mut fingerprint = [0; BASE_FINGERPRINT_LEN];
    fingerprint.copy_from_slice(&digest);
    fingerprint
}
