use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::storage::{RepoLock, acquire_repo_lock, doctor_storage, load_repo, persist_repo};
use crate::vfs::{FileWorktree, LaneFileChange, LaneFileChangeStatus, LaneFs, LaneFsError};
use crate::{FilePath, LaneOpSummary, LaneRepo};

mod human_review;

const STORAGE_PATH: &str = ".lane";

type CliResult<T> = Result<T, CliError>;

const BYTE_PREVIEW_LIMIT: usize = 4096;

#[derive(Parser, Debug)]
#[command(name = "lane")]
#[command(about = "Run agents in isolated lanes without copying the repo")]
struct Cli {
    #[arg(long, global = true, value_name = "PATH", default_value = ".")]
    repo_root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(about = "Create an isolated lane")]
    Create { lane: String },
    #[command(about = "Run a command in a lane through a virtual mounted lane view")]
    Exec {
        lane: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = "Review lane work across every lane or one lane")]
    Review {
        #[arg(long)]
        human: bool,
        lane: Option<String>,
    },
    #[command(about = "Show one lane operation with base and inserted byte previews")]
    ShowOp {
        lane: String,
        path: String,
        op_id: String,
    },
    #[command(about = "Resolve and promote one lane operation from replacement bytes")]
    ResolveOp {
        lane: String,
        path: String,
        op_id: String,
        #[arg(long = "with-file", value_name = "PATH")]
        with_file: PathBuf,
    },
    #[command(about = "Show a text diff for a lane")]
    Diff { lane: String, paths: Vec<String> },
    #[command(about = "Promote selected lane operations into the normal repo")]
    PromoteOps {
        lane: String,
        path: String,
        #[arg(required = true)]
        ops: Vec<String>,
    },
    #[command(about = "Promote every non-conflicting operation in a lane")]
    PromoteClean { lane: String },
    #[command(about = "Remove a lane and its private changes")]
    Discard { lane: String },
    #[command(about = "Validate lane storage and report repairable state")]
    Doctor,
}

