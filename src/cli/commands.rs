use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use crate::LaneError;
use crate::storage::{cleanup_storage as cleanup_lane_storage, doctor_storage};
use crate::vfs::LaneFsError;

use super::error::{CliError, CliResult};
use super::output::{
    AcceptCleanOutput, AcceptOpsOutput, AcceptReplacementOpOutput, AcceptReplacementOpsOutput,
    DiscardOutput, DoctorOutput, OpDetailOutput, ReviewOutput, StorageCleanupOutput,
};
use super::preview::byte_preview;
use super::repo::{open_locked_lane_fs, path_label, persist_lane_repo, print_json, storage_path};
use super::review::{
    change_for_path, collect_changes, collect_review, filter_change_ops, grouped_ops, print_diff,
    review_lanes,
};

#[cfg(windows)]
pub(super) fn run_one(
    repo_root: &Path,
    lane: &str,
    observe: bool,
    command: &[String],
) -> CliResult<ExitCode> {
    let output = crate::virtual_run::run_virtual_lane(
        repo_root,
        lane,
        command,
        crate::virtual_run::VirtualRunOptions {
            observe,
            ..Default::default()
        },
    )
    .map_err(CliError::message)?;
    let failed = output.failed();
    print_json(&output)?;
    if failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

#[cfg(not(windows))]
pub(super) fn run_one(
    _repo_root: &Path,
    _lane: &str,
    _observe: bool,
    _command: &[String],
) -> CliResult<ExitCode> {
    Err(CliError::message(
        "lane run requires the WinFsp virtual filesystem on Windows".to_owned(),
    ))
}

pub(super) fn review(repo_root: &Path, lane: Option<&str>, human: bool) -> CliResult<()> {
    let locked = open_locked_lane_fs(repo_root)?;
    let lanes = review_lanes(&locked.fs, lane)?;
    let (summary, lane_summaries, paths) = collect_review(&locked.fs, &locked.last_run, &lanes)?;
    let output = ReviewOutput {
        lane: lane.map(str::to_owned),
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        summary,
        lanes: lane_summaries,
        paths,
    };
    if human {
        print!("{}", super::human_review::format(&output));
    } else {
        print_json(&output)?;
    }
    Ok(())
}

pub(super) fn lane_exists(repo_root: &Path, lane: &str) -> CliResult<bool> {
    let locked = open_locked_lane_fs(repo_root)?;
    Ok(locked
        .fs
        .repo()
        .lane_ids()
        .any(|candidate| candidate == lane))
}

pub(super) fn doctor(repo_root: &Path) -> CliResult<ExitCode> {
    let storage_path = storage_path(repo_root);
    let report = doctor_storage(&storage_path)?;
    let healthy = report.is_healthy();
    let output = DoctorOutput {
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path),
        healthy,
        report,
    };
    print_json(&output)?;
    if healthy {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

pub(super) fn cleanup_storage(repo_root: &Path) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let report = cleanup_lane_storage(&storage_path)?;
    let output = StorageCleanupOutput {
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path),
        blobs_removed: report.blobs_removed,
        bytes_removed: report.bytes_removed,
        blobs_remaining: report.blobs_remaining,
    };
    print_json(&output)?;
    Ok(())
}

pub(super) fn review_op_detail(
    repo_root: &Path,
    lane: &str,
    path: &str,
    op_id: &str,
) -> CliResult<()> {
    let locked = open_locked_lane_fs(repo_root)?;
    let detail = locked.fs.op_detail(lane, path, op_id)?;
    let output = OpDetailOutput {
        lane,
        path,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        op: detail.summary,
        base: byte_preview(&detail.base),
        inserted: byte_preview(&detail.inserted),
    };

    print_json(&output)?;
    Ok(())
}

pub(super) fn accept_replacement_op(
    repo_root: &Path,
    lane: &str,
    path: &str,
    op_id: &str,
    with_file: &Path,
) -> CliResult<()> {
    let replacement = fs::read(with_file)?;
    let replacement_file = fs::canonicalize(with_file).unwrap_or_else(|_| with_file.to_path_buf());
    let mut locked = open_locked_lane_fs(repo_root)?;
    let detail = locked.fs.op_detail(lane, path, op_id)?;
    locked.fs.accept_replacement_op_file(
        lane,
        path,
        op_id,
        replacement.clone(),
        persist_lane_repo(&locked.storage_path),
    )?;

    let output = AcceptReplacementOpOutput {
        lane,
        path,
        op_id,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        replacement_file: path_label(replacement_file),
        accepted_op: detail.summary,
        replacement: byte_preview(&replacement),
        remaining: collect_changes(&locked.fs, lane)?,
    };
    print_json(&output)?;
    Ok(())
}

