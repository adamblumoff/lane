use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use crate::storage::{acquire_repo_lock, doctor_storage, persist_repo};

use super::error::{CliError, CliResult};
use super::output::{
    CreateOutput, DiscardOutput, DoctorOutput, PromoteCleanOutput, PromoteOpsOutput,
    ResolveOpOutput, ReviewOutput, ShowOpOutput,
};
use super::preview::byte_preview;
use super::repo::{
    load_lane_repo, open_locked_lane_fs, path_label, persist_lane_repo, print_json, storage_path,
};
use super::review::{
    change_for_path, collect_changes, collect_review, filter_change_ops, grouped_ops, print_diff,
    review_lanes,
};

pub(super) fn create(repo_root: &Path, lane: &str) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _lock = acquire_repo_lock(&storage_path)?;
    let mut repo = load_lane_repo(&storage_path)?;
    let created = repo.create_lane(lane)?;
    persist_repo(&storage_path, &repo)?;

    let output = CreateOutput {
        lane,
        created,
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
    };
    print_json(&output)?;
    Ok(())
}

#[cfg(windows)]
pub(super) fn exec(
    repo_root: &Path,
    lane: &str,
    observe: bool,
    command: &[String],
) -> CliResult<ExitCode> {
    let run = crate::virtual_exec::run_virtual_lane(
        repo_root,
        lane,
        command,
        crate::virtual_exec::VirtualExecOptions {
            observe,
            ..Default::default()
        },
    )
    .map_err(CliError::message)?;
    let failed = run.failed;
    print_json(&run.output)?;
    if failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

#[cfg(not(windows))]
pub(super) fn exec(
    _repo_root: &Path,
    _lane: &str,
    _observe: bool,
    _command: &[String],
) -> CliResult<ExitCode> {
    Err(CliError::message(
        "lane exec requires the WinFsp virtual filesystem on Windows".to_owned(),
    ))
}

pub(super) fn review(repo_root: &Path, lane: Option<&str>, human: bool) -> CliResult<()> {
    let locked = open_locked_lane_fs(repo_root)?;
    let lanes = review_lanes(&locked.fs, lane)?;
    let (summary, lane_summaries, paths) = collect_review(&locked.fs, &lanes)?;
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

pub(super) fn show_op(repo_root: &Path, lane: &str, path: &str, op_id: &str) -> CliResult<()> {
    let locked = open_locked_lane_fs(repo_root)?;
    let detail = locked.fs.op_detail(lane, path, op_id)?;
    let output = ShowOpOutput {
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

pub(super) fn resolve_op(
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
    locked.fs.resolve_op_file(
        lane,
        path,
        op_id,
        replacement.clone(),
        persist_lane_repo(&locked.storage_path),
    )?;

    let output = ResolveOpOutput {
        lane,
        path,
        op_id,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        replacement_file: path_label(replacement_file),
        resolved_op: detail.summary,
        replacement: byte_preview(&replacement),
        remaining: collect_changes(&locked.fs, lane)?,
    };
    print_json(&output)?;
    Ok(())
}

pub(super) fn diff(repo_root: &Path, lane: &str, paths: Vec<String>) -> CliResult<()> {
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

pub(super) fn promote_ops(
    repo_root: &Path,
    lane: &str,
    path: &str,
    ops: &[String],
) -> CliResult<()> {
    let mut locked = open_locked_lane_fs(repo_root)?;
    let before = change_for_path(&locked.fs, lane, path)?
        .into_iter()
        .collect::<Vec<_>>();
    locked.fs.promote_ops_files(
        lane,
        &[(path.to_owned(), ops.to_vec())],
        persist_lane_repo(&locked.storage_path),
    )?;

    let selected_ops = ops.iter().cloned().collect::<BTreeSet<_>>();
    let promoted = filter_change_ops(&before, |op| selected_ops.contains(&op.op_id));
    let output = PromoteOpsOutput {
        lane,
        path,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        promoted_ops: ops.to_vec(),
        promoted,
    };
    print_json(&output)?;
    Ok(())
}

pub(super) fn promote_clean(repo_root: &Path, lane: &str) -> CliResult<()> {
    let mut locked = open_locked_lane_fs(repo_root)?;
    let before = collect_changes(&locked.fs, lane)?;
    let promoted = filter_change_ops(&before, |op| op.conflicts_with.is_empty());
    let conflicts = filter_change_ops(&before, |op| !op.conflicts_with.is_empty());
    let promoted_ops = grouped_ops(&promoted);

    if !promoted_ops.is_empty() {
        let selections = promoted_ops
            .iter()
            .map(|path_ops| (path_ops.path.clone(), path_ops.ops.clone()))
            .collect::<Vec<_>>();
        locked
            .fs
            .promote_ops_files(lane, &selections, persist_lane_repo(&locked.storage_path))?;
    }

    let output = PromoteCleanOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        promoted_ops,
        promoted,
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
