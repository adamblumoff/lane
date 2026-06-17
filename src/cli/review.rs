use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use similar::TextDiff;

use crate::vfs::LaneFs;
use crate::{FilePath, LaneId, LaneOpSummary, LaneRunState};

use super::error::{CliError, CliResult};
use super::output::{
    ChangeOutput, PathOpsOutput, ReviewActionInput, ReviewActionKind, ReviewActionOutput,
    ReviewConflictOutput, ReviewLaneOutput, ReviewLaneSummary, ReviewOpOutput, ReviewOpState,
    ReviewOrderedOpOutput, ReviewPathOutput, ReviewSummary,
};
use super::preview::byte_preview;

pub(super) fn collect_changes(fs: &LaneFs, lane: &str) -> CliResult<Vec<ChangeOutput>> {
    fs.changed_paths(lane)?
        .into_iter()
        .map(|path| change_for_path(fs, lane, path))
        .collect::<CliResult<Vec<_>>>()
        .map(|changes| changes.into_iter().flatten().collect())
}

pub(super) fn review_lanes(fs: &LaneFs, lane: Option<&str>) -> CliResult<Vec<String>> {
    if let Some(lane) = lane {
        fs.changed_paths(lane)?;
        Ok(vec![lane.to_owned()])
    } else {
        Ok(fs.repo().lane_ids().map(str::to_owned).collect())
    }
}

pub(super) fn collect_review(
    fs: &LaneFs,
    last_run: &BTreeMap<LaneId, LaneRunState>,
    lanes: &[String],
) -> CliResult<(ReviewSummary, Vec<ReviewLaneSummary>, Vec<ReviewPathOutput>)> {
    let mut by_path = BTreeMap::<FilePath, ReviewPathDraft>::new();
    let mut by_lane = lanes
        .iter()
        .map(|lane| {
            (
                lane.clone(),
                ReviewLaneSummaryDraft {
                    lane: lane.clone(),
                    last_run: last_run.get(lane.as_str()).cloned(),
                    ..ReviewLaneSummaryDraft::default()
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut clean_ops = 0usize;
    let mut conflicted_ops = 0usize;

    for lane in lanes {
        for change in collect_changes(fs, lane)? {
            let total_ops = change.ops.len();
            let clean_count = change
                .ops
                .iter()
                .filter(|op| op.conflicts_with.is_empty())
                .count();
            let conflicted_count = total_ops - clean_count;
            let lane_summary = by_lane.get_mut(lane).expect("review lane is initialized");
            lane_summary.changed_paths += 1;
            lane_summary.clean_ops += clean_count;
            lane_summary.conflicted_ops += conflicted_count;

            let draft = by_path.entry(change.path.clone()).or_default();
            draft.lanes.insert(
                lane.clone(),
                ReviewLaneOutput {
                    lane: lane.clone(),
                    status: change.status,
                    base_size: change.base_size,
                    lane_size: change.lane_size,
                    total_ops,
                    clean_ops: clean_count,
                    conflicted_ops: conflicted_count,
                },
            );

            for op in &change.ops {
                let reviewed_op = review_op(fs, op)?;
                if op.conflicts_with.is_empty() {
                    clean_ops += 1;
                    draft.clean_ops.push(reviewed_op);
                } else {
                    conflicted_ops += 1;
                    draft.conflicted_ops.push(reviewed_op);
                }
            }
        }
    }

    let mut conflict_groups = 0usize;
    let paths = by_path
        .into_iter()
        .map(|(path, draft)| {
            let mut clean_ops = draft.clean_ops;
            clean_ops.sort_by(order_review_ops);
            let conflicts = conflict_groups_for_path(draft.conflicted_ops);
            let ops = ordered_ops_for_path(&clean_ops, &conflicts);
            conflict_groups += conflicts.len();
            ReviewPathOutput {
                path,
                lanes: draft.lanes.into_values().collect(),
                ops,
                conflicts,
            }
        })
        .collect::<Vec<_>>();

    Ok((
        ReviewSummary {
            lanes: lanes.len(),
            changed_paths: paths.len(),
            clean_ops,
            conflicted_ops,
            conflict_groups,
        },
        by_lane
            .into_values()
            .map(ReviewLaneSummaryDraft::into_output)
            .collect(),
        paths,
    ))
}

fn review_op(fs: &LaneFs, summary: &LaneOpSummary) -> CliResult<ReviewOpOutput> {
    let detail = fs.op_detail(&summary.lane, &summary.path, &summary.op_id)?;
    Ok(ReviewOpOutput {
        op: detail.summary,
        base: byte_preview(&detail.base),
        inserted: byte_preview(&detail.inserted),
    })
}

fn conflict_groups_for_path(ops: Vec<ReviewOpOutput>) -> Vec<ReviewConflictOutput> {
    let mut groups = Vec::new();
    let mut visited = vec![false; ops.len()];

    for index in 0..ops.len() {
        if visited[index] {
            continue;
        }

        let mut stack = vec![index];
        let mut group_indices = Vec::new();
        visited[index] = true;

        while let Some(current) = stack.pop() {
            group_indices.push(current);
            for candidate in 0..ops.len() {
                if !visited[candidate] && review_ops_conflict(&ops[current], &ops[candidate]) {
                    visited[candidate] = true;
                    stack.push(candidate);
                }
            }
        }

        let mut group_ops = group_indices
            .into_iter()
            .map(|index| ops[index].clone())
            .collect::<Vec<_>>();
        group_ops.sort_by(order_review_ops);
        groups.push(review_conflict_output(group_ops));
    }

    groups.sort_by(|left, right| {
        left.range_start
            .cmp(&right.range_start)
            .then(left.range_end.cmp(&right.range_end))
            .then_with(|| left.ops[0].op.order_key.cmp(&right.ops[0].op.order_key))
    });
    groups
}

fn review_conflict_output(ops: Vec<ReviewOpOutput>) -> ReviewConflictOutput {
    let range_start = ops.iter().map(|op| op.op.base_start).min().unwrap_or(0);
    let range_end = ops.iter().map(|op| op.op.base_end).max().unwrap_or(0);
    let lanes = ops
        .iter()
        .map(|op| op.op.lane.as_str().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut actions = Vec::new();
    if ops.len() > 1 {
        actions.push(accept_replacement_ops_action(&ops));
    }
    actions.extend(
        ops.iter()
            .flat_map(|op| [detail_action(op), accept_replacement_op_action(op)]),
    );

    ReviewConflictOutput {
        range_start,
        range_end,
        lanes,
        actions,
        ops,
    }
}

pub(super) fn accept_clean_action(lane: &str) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::Accept,
        command: vec!["accept".to_owned(), lane.to_owned()],
        lane: Some(lane.to_owned()),
        path: None,
        op_ids: Vec::new(),
        required_inputs: Vec::new(),
    }
}

pub(super) fn detail_action(op: &ReviewOpOutput) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::Detail,
        command: vec![
            "review".to_owned(),
            op.op.lane.to_string(),
            op.op.path.to_string(),
            op.op.op_id.clone(),
        ],
        lane: Some(op.op.lane.to_string()),
        path: Some(op.op.path.clone()),
        op_ids: vec![op.op.op_id.clone()],
        required_inputs: Vec::new(),
    }
}

pub(super) fn accept_replacement_op_action(op: &ReviewOpOutput) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::Accept,
        command: vec![
            "accept".to_owned(),
            op.op.lane.to_string(),
            op.op.path.to_string(),
            op.op.op_id.clone(),
            "--with-file".to_owned(),
            "<replacement-file>".to_owned(),
        ],
        lane: Some(op.op.lane.to_string()),
        path: Some(op.op.path.clone()),
        op_ids: vec![op.op.op_id.clone()],
        required_inputs: vec![ReviewActionInput {
            name: "with_file",
            placeholder: "<replacement-file>",
        }],
    }
}

