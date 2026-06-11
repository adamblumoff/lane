use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};

use crate::storage::{acquire_repo_lock, encode_path_component, persist_bytes, persist_repo};
use crate::{FilePath, LaneId, LaneTextPreview, ensure_user_lane};

use super::error::{CliError, CliResult};
use super::human_review::format_command;
use super::output::{ReviewActionKind, ReviewOutput};
use super::repo::{load_lane_repo, open_locked_lane_fs, path_label, print_json, storage_path};
use super::review::collect_review;

const RUN_VERSION: u32 = 1;

pub(super) fn try_run(
    repo_root: &Path,
    name: &str,
    attempts: usize,
    observe: bool,
    command: &[String],
) -> CliResult<ExitCode> {
    validate_run_request(name, attempts)?;
    let lanes = attempt_lanes(name, attempts)?;
    reserve_attempt_lanes(repo_root, name, &lanes)?;

    let jobs = lanes
        .into_iter()
        .enumerate()
        .map(|(index, lane)| {
            let repo_root = repo_root.to_path_buf();
            let command = command.to_vec();
            let lane_for_thread = lane.clone();
            (
                index + 1,
                lane,
                thread::spawn(move || {
                    run_lane_command(repo_root, lane_for_thread, index + 1, command, observe)
                }),
            )
        })
        .collect::<Vec<_>>();
    let attempt_records = join_attempt_jobs("attempt", jobs);

    let run = RunRecord {
        version: RUN_VERSION,
        name: name.to_owned(),
        command: command.to_vec(),
        attempts: attempt_records,
        checks: Vec::new(),
    };
    persist_run(repo_root, &run)?;

    let output = TryOutput {
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path(repo_root)),
        run,
    };
    let failed = output
        .run
        .attempts
        .iter()
        .any(|attempt| attempt.orchestration_error.is_some());
    print_json(&output)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

pub(super) fn check(
    repo_root: &Path,
    run_name: &str,
    check_name: Option<&str>,
    command: &[String],
) -> CliResult<ExitCode> {
    let run = load_run(repo_root, run_name)?;
    let jobs = run
        .attempts
        .iter()
        .map(|attempt| {
            let repo_root = repo_root.to_path_buf();
            let command = command.to_vec();
            let lane = attempt.lane.clone();
            let index = attempt.index;
            (
                index,
                lane.clone(),
                thread::spawn(move || run_check_command(repo_root, lane, index, command)),
            )
        })
        .collect::<Vec<_>>();
    let check_attempts = join_attempt_jobs("check", jobs);

    let check = CheckRecord {
        name: check_name
            .map(str::to_owned)
            .unwrap_or_else(|| format!("check-{}", run.checks.len() + 1)),
        command: command.to_vec(),
        attempts: check_attempts,
    };
    let failed = check.attempts.iter().any(|attempt| !attempt.ok());
    let run = append_check(repo_root, run_name, check.clone())?;

    let output = CheckOutput {
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path(repo_root)),
        run: run.name.clone(),
        check,
    };
    print_json(&output)?;
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