pub fn run() -> CliResult<ExitCode> {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> CliResult<ExitCode> {
    let repo_root = repo_root(cli.repo_root)?;
    match cli.command {
        Command::Create { lane } => create(&repo_root, &lane).map(|()| ExitCode::SUCCESS),
        Command::Exec { lane, command } => exec(&repo_root, &lane, &command),
        Command::Review { human, lane } => {
            review(&repo_root, lane.as_deref(), human).map(|()| ExitCode::SUCCESS)
        }
        Command::ShowOp { lane, path, op_id } => {
            show_op(&repo_root, &lane, &path, &op_id).map(|()| ExitCode::SUCCESS)
        }
        Command::ResolveOp {
            lane,
            path,
            op_id,
            with_file,
        } => resolve_op(&repo_root, &lane, &path, &op_id, &with_file).map(|()| ExitCode::SUCCESS),
        Command::Diff { lane, paths } => diff(&repo_root, &lane, paths).map(|()| ExitCode::SUCCESS),
        Command::PromoteOps { lane, path, ops } => {
            promote_ops(&repo_root, &lane, &path, &ops).map(|()| ExitCode::SUCCESS)
        }
        Command::PromoteClean { lane } => {
            promote_clean(&repo_root, &lane).map(|()| ExitCode::SUCCESS)
        }
        Command::Discard { lane } => discard(&repo_root, &lane).map(|()| ExitCode::SUCCESS),
        Command::Doctor => doctor(&repo_root),
    }
}

fn create(repo_root: &Path, lane: &str) -> CliResult<()> {
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
fn exec(repo_root: &Path, lane: &str, command: &[String]) -> CliResult<ExitCode> {
    let run = crate::virtual_exec::run_virtual_lane(repo_root, lane, command)
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
fn exec(_repo_root: &Path, _lane: &str, _command: &[String]) -> CliResult<ExitCode> {
    Err(CliError::message(
        "lane exec requires the WinFsp virtual filesystem on Windows".to_owned(),
    ))
}

fn review(repo_root: &Path, lane: Option<&str>, human: bool) -> CliResult<()> {
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
        print!("{}", human_review::format(&output));
    } else {
        print_json(&output)?;
    }
    Ok(())
}

fn doctor(repo_root: &Path) -> CliResult<ExitCode> {
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

fn show_op(repo_root: &Path, lane: &str, path: &str, op_id: &str) -> CliResult<()> {
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

fn resolve_op(
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
    locked
        .fs
        .resolve_op_file(lane, path, op_id, replacement.clone())?;
    locked.persist()?;

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

fn diff(repo_root: &Path, lane: &str, paths: Vec<String>) -> CliResult<()> {
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

fn promote_ops(repo_root: &Path, lane: &str, path: &str, ops: &[String]) -> CliResult<()> {
    let mut locked = open_locked_lane_fs(repo_root)?;
    let before = change_for_path(&locked.fs, lane, path)?
        .into_iter()
        .collect::<Vec<_>>();
    locked
        .fs
        .promote_ops_files(lane, &[(path.to_owned(), ops.to_vec())])?;
    locked.persist()?;

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

fn promote_clean(repo_root: &Path, lane: &str) -> CliResult<()> {
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
        locked.fs.promote_ops_files(lane, &selections)?;
        locked.persist()?;
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

fn discard(repo_root: &Path, lane: &str) -> CliResult<()> {
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

fn collect_changes(fs: &LaneFs, lane: &str) -> CliResult<Vec<ChangeOutput>> {
    fs.changed_paths(lane)?
        .into_iter()
        .map(|path| change_for_path(fs, lane, path))
        .collect::<CliResult<Vec<_>>>()
        .map(|changes| changes.into_iter().flatten().collect())
}

fn review_lanes(fs: &LaneFs, lane: Option<&str>) -> CliResult<Vec<String>> {
    if let Some(lane) = lane {
        fs.changed_paths(lane)?;
        Ok(vec![lane.to_owned()])
    } else {
        Ok(fs.repo().lane_ids().map(str::to_owned).collect())
    }
}

fn collect_review(
    fs: &LaneFs,
    lanes: &[String],
) -> CliResult<(ReviewSummary, Vec<ReviewLaneSummary>, Vec<ReviewPathOutput>)> {
    let mut by_path = BTreeMap::<FilePath, ReviewPathDraft>::new();
    let mut by_lane = lanes
        .iter()
        .map(|lane| {
            fs.repo()
                .last_exec(lane)
                .map(|last_exec| {
                    (
                        lane.clone(),
                        ReviewLaneSummaryDraft {
                            lane: lane.clone(),
                            last_exec: last_exec.cloned(),
                            ..ReviewLaneSummaryDraft::default()
                        },
                    )
                })
                .map_err(CliError::from)
        })
        .collect::<CliResult<BTreeMap<_, _>>>()?;
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
            let conflicts = conflict_groups_for_path(draft.conflicted_ops);
            conflict_groups += conflicts.len();
            ReviewPathOutput {
                path,
                lanes: draft.lanes.into_values().collect(),
                clean_ops: draft.clean_ops,
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
        group_ops.sort_by(|left, right| {
            left.op
                .base_start
                .cmp(&right.op.base_start)
                .then(left.op.base_end.cmp(&right.op.base_end))
                .then(left.op.lane.cmp(&right.op.lane))
                .then(left.op.op_id.cmp(&right.op.op_id))
        });
        groups.push(review_conflict_output(group_ops));
    }

    groups
}

fn review_conflict_output(ops: Vec<ReviewOpOutput>) -> ReviewConflictOutput {
    let range_start = ops.iter().map(|op| op.op.base_start).min().unwrap_or(0);
    let range_end = ops.iter().map(|op| op.op.base_end).max().unwrap_or(0);
    let lanes = ops
        .iter()
        .map(|op| op.op.lane.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let actions = ops
        .iter()
        .flat_map(|op| [show_op_action(op), resolve_op_action(op)])
        .collect();

    ReviewConflictOutput {
        range_start,
        range_end,
        lanes,
        actions,
        ops,
    }
}

fn promote_clean_action(lane: &str) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::PromoteClean,
        command: vec!["promote-clean".to_owned(), lane.to_owned()],
        lane: Some(lane.to_owned()),
        path: None,
        op_id: None,
        required_inputs: Vec::new(),
    }
}

fn show_op_action(op: &ReviewOpOutput) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::ShowOp,
        command: vec![
            "show-op".to_owned(),
            op.op.lane.clone(),
            op.op.path.clone(),
            op.op.op_id.clone(),
        ],
        lane: Some(op.op.lane.clone()),
        path: Some(op.op.path.clone()),
        op_id: Some(op.op.op_id.clone()),
        required_inputs: Vec::new(),
    }
}

fn resolve_op_action(op: &ReviewOpOutput) -> ReviewActionOutput {
    ReviewActionOutput {
        kind: ReviewActionKind::ResolveOp,
        command: vec![
            "resolve-op".to_owned(),
            op.op.lane.clone(),
            op.op.path.clone(),
            op.op.op_id.clone(),
            "--with-file".to_owned(),
            "<replacement-file>".to_owned(),
        ],
        lane: Some(op.op.lane.clone()),
        path: Some(op.op.path.clone()),
        op_id: Some(op.op.op_id.clone()),
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
        op_id: None,
        required_inputs: Vec::new(),
    }
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
            .is_some_and(|(lane, suffix)| lane == op.lane && suffix == "delete")
}

fn filter_change_ops(
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

fn grouped_ops(changes: &[ChangeOutput]) -> Vec<PathOpsOutput> {
    changes
        .iter()
        .map(|change| PathOpsOutput {
            path: change.path.clone(),
            ops: change.ops.iter().map(|op| op.op_id.clone()).collect(),
        })
        .collect()
}

fn change_for_path(
    fs: &LaneFs,
    lane: &str,
    path: impl Into<String>,
) -> CliResult<Option<ChangeOutput>> {
    fs.change_for_path(lane, path)
        .map(|change| change.map(ChangeOutput::from))
        .map_err(CliError::from)
}

fn print_diff(lane: &str, change: &ChangeOutput) {
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

struct LockedLaneFs {
    storage_path: PathBuf,
    fs: LaneFs,
    _lock: RepoLock,
}

impl LockedLaneFs {
    fn persist(&self) -> CliResult<()> {
        persist_repo(&self.storage_path, self.fs.repo())?;
        Ok(())
    }
}

fn open_locked_lane_fs(repo_root: &Path) -> CliResult<LockedLaneFs> {
    let storage_path = storage_path(repo_root);
    let lock = acquire_repo_lock(&storage_path)?;
    let repo = load_lane_repo(&storage_path)?;
    Ok(LockedLaneFs {
        storage_path,
        fs: LaneFs::new(repo, FileWorktree::new(repo_root)),
        _lock: lock,
    })
}

fn load_lane_repo(storage_path: &Path) -> CliResult<LaneRepo> {
    Ok(load_repo(storage_path)?.unwrap_or_default())
}

fn repo_root(repo_root: PathBuf) -> CliResult<PathBuf> {
    let path = if repo_root.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        repo_root
    };
    let root = fs::canonicalize(&path).map_err(|error| {
        CliError::message(format!(
            "repo root {} is not readable: {error}",
            path.display()
        ))
    })?;
    if !root.is_dir() {
        return Err(CliError::message(format!(
            "repo root {} is not a directory",
            root.display()
        )));
    }
    Ok(root)
}

fn storage_path(repo_root: &Path) -> PathBuf {
    repo_root.join(STORAGE_PATH)
}

fn path_label(path: impl AsRef<Path>) -> String {
    display_path(path.as_ref())
}

#[cfg(windows)]
fn display_path(path: &Path) -> String {
    let label = path.display().to_string();
    if let Some(path) = label.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = label.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        label
    }
}

#[cfg(not(windows))]
fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn print_json(output: &impl Serialize) -> CliResult<()> {
    println!("{}", serde_json::to_string(output)?);
    Ok(())
}

fn byte_preview(bytes: &[u8]) -> BytePreview {
    BytePreview {
        len: bytes.len(),
        sha256: sha256_hex(bytes),
        utf8: utf8_preview(bytes),
        truncated: bytes.len() > BYTE_PREVIEW_LIMIT,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn utf8_preview(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    if bytes.len() <= BYTE_PREVIEW_LIMIT {
        return Some(text.to_owned());
    }

    let mut end = 0;
    for (index, character) in text.char_indices() {
        let next = index + character.len_utf8();
        if next > BYTE_PREVIEW_LIMIT {
            break;
        }
        end = next;
    }
    Some(text[..end].to_owned())
}

#[derive(Serialize)]
struct CreateOutput<'a> {
    lane: &'a str,
    created: bool,
    repo_root: String,
    storage_path: String,
}

#[derive(Serialize)]
struct ReviewOutput {
    lane: Option<String>,
    repo_root: String,
    storage_path: String,
    summary: ReviewSummary,
    lanes: Vec<ReviewLaneSummary>,
    paths: Vec<ReviewPathOutput>,
}

#[derive(Serialize)]
struct DoctorOutput {
    repo_root: String,
    storage_path: String,
    healthy: bool,
    report: crate::storage::StorageDoctorReport,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewSummary {
    lanes: usize,
    changed_paths: usize,
    clean_ops: usize,
    conflicted_ops: usize,
    conflict_groups: usize,
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
    last_exec: Option<crate::LaneExecState>,
}

impl ReviewLaneSummaryDraft {
    fn into_output(self) -> ReviewLaneSummary {
        let mut actions = Vec::new();
        if self.clean_ops > 0 {
            actions.push(promote_clean_action(&self.lane));
        }
        actions.push(discard_action(&self.lane));

        ReviewLaneSummary {
            lane: self.lane,
            changed_paths: self.changed_paths,
            clean_ops: self.clean_ops,
            conflicted_ops: self.conflicted_ops,
            last_exec: self.last_exec,
            actions,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct ReviewLaneSummary {
    lane: String,
    changed_paths: usize,
    clean_ops: usize,
    conflicted_ops: usize,
    last_exec: Option<crate::LaneExecState>,
    actions: Vec<ReviewActionOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewPathOutput {
    path: FilePath,
    lanes: Vec<ReviewLaneOutput>,
    clean_ops: Vec<ReviewOpOutput>,
    conflicts: Vec<ReviewConflictOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewLaneOutput {
    lane: String,
    status: LaneFileChangeStatus,
    base_size: Option<usize>,
    lane_size: Option<usize>,
    total_ops: usize,
    clean_ops: usize,
    conflicted_ops: usize,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewConflictOutput {
    range_start: u64,
    range_end: u64,
    lanes: Vec<String>,
    actions: Vec<ReviewActionOutput>,
    ops: Vec<ReviewOpOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewActionOutput {
    kind: ReviewActionKind,
    command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<FilePath>,
    #[serde(skip_serializing_if = "Option::is_none")]
    op_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    required_inputs: Vec<ReviewActionInput>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReviewActionKind {
    PromoteClean,
    ShowOp,
    ResolveOp,
    Discard,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewActionInput {
    name: &'static str,
    placeholder: &'static str,
}

#[derive(Clone, Debug, Serialize)]
struct ReviewOpOutput {
    op: LaneOpSummary,
    base: BytePreview,
    inserted: BytePreview,
}

#[derive(Serialize)]
struct ShowOpOutput<'a> {
    lane: &'a str,
    path: &'a str,
    repo_root: String,
    storage_path: String,
    op: LaneOpSummary,
    base: BytePreview,
    inserted: BytePreview,
}

#[derive(Serialize)]
struct ResolveOpOutput<'a> {
    lane: &'a str,
    path: &'a str,
    op_id: &'a str,
    repo_root: String,
    storage_path: String,
    replacement_file: String,
    resolved_op: LaneOpSummary,
    replacement: BytePreview,
    remaining: Vec<ChangeOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct BytePreview {
    len: usize,
    sha256: String,
    utf8: Option<String>,
    truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
struct ChangeOutput {
    path: FilePath,
    status: LaneFileChangeStatus,
    base_size: Option<usize>,
    lane_size: Option<usize>,
    ops: Vec<LaneOpSummary>,
    #[serde(skip_serializing)]
    base: Option<Vec<u8>>,
    #[serde(skip_serializing)]
    lane: Option<Vec<u8>>,
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
struct PromoteOpsOutput<'a> {
    lane: &'a str,
    path: &'a str,
    repo_root: String,
    storage_path: String,
    promoted_ops: Vec<String>,
    promoted: Vec<ChangeOutput>,
}

#[derive(Serialize)]
struct PromoteCleanOutput<'a> {
    lane: &'a str,
    repo_root: String,
    storage_path: String,
    promoted_ops: Vec<PathOpsOutput>,
    promoted: Vec<ChangeOutput>,
    conflicts: Vec<ChangeOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct PathOpsOutput {
    path: FilePath,
    ops: Vec<String>,
}

#[derive(Serialize)]
struct DiscardOutput<'a> {
    lane: &'a str,
    removed: bool,
    discarded_changes: usize,
    repo_root: String,
    storage_path: String,
}

#[derive(Debug)]
pub struct CliError {
    message: String,
}

impl CliError {
    fn message(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for CliError {}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::message(error)
    }
}

impl From<LaneFsError> for CliError {
    fn from(error: LaneFsError) -> Self {
        Self::message(error)
    }
}

impl From<crate::LaneError> for CliError {
    fn from(error: crate::LaneError) -> Self {
        Self::message(format!("{error:?}"))
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::message(error)
    }
}
