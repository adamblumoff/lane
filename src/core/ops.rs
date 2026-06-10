use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ops::Range;

use similar::{Algorithm, DiffTag, capture_diff_slices};

use super::{DecodeError, FileOpStorageSnapshot, LaneError, LaneOpKind};

const ORDER_ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FileOp {
    pub(super) id: u64,
    pub(super) base_start: u64,
    pub(super) base_len: u64,
    pub(super) order_key: String,
    pub(super) inserted: Vec<u8>,
}

impl FileOp {
    pub(super) fn storage_snapshot(&self) -> FileOpStorageSnapshot {
        FileOpStorageSnapshot {
            id: self.id,
            base_start: self.base_start,
            base_len: self.base_len,
            order_key: self.order_key.clone(),
            inserted: self.inserted.clone(),
        }
    }

    pub(super) fn from_storage_snapshot(snapshot: FileOpStorageSnapshot) -> Self {
        Self {
            id: snapshot.id,
            base_start: snapshot.base_start,
            base_len: snapshot.base_len,
            order_key: snapshot.order_key,
            inserted: snapshot.inserted,
        }
    }
}

pub(super) fn diff_to_ops(base: &[u8], content: &[u8], base_missing: bool) -> Vec<FileOp> {
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

pub(super) fn render_ops(path: &str, base: &[u8], ops: &[FileOp]) -> Result<Vec<u8>, LaneError> {
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

#[derive(Clone, Copy)]
pub(super) struct RebaseOpSet<'a> {
    pub(super) lane: &'a str,
    pub(super) ops: &'a [FileOp],
}

pub(super) fn rebase_ops_after_promotion(
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

pub(super) fn selected_present_ops(
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

pub(super) fn base_slice_for_op(
    path: &str,
    base: Option<&[u8]>,
    op: &FileOp,
) -> Result<Vec<u8>, LaneError> {
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

pub(super) fn next_file_op_id(ops: &[FileOp]) -> u64 {
    ops.iter().map(|op| op.id).max().unwrap_or(0) + 1
}

pub(super) fn ensure_delete_selection(
    path: &str,
    lane: &str,
    op_ids: &[String],
) -> Result<(), LaneError> {
    for op_id in op_ids {
        if parse_lane_op_id(lane, op_id) != Some(ParsedOpId::Delete) {
            return Err(operation_missing(path, op_id));
        }
    }
    Ok(())
}

pub(super) fn operation_missing(path: &str, op_id: &str) -> LaneError {
    LaneError::OperationMissing {
        path: path.to_owned(),
        op_id: op_id.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ParsedOpId {
    Present(u64),
    Delete,
}

pub(super) fn parse_lane_op_id(lane: &str, op_id: &str) -> Option<ParsedOpId> {
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

pub(super) fn entries_conflict(left: &[FileOp], right: &[FileOp], base_missing: bool) -> bool {
    left.iter().any(|left_op| {
        right
            .iter()
            .any(|right_op| ops_conflict(left_op, right_op, base_missing))
    })
}

pub(super) fn ops_conflict(left: &FileOp, right: &FileOp, base_missing: bool) -> bool {
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

pub(super) fn op_kind(op: &FileOp, base_missing: bool) -> LaneOpKind {
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

pub(super) fn op_id_for(lane: &str, op: &FileOp) -> String {
    format!("{lane}:{}", op.id)
}

pub(super) fn delete_op_id_for(lane: &str) -> String {
    format!("{lane}:delete")
}

pub(super) fn order_key(lane: &str, op: &FileOp) -> String {
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

pub(super) fn order_key_digits(key: &str) -> Vec<usize> {
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

pub(super) fn validate_ops(ops: &[FileOp]) -> Result<(), DecodeError> {
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

pub(super) fn normalize_ops_checked(ops: Vec<FileOp>) -> Result<Vec<FileOp>, DecodeError> {
    validate_ops(&ops)?;
    Ok(ops
        .into_iter()
        .filter(|op| op.base_len > 0 || !op.inserted.is_empty())
        .collect())
}