fn accept_replacement_ops_action(ops: &[ReviewOpOutput]) -> ReviewActionOutput {
    let path = ops
        .first()
        .map(|op| op.op.path.clone())
        .expect("conflict action requires at least one operation");
    let op_ids = ops.iter().map(|op| op.op.op_id.clone()).collect::<Vec<_>>();
    let mut command = vec!["accept".to_owned(), path.to_string()];
    for op_id in &op_ids {
        command.push("--op".to_owned());
        command.push(op_id.clone());
    }
    command.push("--with-file".to_owned());
    command.push("<replacement-file>".to_owned());

    ReviewActionOutput {
        kind: ReviewActionKind::Accept,
        command,
        lane: None,
        path: Some(path),
        op_ids,
        required_inputs: vec![ReviewActionInput {
            name: "with_file",
            placeholder: "<replacement-file>",
        }],
    }
}

fn discard_action(lane: &str) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::Discard,
        command: vec!["discard".to_owned(), lane.to_owned()],
        lane: Some(lane.to_owned()),
        path: None,
        op_ids: Vec::new(),
        required_inputs: Vec::new(),
    }
}

fn ordered_ops_for_path(
    clean_ops: &[ReviewOpOutput],
    conflicts: &[ReviewConflictOutput],
) -> Vec<ReviewOrderedOpOutput> {
    let conflict_groups = conflicts
        .iter()
        .enumerate()
        .flat_map(|(index, conflict)| {
            conflict
                .ops
                .iter()
                .map(move |op| (op.op.op_id.clone(), index + 1))
        })
        .collect::<BTreeMap<_, _>>();
    let mut ops = clean_ops
        .iter()
        .chain(conflicts.iter().flat_map(|conflict| conflict.ops.iter()))
        .map(|op| {
            let conflict_group = conflict_groups.get(&op.op.op_id).copied();
            ReviewOrderedOpOutput {
                state: if conflict_group.is_some() {
                    ReviewOpState::Conflicted
                } else {
                    ReviewOpState::Clean
                },
                conflict_group,
                op: op.op.clone(),
                base: op.base.clone(),
                inserted: op.inserted.clone(),
            }
        })
        .collect::<Vec<_>>();
    ops.sort_by(|left, right| order_lane_op_summaries(&left.op, &right.op));
    ops
}