fn join_attempt_jobs(
    kind: &str,
    jobs: Vec<(usize, LaneId, JoinHandle<AttemptRecord>)>,
) -> Vec<AttemptRecord> {
    let mut records = jobs
        .into_iter()
        .map(|(index, lane, job)| match job.join() {
            Ok(record) => record,
            Err(_) => AttemptRecord::thread_panic(kind, index, lane),
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|attempt| attempt.index);
    records
}

pub(super) fn compare(repo_root: &Path, run_name: &str, human: bool) -> CliResult<()> {
    let run = load_run(repo_root, run_name)?;
    let lanes = run
        .attempts
        .iter()
        .map(|attempt| attempt.lane.clone())
        .collect::<Vec<_>>();
    let locked = open_locked_lane_fs(repo_root)?;
    let (summary, lane_summaries, paths) = collect_review(&locked.fs, &lanes)?;
    let review = ReviewOutput {
        lane: None,
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        summary,
        lanes: lane_summaries,
        paths,
    };
    let attempts = compare_attempts(&run, &review);
    let output = CompareOutput {
        repo_root: path_label(repo_root),
        storage_path: path_label(&locked.storage_path),
        run,
        attempts,
        review,
    };

    if human {
        print!("{}", format_compare(&output));
    } else {
        print_json(&output)?;
    }
    Ok(())
}

fn validate_run_request(name: &str, attempts: usize) -> CliResult<()> {
    if name.trim().is_empty() {
        return Err(CliError::message("run name cannot be empty"));
    }
    if attempts == 0 {
        return Err(CliError::message("attempts must be greater than zero"));
    }
    Ok(())
}

fn attempt_lanes(name: &str, attempts: usize) -> CliResult<Vec<LaneId>> {
    (1..=attempts)
        .map(|index| {
            let lane = format!("{name}-{index}");
            ensure_user_lane(&lane).map_err(CliError::from)?;
            Ok(lane)
        })
        .collect()
}

fn reserve_attempt_lanes(repo_root: &Path, name: &str, lanes: &[LaneId]) -> CliResult<()> {
    let run_path = run_path(repo_root, name);
    if run_path.exists() {
        return Err(CliError::message(format!("run {name:?} already exists")));
    }

    let storage_path = storage_path(repo_root);
    let _lock = acquire_repo_lock(&storage_path)?;
    let mut repo = load_lane_repo(&storage_path)?;
    let existing = repo
        .lane_ids()
        .filter(|lane| lanes.iter().any(|attempt_lane| attempt_lane == lane))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !existing.is_empty() {
        return Err(CliError::message(format!(
            "attempt lanes already exist: {}",
            existing.join(", ")
        )));
    }

    for lane in lanes {
        repo.create_lane(lane)?;
    }
    persist_repo(&storage_path, &repo)?;
    Ok(())
}

#[cfg(windows)]
fn run_lane_command(
    repo_root: PathBuf,
    lane: LaneId,
    index: usize,
    command: Vec<String>,
    observe: bool,
) -> AttemptRecord {
    match crate::virtual_exec::run_virtual_lane(
        &repo_root,
        &lane,
        &command,
        crate::virtual_exec::VirtualExecOptions {
            observe,
            ..Default::default()
        },
    ) {
        Ok(run) => AttemptRecord {
            index,
            lane,
            exec: Some(run.into_record().into()),
            orchestration_error: None,
        },
        Err(error) => AttemptRecord {
            index,
            lane,
            exec: None,
            orchestration_error: Some(error.to_string()),
        },
    }
}

#[cfg(not(windows))]
fn run_lane_command(
    _repo_root: PathBuf,
    lane: LaneId,
    index: usize,
    _command: Vec<String>,
    _observe: bool,
) -> AttemptRecord {
    AttemptRecord {
        index,
        lane,
        exec: None,
        orchestration_error: Some(
            "lane try is only supported on Windows (requires the WinFsp virtual filesystem)"
                .to_owned(),
        ),
    }
}

#[cfg(windows)]
fn run_check_command(
    repo_root: PathBuf,
    lane: LaneId,
    index: usize,
    command: Vec<String>,
) -> AttemptRecord {
    match crate::virtual_exec::run_virtual_lane(
        &repo_root,
        &lane,
        &command,
        crate::virtual_exec::VirtualExecOptions {
            observe: false,
            persist_changes: false,
        },
    ) {
        Ok(run) => AttemptRecord {
            index,
            lane,
            exec: Some(run.into_record().into()),
            orchestration_error: None,
        },
        Err(error) => AttemptRecord {
            index,
            lane,
            exec: None,
            orchestration_error: Some(error.to_string()),
        },
    }
}

#[cfg(not(windows))]
fn run_check_command(
    _repo_root: PathBuf,
    lane: LaneId,
    index: usize,
    _command: Vec<String>,
) -> AttemptRecord {
    AttemptRecord {
        index,
        lane,
        exec: None,
        orchestration_error: Some(
            "lane check is only supported on Windows (requires the WinFsp virtual filesystem)"
                .to_owned(),
        ),
    }
}

fn run_path(repo_root: &Path, name: &str) -> PathBuf {
    storage_path(repo_root)
        .join("runs")
        .join(format!("{}.json", encode_path_component(name)))
}

fn load_run(repo_root: &Path, name: &str) -> CliResult<RunRecord> {
    let path = run_path(repo_root, name);
    let bytes = fs::read(&path).map_err(|error| {
        CliError::message(format!(
            "run {name:?} is not readable at {}: {error}",
            path.display()
        ))
    })?;
    let run = serde_json::from_slice::<RunRecord>(&bytes)?;
    if run.version != RUN_VERSION {
        return Err(CliError::message(format!(
            "run {name:?} has version {}; expected {RUN_VERSION}",
            run.version
        )));
    }
    Ok(run)
}

fn persist_run(repo_root: &Path, run: &RunRecord) -> CliResult<()> {
    let path = run_path(repo_root, &run.name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(run)?;
    persist_bytes(&path, &bytes)?;
    Ok(())
}

fn append_check(repo_root: &Path, run_name: &str, check: CheckRecord) -> CliResult<RunRecord> {
    let storage_path = storage_path(repo_root);
    let _lock = acquire_repo_lock(&storage_path)?;
    let mut run = load_run(repo_root, run_name)?;
    run.checks.push(check);
    persist_run(repo_root, &run)?;
    Ok(run)
}

fn compare_attempts(run: &RunRecord, review: &ReviewOutput) -> Vec<CompareAttempt> {
    let review_by_lane = review
        .lanes
        .iter()
        .map(|lane| (lane.lane.clone(), lane))
        .collect::<BTreeMap<_, _>>();
    run.attempts
        .iter()
        .map(|attempt| {
            let lane_review = review_by_lane.get(&attempt.lane);
            let checks = run
                .checks
                .iter()
                .filter_map(|check| {
                    check
                        .attempts
                        .iter()
                        .find(|check_attempt| check_attempt.lane == attempt.lane)
                        .map(|check_attempt| CompareCheck {
                            name: check.name.clone(),
                            ok: check_attempt.ok(),
                            exit_code: check_attempt.exec.as_ref().and_then(|exec| exec.exit_code),
                            worker_error: check_attempt
                                .exec
                                .as_ref()
                                .and_then(|exec| exec.worker_error.clone()),
                            orchestration_error: check_attempt.orchestration_error.clone(),
                        })
                })
                .collect::<Vec<_>>();
            let checks_passed = checks.iter().filter(|check| check.ok).count();
            let checks_failed = checks.len() - checks_passed;
            let clean_ops = lane_review.map_or(0, |lane| lane.clean_ops);
            let conflicted_ops = lane_review.map_or(0, |lane| lane.conflicted_ops);
            let changed_paths = lane_review.map_or(0, |lane| lane.changed_paths);
            CompareAttempt {
                index: attempt.index,
                lane: attempt.lane.clone(),
                attempt_ok: attempt.ok(),
                attempt_exit_code: attempt.exec.as_ref().and_then(|exec| exec.exit_code),
                attempt_error: attempt.orchestration_error.clone().or_else(|| {
                    attempt
                        .exec
                        .as_ref()
                        .and_then(|exec| exec.worker_error.clone())
                }),
                checks_passed,
                checks_failed,
                checks,
                changed_paths,
                clean_ops,
                conflicted_ops,
                actions: compare_actions(&attempt.lane, clean_ops),
            }
        })
        .collect()
}

fn compare_actions(lane: &str, clean_ops: usize) -> Vec<CompareAction> {
    let mut actions = vec![
        CompareAction::new("review_human", ["review", "--human", lane]),
        CompareAction::new("diff", ["diff", lane]),
    ];
    if clean_ops > 0 {
        actions.push(CompareAction::new("promote_clean", ["promote-clean", lane]));
    }
    actions.push(CompareAction::new("discard", ["discard", lane]));
    actions
}

fn format_compare(output: &CompareOutput) -> String {
    let mut text = String::new();
    text.push_str("Lane compare\n");
    text.push_str(&format!("run: {}\n", output.run.name));
    text.push_str(&format!("repo: {}\n", output.repo_root));
    text.push_str(&format!("storage: {}\n", output.storage_path));
    text.push_str(&format!(
        "summary: {}, {}, {}, {}, {}\n",
        count_label(output.run.attempts.len(), "attempt"),
        count_label(output.run.checks.len(), "check"),
        count_label(output.review.summary.changed_paths, "changed path"),
        count_label(output.review.summary.clean_ops, "clean op"),
        count_label(output.review.summary.conflict_groups, "conflict group"),
    ));
    text.push('\n');
    text.push_str("Attempts\n");
    if output.attempts.is_empty() {
        text.push_str("  - none\n");
    } else {
        for attempt in &output.attempts {
            text.push_str(&format!(
                "  - {}: attempt {}, checks {}/{}, {}, {}, {}\n",
                attempt.lane,
                if attempt.attempt_ok { "ok" } else { "failed" },
                attempt.checks_passed,
                attempt.checks_passed + attempt.checks_failed,
                count_label(attempt.changed_paths, "changed path"),
                count_label(attempt.clean_ops, "clean op"),
                count_label(attempt.conflicted_ops, "conflicted op"),
            ));
            for action in &attempt.actions {
                text.push_str(&format!(
                    "    {}: {}\n",
                    action.kind,
                    format_command(action.command.iter().map(String::as_str))
                ));
            }
        }
    }

    if !output.run.checks.is_empty() {
        text.push_str("\nChecks\n");
        for check in &output.run.checks {
            text.push_str(&format!("  - {}\n", check.name));
            for attempt in &check.attempts {
                text.push_str(&format!(
                    "    {}: {}\n",
                    attempt.lane,
                    attempt_status_label(
                        attempt.ok(),
                        attempt.exec.as_ref(),
                        attempt.orchestration_error.as_deref()
                    )
                ));
            }
        }
    }

    text.push_str("\nNeeds decision\n");
    if output.review.summary.conflict_groups == 0 {
        text.push_str("  - none\n");
    } else {
        for path in &output.review.paths {
            for (index, conflict) in path.conflicts.iter().enumerate() {
                text.push_str(&format!(
                    "  - {} group {} [{}..{}), lanes: {}\n",
                    path.path,
                    index + 1,
                    conflict.range_start,
                    conflict.range_end,
                    conflict.lanes.join(", "),
                ));
                for action in conflict
                    .actions
                    .iter()
                    .filter(|action| matches!(action.kind, ReviewActionKind::ResolveOp))
                {
                    text.push_str(&format!(
                        "    resolve: {}\n",
                        format_command(action.command.iter().map(String::as_str))
                    ));
                }
            }
        }
    }

    text
}

fn attempt_status_label(ok: bool, exec: Option<&RecordedExec>, error: Option<&str>) -> String {
    if let Some(error) = error {
        return format!("orchestration error: {error}");
    }
    let Some(exec) = exec else {
        return "missing exec result".to_owned();
    };
    if ok {
        "ok".to_owned()
    } else if let Some(error) = &exec.worker_error {
        format!("worker error: {error}")
    } else {
        format!(
            "exit {}",
            exec.exit_code
                .map_or_else(|| "none".to_owned(), |code| code.to_string())
        )
    }
}

fn count_label(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RunRecord {
    version: u32,
    name: String,
    command: Vec<String>,
    attempts: Vec<AttemptRecord>,
    checks: Vec<CheckRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AttemptRecord {
    index: usize,
    lane: LaneId,
    exec: Option<RecordedExec>,
    orchestration_error: Option<String>,
}

impl AttemptRecord {
    fn thread_panic(kind: &str, index: usize, lane: LaneId) -> Self {
        Self {
            index,
            lane,
            exec: None,
            orchestration_error: Some(format!("{kind} thread panicked")),
        }
    }

    fn ok(&self) -> bool {
        self.orchestration_error.is_none()
            && self
                .exec
                .as_ref()
                .is_some_and(|exec| exec.worker_error.is_none() && exec.exit_code == Some(0))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CheckRecord {
    name: String,
    command: Vec<String>,
    attempts: Vec<AttemptRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RecordedExec {
    exit_code: Option<i32>,
    worker_error: Option<String>,
    stdout: LaneTextPreview,
    stderr: LaneTextPreview,
    changed_paths: Vec<FilePath>,
    total_ms: u64,
    change_count: usize,
    warnings: Vec<String>,
}

#[cfg(windows)]
impl From<crate::virtual_exec::VirtualExecRecord> for RecordedExec {
    fn from(record: crate::virtual_exec::VirtualExecRecord) -> Self {
        Self {
            exit_code: record.exec.exit_code,
            worker_error: record.exec.worker_error,
            stdout: record.exec.stdout,
            stderr: record.exec.stderr,
            changed_paths: record.exec.changed_paths,
            total_ms: record.total_ms,
            change_count: record.change_count,
            warnings: record.warnings,
        }
    }
}

#[derive(Serialize)]
struct TryOutput {
    repo_root: String,
    storage_path: String,
    run: RunRecord,
}

#[derive(Serialize)]
struct CheckOutput {
    repo_root: String,
    storage_path: String,
    run: String,
    check: CheckRecord,
}

#[derive(Serialize)]
struct CompareOutput {
    repo_root: String,
    storage_path: String,
    run: RunRecord,
    attempts: Vec<CompareAttempt>,
    review: ReviewOutput,
}

#[derive(Clone, Debug, Serialize)]
struct CompareAttempt {
    index: usize,
    lane: LaneId,
    attempt_ok: bool,
    attempt_exit_code: Option<i32>,
    attempt_error: Option<String>,
    checks_passed: usize,
    checks_failed: usize,
    checks: Vec<CompareCheck>,
    changed_paths: usize,
    clean_ops: usize,
    conflicted_ops: usize,
    actions: Vec<CompareAction>,
}

#[derive(Clone, Debug, Serialize)]
struct CompareCheck {
    name: String,
    ok: bool,
    exit_code: Option<i32>,
    worker_error: Option<String>,
    orchestration_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct CompareAction {
    kind: &'static str,
    command: Vec<String>,
}

impl CompareAction {
    fn new<'a>(kind: &'static str, command: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            kind,
            command: command.into_iter().map(str::to_owned).collect(),
        }
    }
}
