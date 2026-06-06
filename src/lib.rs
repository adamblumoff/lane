use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Range;

use serde::Serialize;
use similar::{Algorithm, DiffTag, capture_diff_slices};

pub mod cli;
pub mod storage;
pub mod vfs;
#[cfg(windows)]
pub(crate) mod virtual_exec;

pub type FilePath = String;
pub type LaneId = String;

const STORAGE_MAGIC: &[u8] = b"LANEREPO\0\0\0\x03";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaneRepo {
    lanes: BTreeSet<LaneId>,
    files: BTreeMap<FilePath, LaneFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromotedFile {
    pub path: FilePath,
    pub bytes: Option<Vec<u8>>,
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
    Present(u64),
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileOp {
    id: u64,
    base_start: u64,
    base_len: u64,
    order: u64,
    inserted: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaneError {
    ReservedLane(LaneId),
    LaneMissing(LaneId),
    BaseMissing { path: FilePath },
    BaseChanged { path: FilePath },
    RangeOutOfBounds { start: u64, end: u64, len: u64 },
    OperationOutOfBounds { path: FilePath },
    OperationConflict { path: FilePath },
}

impl LaneRepo {
    pub fn new() -> Self {
        Self {
            lanes: BTreeSet::new(),
            files: BTreeMap::new(),
        }
    }

    pub fn lane_ids(&self) -> impl Iterator<Item = &str> {
        self.lanes.iter().map(String::as_str)
    }

    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
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

    pub fn discard_lane(&mut self, lane: &str) -> bool {
        let removed = self.lanes.remove(lane);
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

    pub fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.read_path(path, lane, Some(base))?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
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

    pub fn write_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        let replacement = replacement.into();
        let mut current = self.read_path(path, lane, base)?.unwrap_or_else(Vec::new);
        ensure_valid_range(range.clone(), current.len() as u64)?;

        let start: usize = range
            .start
            .try_into()
            .map_err(|_| LaneError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len: current.len() as u64,
            })?;
        let end: usize = range
            .end
            .try_into()
            .map_err(|_| LaneError::RangeOutOfBounds {
                start: range.start,
                end: range.end,
                len: current.len() as u64,
            })?;
        current.splice(start..end, replacement);
        self.replace_path(path, lane, base, Some(current))
    }

    pub fn write(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.write_path(path, lane, Some(base), range, replacement)
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

    pub fn replace(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        content: impl Into<Vec<u8>>,
    ) -> Result<(), LaneError> {
        self.replace_path(path, lane, Some(base), Some(content.into()))
    }

    pub fn delete_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<(), LaneError> {
        self.replace_path(path, lane, base, None)
    }

    pub fn delete(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
    ) -> Result<(), LaneError> {
        self.write_path(path, lane, Some(base), range, Vec::new())
    }

    pub fn promote_path(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_lane(lane)?;
        let Some(file) = self.files.get_mut(path) else {
            return Ok(base.map(<[u8]>::to_vec));
        };

        let promoted = file.promote(path, lane, base)?;
        if file.is_empty() {
            self.files.remove(path);
        }
        Ok(promoted)
    }

    pub fn promote(&mut self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.promote_path(path, lane, Some(base))?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
    }

    pub fn promote_lane(
        &mut self,
        lane: &str,
        bases: impl IntoIterator<Item = (FilePath, Option<Vec<u8>>)>,
    ) -> Result<Vec<PromotedFile>, LaneError> {
        let base_by_path: BTreeMap<_, _> = bases.into_iter().collect();
        let mut changed_bases = Vec::new();
        for path in self.overlay_paths(lane)? {
            let base = base_by_path
                .get(path)
                .ok_or_else(|| LaneError::BaseMissing {
                    path: path.to_owned(),
                })?;
            if self.read_path(path, lane, base.as_deref())? != *base {
                changed_bases.push((path.to_owned(), base.clone()));
            }
        }
        self.promote_paths(lane, changed_bases)
    }

    pub fn promote_paths(
        &mut self,
        lane: &str,
        bases: impl IntoIterator<Item = (FilePath, Option<Vec<u8>>)>,
    ) -> Result<Vec<PromotedFile>, LaneError> {
        self.ensure_lane(lane)?;
        let mut draft = self.clone();
        let mut promoted = Vec::new();

        for (path, base) in bases {
            promoted.push(PromotedFile {
                bytes: draft.promote_path(&path, lane, base.as_deref())?,
                path,
            });
        }

        *self = draft;
        Ok(promoted)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(STORAGE_MAGIC);

        write_u64(&mut bytes, self.lanes.len() as u64);
        for lane in &self.lanes {
            write_bytes(&mut bytes, lane.as_bytes());
        }

        write_u64(&mut bytes, self.files.len() as u64);
        for (path, file) in &self.files {
            write_bytes(&mut bytes, path.as_bytes());
            match file.base {
                BaseState::Present(hash) => {
                    bytes.push(1);
                    write_u64(&mut bytes, hash);
                }
                BaseState::Missing => {
                    bytes.push(0);
                    write_u64(&mut bytes, 0);
                }
            }

            write_u64(&mut bytes, file.lanes.len() as u64);
            for (lane, entry) in &file.lanes {
                write_bytes(&mut bytes, lane.as_bytes());
                match entry {
                    LaneEntry::Deleted => bytes.push(0),
                    LaneEntry::Present(view) => {
                        bytes.push(1);
                        write_u64(&mut bytes, view.ops.len() as u64);
                        for op in &view.ops {
                            write_u64(&mut bytes, op.id);
                            write_u64(&mut bytes, op.base_start);
                            write_u64(&mut bytes, op.base_len);
                            write_u64(&mut bytes, op.order);
                            write_bytes(&mut bytes, &op.inserted);
                        }
                    }
                }
            }
        }

        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut cursor = Cursor::new(bytes);
        cursor.expect(STORAGE_MAGIC)?;

        let mut lanes = BTreeSet::new();
        for _ in 0..cursor.read_u64()? {
            lanes.insert(read_string(&mut cursor)?);
        }

        let mut files = BTreeMap::new();
        for _ in 0..cursor.read_u64()? {
            let path = read_string(&mut cursor)?;
            let base = match cursor.read_byte()? {
                0 => {
                    cursor.read_u64()?;
                    BaseState::Missing
                }
                1 => BaseState::Present(cursor.read_u64()?),
                tag => return Err(DecodeError::InvalidBase(tag)),
            };

            let mut overlays = BTreeMap::new();
            for _ in 0..cursor.read_u64()? {
                let lane = read_string(&mut cursor)?;
                let entry = match cursor.read_byte()? {
                    0 => LaneEntry::Deleted,
                    1 => {
                        let mut ops = Vec::new();
                        for _ in 0..cursor.read_u64()? {
                            ops.push(FileOp {
                                id: cursor.read_u64()?,
                                base_start: cursor.read_u64()?,
                                base_len: cursor.read_u64()?,
                                order: cursor.read_u64()?,
                                inserted: cursor.read_bytes()?.to_vec(),
                            });
                        }
                        LaneEntry::Present(LaneView {
                            ops: normalize_ops_checked(ops)?,
                        })
                    }
                    tag => return Err(DecodeError::InvalidEntry(tag)),
                };
                overlays.insert(lane, entry);
            }

            files.insert(
                path,
                LaneFile {
                    base,
                    lanes: overlays,
                },
            );
        }

        let repo = Self { lanes, files };
        repo.validate()?;
        if !cursor.is_finished() {
            return Err(DecodeError::TrailingBytes);
        }
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
            Some(bytes) => Self::Present(hash_bytes(bytes)),
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

    fn promote(
        &mut self,
        path: &str,
        lane: &str,
        base: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, LaneError> {
        self.ensure_base(path, base)?;
        let promoted = self.read(path, lane, base)?;
        let promoted_entry = self.lanes.get(lane).cloned();
        let old_entries = self.lanes.clone();
        let old_views = old_entries
            .iter()
            .map(|(lane_id, entry)| {
                self.read(path, lane_id, base)
                    .map(|bytes| (lane_id.clone(), entry.clone(), bytes))
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.base = BaseState::for_content(promoted.as_deref());
        self.lanes.clear();

        for (lane_id, entry, old_bytes) in old_views {
            let content = if lane_id == lane {
                promoted.clone()
            } else if let (
                Some(LaneEntry::Present(promoted_view)),
                LaneEntry::Present(view),
                Some(old_base),
            ) = (&promoted_entry, &entry, base)
            {
                if entries_conflict(&promoted_view.ops, &view.ops, false) {
                    old_bytes
                } else {
                    Some(render_ops(
                        path,
                        old_base,
                        &combined_ops(&promoted_view.ops, &view.ops),
                    )?)
                }
            } else {
                old_bytes
            };

            if let Some(next_entry) = entry_for_content(promoted.as_deref(), content) {
                self.lanes.insert(lane_id, next_entry);
            }
        }

        Ok(promoted)
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
                op_id: format!("{lane}:delete"),
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
            op_id: format!("{lane}:{}", op.id),
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    BadMagic,
    UnexpectedEof,
    InvalidUtf8,
    InvalidBase(u8),
    InvalidEntry(u8),
    OperationConflict,
    OperationOutOfBounds,
    OverlayLaneMissing(LaneId),
    TrailingBytes,
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
    for diff_op in capture_diff_slices(Algorithm::Myers, base, content) {
        let (tag, old_range, new_range) = diff_op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        let id = ops.len() as u64 + 1;
        ops.push(FileOp {
            id,
            base_start: old_range.start as u64,
            base_len: (old_range.end - old_range.start) as u64,
            order: id,
            inserted: content[new_range].to_vec(),
        });
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
            order: 1,
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
            .then(left.order.cmp(&right.order))
            .then(left.id.cmp(&right.id))
    });
    sorted
}

fn combined_ops(left: &[FileOp], right: &[FileOp]) -> Vec<FileOp> {
    let mut ops = left.to_vec();
    ops.extend_from_slice(right);
    ops
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
        LaneEntry::Present(view) => base
            .map(|bytes| !bytes.is_empty() && !view.ops.is_empty())
            .unwrap_or(!view.ops.is_empty()),
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

fn order_key(lane: &str, op: &FileOp) -> String {
    format!("{:020}:{lane}:{:020}", op.base_start, op.order)
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

fn ensure_user_lane(lane: &str) -> Result<(), LaneError> {
    if lane.trim().is_empty() || lane == "base" {
        Err(LaneError::ReservedLane(lane.to_owned()))
    } else {
        Ok(())
    }
}

fn ensure_valid_range(range: Range<u64>, len: u64) -> Result<(), LaneError> {
    if range.start > range.end || range.end > len {
        Err(LaneError::RangeOutOfBounds {
            start: range.start,
            end: range.end,
            len,
        })
    } else {
        Ok(())
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn read_string(cursor: &mut Cursor<'_>) -> Result<String, DecodeError> {
    String::from_utf8(cursor.read_bytes()?.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
}

fn write_bytes(target: &mut Vec<u8>, bytes: &[u8]) {
    write_u64(target, bytes.len() as u64);
    target.extend_from_slice(bytes);
}

fn write_u64(target: &mut Vec<u8>, value: u64) {
    target.extend_from_slice(&value.to_le_bytes());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn expect(&mut self, expected: &[u8]) -> Result<(), DecodeError> {
        let actual = self.take(expected.len())?;
        if actual == expected {
            Ok(())
        } else {
            Err(DecodeError::BadMagic)
        }
    }

    fn read_byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn read_u64(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn read_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.read_u64()? as usize;
        self.take(len)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(DecodeError::UnexpectedEof)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(DecodeError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    fn is_finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}