fn order_review_ops(left: &ReviewOpOutput, right: &ReviewOpOutput) -> Ordering {
    order_lane_op_summaries(&left.op, &right.op)
}

fn order_lane_op_summaries(left: &LaneOpSummary, right: &LaneOpSummary) -> Ordering {
    left.order_key
        .cmp(&right.order_key)
        .then(left.base_start.cmp(&right.base_start))
        .then(left.base_end.cmp(&right.base_end))
        .then(left.lane.cmp(&right.lane))
        .then(left.op_id.cmp(&right.op_id))
}

fn review_ops_conflict(left: &ReviewOpOutput, right: &ReviewOpOutput) -> bool {
    if left.op.path != right.op.path {
        return false;
    }
    if is_whole_file_delete(&left.op) || is_whole_file_delete(&right.op) {
        return left.op.conflicts_with.contains(&right.op.lane)
            || right.op.conflicts_with.contains(&left.op.lane);
    }
    if matches!(left.op.kind, crate::LaneOpKind::Create)
        || matches!(right.op.kind, crate::LaneOpKind::Create)
    {
        return true;
    }

    let left_len = left.op.base_end - left.op.base_start;
    let right_len = right.op.base_end - right.op.base_start;
    if left_len == 0 && right_len == 0 {
        return false;
    }
    if left_len == 0 {
        return right.op.base_start < left.op.base_start && left.op.base_start < right.op.base_end;
    }
    if right_len == 0 {
        return left.op.base_start < right.op.base_start && right.op.base_start < left.op.base_end;
    }
    left.op.base_start < right.op.base_end && right.op.base_start < left.op.base_end
}

fn is_whole_file_delete(op: &LaneOpSummary) -> bool {
    matches!(op.kind, crate::LaneOpKind::Delete)
        && op
            .op_id
            .rsplit_once(':')
            .is_some_and(|(lane, suffix)| op.lane == lane && suffix == "delete")
}

pub(super) fn filter_change_ops(
    changes: &[ChangeOutput],
    keep: impl Fn(&LaneOpSummary) -> bool,
) -> Vec<ChangeOutput> {
    changes
        .iter()
        .filter_map(|change| {
            let ops = change
                .ops
                .iter()
                .filter(|op| keep(op))
                .cloned()
                .collect::<Vec<_>>();
            if ops.is_empty() {
                None
            } else {
                let mut filtered = change.clone();
                filtered.ops = ops;
                Some(filtered)
            }
        })
        .collect()
}

pub(super) fn grouped_ops(changes: &[ChangeOutput]) -> Vec<PathOpsOutput> {
    changes
        .iter()
        .map(|change| PathOpsOutput {
            path: change.path.clone(),
            ops: change.ops.iter().map(|op| op.op_id.clone()).collect(),
        })
        .collect()
}

pub(super) fn change_for_path(
    fs: &LaneFs,
    lane: &str,
    path: impl Into<String>,
) -> CliResult<Option<ChangeOutput>> {
    fs.change_for_path(lane, path)
        .map(|change| change.map(ChangeOutput::from))
        .map_err(CliError::from)
}

pub(super) fn print_diff(lane: &str, change: &ChangeOutput) {
    let base = change.base.as_deref().unwrap_or_default();
    let lane_bytes = change.lane.as_deref().unwrap_or_default();
    let Ok(base_text) = std::str::from_utf8(base) else {
        println!("binary files differ: {}", change.path);
        return;
    };
    let Ok(lane_text) = std::str::from_utf8(lane_bytes) else {
        println!("binary files differ: {}", change.path);
        return;
    };
    let diff = TextDiff::from_lines(base_text, lane_text);
    let output = diff
        .unified_diff()
        .header(
            &format!("base/{}", change.path),
            &format!("{lane}/{}", change.path),
        )
        .to_string();
    print!("{output}");
    if !output.ends_with('\n') {
        println!();
    }
}

#[derive(Clone, Debug, Default)]
struct ReviewPathDraft {
    lanes: BTreeMap<String, ReviewLaneOutput>,
    clean_ops: Vec<ReviewOpOutput>,
    conflicted_ops: Vec<ReviewOpOutput>,
}

#[derive(Clone, Debug, Default)]
struct ReviewLaneSummaryDraft {
    lane: String,
    changed_paths: usize,
    clean_ops: usize,
    conflicted_ops: usize,
    last_run: Option<LaneRunState>,
}

impl ReviewLaneSummaryDraft {
    fn into_output(self) -> ReviewLaneSummary {
        let mut actions = Vec::new();
        if self.clean_ops > 0 {
            actions.push(accept_clean_action(&self.lane));
        }
        actions.push(discard_action(&self.lane));

        ReviewLaneSummary {
            lane: self.lane,
            changed_paths: self.changed_paths,
            clean_ops: self.clean_ops,
            conflicted_ops: self.conflicted_ops,
            last_run: self.last_run,
            actions,
        }
    }
}
