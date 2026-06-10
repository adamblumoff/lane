use std::collections::{BTreeMap, BTreeSet};

use super::ops::{
    FileOp, ParsedOpId, RebaseOpSet, base_slice_for_op, delete_op_id_for, diff_to_ops,
    ensure_delete_selection, entries_conflict, next_file_op_id, normalize_ops_checked, op_id_for,
    op_kind, operation_missing, ops_conflict, order_key, parse_lane_op_id,
    rebase_ops_after_promotion, render_ops, selected_present_ops, validate_ops,
};
use super::types::base_fingerprint;
use super::{
    BaseFingerprint, BaseStorageSnapshot, DecodeError, FilePath, LaneEntryStorageSnapshot,
    LaneError, LaneExecState, LaneFileStorageSnapshot, LaneId, LaneOpDetail, LaneOpKind,
    LaneOpSummary, LaneRepoStorageSnapshot, ensure_user_lane,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRepo {
    lanes: BTreeSet<LaneId>,
    last_exec: BTreeMap<LaneId, LaneExecState>,
    files: BTreeMap<FilePath, LaneFile>,
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