pub(super) fn accept_replacement_ops(
    repo_root: &Path,
    path: &str,
    ops: &[String],
    with_file: &Path,
) -> CliResult<()> {
    let selections = parse_lane_op_selections(ops)?;
    let replacement = fs::read(with_file)?;
    let replacement_file = fs::canonicalize(with_file).unwrap_or_else(|_| with_file.to_path_buf());
    let mut locked = open_locked_lane_fs(repo_root)?;
    let details = selections
        .iter()
        .map(|(lane, op_id)| locked.fs.op_detail(lane, path, op_id))
        .collect::<Result<Vec<_>, _>>()?;
    locked
        .fs
        .accept_replacement_ops_file(
            path,
            &selections,
            replacement.clone(),
            persist_lane_repo(&locked.storage_path),
        )
        .map_err(accept_replacement_ops_cli_error)?;

    let affected_lanes = selections
        .iter()
        .map(|(lane, _)| lane.clone())
        .collect::<BTreeSet<_>>();
    let mut remaining = Vec::new();
    for lane in affected_lanes {
        remaining.extend(collect_changes(&locked.fs, &lane)?);
    }

    let output = AcceptReplacementOpsOutput {
        path: path.to_owned(),
        ops: ops.to_vec(),
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        replacement_file: path_label(replacement_file),
        accepted_ops: details.into_iter().map(|detail| detail.summary).collect(),
        replacement: byte_preview(&replacement),
        remaining,
    };
    print_json(&output)?;
    Ok(())
}

fn parse_lane_op_selections(ops: &[String]) -> CliResult<Vec<(String, String)>> {
    if ops.len() < 2 {
        return Err(CliError::message(
            "accept requires at least two --op values from one conflict group".to_owned(),
        ));
    }
    ops.iter()
        .map(|op_id| {
            let Some((lane, suffix)) = op_id.rsplit_once(':') else {
                return Err(CliError::message(format!(
                    "accept --op value must be lane-qualified: {op_id}"
                )));
            };
            if lane.is_empty() || suffix.is_empty() {
                return Err(CliError::message(format!(
                    "accept --op value must be lane-qualified: {op_id}"
                )));
            }
            Ok((lane.to_owned(), op_id.clone()))
        })
        .collect()
}

fn accept_replacement_ops_cli_error(error: LaneFsError) -> CliError {
    match error {
        LaneFsError::Lane(LaneError::InvalidOperationSelection { reason, .. }) => {
            CliError::message(format!(
                "accept can only combine ops from one conflict group: {reason}"
            ))
        }
        error => CliError::from(error),
    }
}

pub(super) fn review_diff(repo_root: &Path, lane: &str, paths: Vec<String>) -> CliResult<()> {
    let locked = open_locked_lane_fs(repo_root)?;
    let changes = if paths.is_empty() {
        collect_changes(&locked.fs, lane)?
    } else {
        paths
            .into_iter()
            .map(|path| change_for_path(&locked.fs, lane, path))
            .collect::<CliResult<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect()
    };

    if changes.is_empty() {
        println!("no changes in lane {lane}");
        return Ok(());
    }

    for change in &changes {
        print_diff(lane, change);
    }
    Ok(())
}

pub(super) fn accept_ops(
    repo_root: &Path,
    lane: &str,
    path: &str,
    ops: &[String],
) -> CliResult<()> {
    let mut locked = open_locked_lane_fs(repo_root)?;
    let before = change_for_path(&locked.fs, lane, path)?
        .into_iter()
        .collect::<Vec<_>>();
    locked.fs.accept_ops_files(
        lane,
        &[(path.to_owned(), ops.to_vec())],
        persist_lane_repo(&locked.storage_path),
    )?;

    let selected_ops = ops.iter().cloned().collect::<BTreeSet<_>>();
    let accepted = filter_change_ops(&before, |op| selected_ops.contains(&op.op_id));
    let output = AcceptOpsOutput {
        lane,
        path,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        accepted_ops: ops.to_vec(),
        accepted,
    };
    print_json(&output)?;
    Ok(())
}

pub(super) fn accept_clean(repo_root: &Path, lane: &str) -> CliResult<()> {
    let mut locked = open_locked_lane_fs(repo_root)?;
    let before = collect_changes(&locked.fs, lane)?;
    let accepted = filter_change_ops(&before, |op| op.conflicts_with.is_empty());
    let conflicts = filter_change_ops(&before, |op| !op.conflicts_with.is_empty());
    let accepted_ops = grouped_ops(&accepted);

    if !accepted_ops.is_empty() {
        let selections = accepted_ops
            .iter()
            .map(|path_ops| (path_ops.path.clone(), path_ops.ops.clone()))
            .collect::<Vec<_>>();
        locked
            .fs
            .accept_ops_files(lane, &selections, persist_lane_repo(&locked.storage_path))?;
    }

    let output = AcceptCleanOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        accepted_ops,
        accepted,
        conflicts,
    };
    print_json(&output)?;
    Ok(())
}

pub(super) fn discard(repo_root: &Path, lane: &str) -> CliResult<()> {
    let mut locked = open_locked_lane_fs(repo_root)?;
    let discarded_changes = collect_changes(&locked.fs, lane).map_or(0, |changes| changes.len());
    let removed = locked.fs.discard_lane(lane);
    locked.persist()?;

    let output = DiscardOutput {
        lane,
        removed,
        discarded_changes,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
    };
    print_json(&output)?;
    Ok(())
}
