use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::time::Instant;

use clap::{Parser, Subcommand};
use serde::Serialize;
use similar::TextDiff;

use crate::materialize::{MaterializeError, MaterializeTimings, run_materialized};
use crate::storage::{acquire_raw_repo_lock, acquire_repo_lock, load_repo, persist_repo};
use crate::vfs::{FileWorktree, LaneFs, LaneFsError};
use crate::{FilePath, LaneRepo};

const STORAGE_PATH: &str = ".lane/repo.lane";

type CliResult<T> = Result<T, CliError>;

#[derive(Parser, Debug)]
#[command(name = "lane")]
#[command(about = "Run agents in isolated lanes without copying the repo")]
pub struct Cli {
    #[arg(long, global = true, value_name = "PATH", default_value = ".")]
    repo_root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(about = "Create an isolated lane")]
    Create {
        lane: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Run a command in a lane through the real repo and print a JSON result")]
    Exec {
        lane: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = "List files changed in a lane")]
    Changes {
        lane: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Show a text diff for a lane")]
    Diff { lane: String, paths: Vec<String> },
    #[command(about = "Promote one lane file into the normal repo")]
    Promote {
        lane: String,
        path: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Promote every changed file in a lane")]
    PromoteLane {
        lane: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Remove a lane and its private changes")]
    Discard {
        lane: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn run() -> CliResult<ExitCode> {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> CliResult<ExitCode> {
    let repo_root = repo_root(cli.repo_root)?;
    match cli.command {
        Command::Create { lane, json } => {
            create(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Exec { lane, command } => exec(&repo_root, &lane, command),
        Command::Changes { lane, json } => {
            changes(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Diff { lane, paths } => diff(&repo_root, &lane, paths).map(|()| ExitCode::SUCCESS),
        Command::Promote { lane, path, json } => {
            promote(&repo_root, &lane, &path, json).map(|()| ExitCode::SUCCESS)
        }
        Command::PromoteLane { lane, json } => {
            promote_lane(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Discard { lane, json } => {
            discard(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
    }
}

fn create(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
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
    if json {
        print_json(&output)?;
    } else if created {
        println!("created lane {lane}");
    } else {
        println!("lane {lane} already exists");
    }
    Ok(())
}

fn exec(repo_root: &Path, lane: &str, command: Vec<String>) -> CliResult<ExitCode> {
    let output = run_lane(repo_root, lane, &command)?;
    let failed = output.exit_code != Some(0)
        || output.worker_error.is_some()
        || output.restore_error.is_some();
    print_json(&output)?;
    if failed {
        Ok(ExitCode::FAILURE)
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn run_lane(repo_root: &Path, lane: &str, command: &[String]) -> CliResult<ExecOutput> {
    let total_start = Instant::now();
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliError::Message("missing command for lane exec".to_owned()))?;
    let storage_path = storage_path(repo_root);

    let pre_storage_lock_wait_start = Instant::now();
    let pre_storage_lock = acquire_repo_lock(&storage_path)?;
    let pre_storage_lock_wait_ms = elapsed_ms(pre_storage_lock_wait_start);
    let pre_storage_lock_held_start = Instant::now();
    let (_, mut fs) = open_lane_fs(repo_root)?;
    fs.create_lane(lane)?;
    let pre_storage_lock_held_ms = elapsed_ms(pre_storage_lock_held_start);
    drop(pre_storage_lock);

    let raw_lock_wait_start = Instant::now();
    let raw_lock = acquire_raw_repo_lock(&storage_path)?;
    let raw_lock_wait_ms = elapsed_ms(raw_lock_wait_start);
    let raw_lock_held_start = Instant::now();
    let mut materialized = run_materialized(repo_root, &fs, lane, || {
        run_raw_worker(program, args, lane, repo_root)
    })?;
    let raw_lock_held_ms = elapsed_ms(raw_lock_held_start);
    drop(raw_lock);

    let post_storage_lock_wait_start = Instant::now();
    let post_storage_lock = acquire_repo_lock(&storage_path)?;
    let post_storage_lock_wait_ms = elapsed_ms(post_storage_lock_wait_start);
    let post_storage_lock_held_start = Instant::now();
    let (storage_path, mut fs) = open_lane_fs(repo_root)?;
    fs.create_lane(lane)?;
    let mut materialize_timings = materialized.timings;
    if let Some(changes_to_ingest) = materialized.changes_to_ingest.take() {
        let ingest_start = Instant::now();
        changes_to_ingest.ingest_into(&mut fs, lane)?;
        materialize_timings.ingest_ms = elapsed_ms(ingest_start);
    }
    if materialized.restore_error.is_none() {
        persist_repo(&storage_path, fs.repo())?;
    }
    let changes = collect_changes(&fs, lane)?;
    let post_storage_lock_held_ms = elapsed_ms(post_storage_lock_held_start);
    drop(post_storage_lock);
    finish_exec_output(ExecOutputContext {
        total_start,
        repo_root,
        lane,
        storage_path,
        materialized,
        breakdown: WorkerTimingBreakdown {
            pre_storage_lock_wait_ms,
            pre_storage_lock_held_ms,
            raw_lock_wait_ms,
            raw_lock_held_ms,
            post_storage_lock_wait_ms,
            post_storage_lock_held_ms,
        },
        materialize_timings,
        changes,
    })
}

struct WorkerTimingBreakdown {
    pre_storage_lock_wait_ms: u64,
    pre_storage_lock_held_ms: u64,
    raw_lock_wait_ms: u64,
    raw_lock_held_ms: u64,
    post_storage_lock_wait_ms: u64,
    post_storage_lock_held_ms: u64,
}

struct ExecOutputContext<'a> {
    total_start: Instant,
    repo_root: &'a Path,
    lane: &'a str,
    storage_path: PathBuf,
    materialized: crate::materialize::MaterializedRun<WorkerOutput>,
    breakdown: WorkerTimingBreakdown,
    materialize_timings: MaterializeTimings,
    changes: Vec<ChangeOutput>,
}

fn finish_exec_output(input: ExecOutputContext<'_>) -> CliResult<ExecOutput> {
    let ExecOutputContext {
        total_start,
        repo_root,
        lane,
        storage_path,
        materialized,
        breakdown,
        materialize_timings,
        changes,
    } = input;
    let storage_lock_wait_ms =
        breakdown.pre_storage_lock_wait_ms + breakdown.post_storage_lock_wait_ms;
    let storage_lock_held_ms =
        breakdown.pre_storage_lock_held_ms + breakdown.post_storage_lock_held_ms;
    let lock_wait_ms = storage_lock_wait_ms + breakdown.raw_lock_wait_ms;
    let lock_held_ms = storage_lock_held_ms + breakdown.raw_lock_held_ms;
    let timings = ExecTimings {
        total_ms: elapsed_ms(total_start),
        lock_wait_ms,
        lock_held_ms,
        storage_lock_wait_ms,
        storage_lock_held_ms,
        raw_lock_wait_ms: breakdown.raw_lock_wait_ms,
        raw_lock_held_ms: breakdown.raw_lock_held_ms,
        pre_worker_lock_ms: breakdown.pre_storage_lock_held_ms + materialize_timings.pre_worker_ms,
        worker_ms: materialize_timings.worker_ms,
        post_worker_lock_ms: materialize_timings.post_worker_ms
            + breakdown.post_storage_lock_held_ms,
        materialize: materialize_timings,
    };
    let worker = materialized.output;

    Ok(ExecOutput {
        lane: lane.to_owned(),
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        workspace_root: path_label(repo_root),
        mode: "raw_repo",
        projected_paths: materialized.projected_paths,
        exit_code: worker.exit_code,
        stdout: worker.stdout,
        stderr: worker.stderr,
        worker_error: worker.worker_error,
        restored: materialized.restored,
        restore_error: materialized.restore_error,
        changed_paths: materialized.changed_paths,
        timings,
        changes,
    })
}

fn run_raw_worker(program: &str, args: &[String], lane: &str, repo_root: &Path) -> WorkerOutput {
    match command_with_lane_env(program, args, lane, repo_root).output() {
        Ok(output) => WorkerOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            worker_error: None,
        },
        Err(error) => WorkerOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            worker_error: Some(error.to_string()),
        },
    }
}

fn command_with_lane_env<'a>(
    program: &'a str,
    args: &'a [String],
    lane: &'a str,
    repo_root: &'a Path,
) -> ProcessCommand {
    let repo_root_label = path_label(repo_root);
    let mut command = ProcessCommand::new(program);
    command
        .args(args)
        .current_dir(repo_root)
        .env("LANE_ID", lane)
        .env("LANE_REPO_ROOT", &repo_root_label)
        .env("LANE_VIEW_ROOT", &repo_root_label)
        .env("LANE_EXEC_MODE", "raw_repo")
        .env_remove("LANE_STORAGE_PATH");
    command
}

fn changes(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _raw_lock = acquire_raw_repo_lock(&storage_path)?;
    let _lock = acquire_repo_lock(&storage_path)?;
    let (_, fs) = open_lane_fs(repo_root)?;
    let output = ChangesOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path),
        changes: collect_changes(&fs, lane)?,
    };

    if json {
        print_json(&output)?;
    } else if output.changes.is_empty() {
        println!("no changes in lane {lane}");
    } else {
        for change in &output.changes {
            println!("{}\t{}", change.status.short(), change.path);
        }
    }
    Ok(())
}

fn diff(repo_root: &Path, lane: &str, paths: Vec<String>) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _raw_lock = acquire_raw_repo_lock(&storage_path)?;
    let _lock = acquire_repo_lock(&storage_path)?;
    let (_, fs) = open_lane_fs(repo_root)?;
    let changes = if paths.is_empty() {
        collect_changes(&fs, lane)?
    } else {
        paths
            .into_iter()
            .map(|path| change_for_path(&fs, lane, path))
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

fn promote(repo_root: &Path, lane: &str, path: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _raw_lock = acquire_raw_repo_lock(&storage_path)?;
    let _lock = acquire_repo_lock(&storage_path)?;
    let (storage_path, mut fs) = open_lane_fs(repo_root)?;
    let before = change_for_path(&fs, lane, path)?;
    fs.promote_file(lane, path)?;
    persist_repo(&storage_path, fs.repo())?;

    let promoted = before.into_iter().collect::<Vec<_>>();
    let output = PromoteOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        promoted,
    };
    if json {
        print_json(&output)?;
    } else if output.promoted.is_empty() {
        println!("no changes promoted from lane {lane}");
    } else {
        for change in &output.promoted {
            println!("promoted {}\t{}", change.status.short(), change.path);
        }
    }
    Ok(())
}

fn promote_lane(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _raw_lock = acquire_raw_repo_lock(&storage_path)?;
    let _lock = acquire_repo_lock(&storage_path)?;
    let (storage_path, mut fs) = open_lane_fs(repo_root)?;
    let before = collect_changes(&fs, lane)?;
    fs.promote_lane(lane)?;
    persist_repo(&storage_path, fs.repo())?;

    let output = PromoteOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        promoted: before,
    };
    if json {
        print_json(&output)?;
    } else if output.promoted.is_empty() {
        println!("no changes promoted from lane {lane}");
    } else {
        for change in &output.promoted {
            println!("promoted {}\t{}", change.status.short(), change.path);
        }
    }
    Ok(())
}

fn discard(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _raw_lock = acquire_raw_repo_lock(&storage_path)?;
    let _lock = acquire_repo_lock(&storage_path)?;
    let (storage_path, mut fs) = open_lane_fs(repo_root)?;
    let discarded_changes = collect_changes(&fs, lane).map_or(0, |changes| changes.len());
    let removed = fs.discard_lane(lane);
    persist_repo(&storage_path, fs.repo())?;

    let output = DiscardOutput {
        lane,
        removed,
        discarded_changes,
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
    };
    if json {
        print_json(&output)?;
    } else if removed {
        println!("discarded lane {lane} ({discarded_changes} changed paths)");
    } else {
        println!("lane {lane} did not exist");
    }
    Ok(())
}

fn collect_changes(fs: &LaneFs<FileWorktree>, lane: &str) -> CliResult<Vec<ChangeOutput>> {
    fs.changed_paths(lane)?
        .into_iter()
        .map(|path| change_for_path(fs, lane, path))
        .collect::<CliResult<Vec<_>>>()
        .map(|changes| changes.into_iter().flatten().collect())
}

fn change_for_path(
    fs: &LaneFs<FileWorktree>,
    lane: &str,
    path: impl Into<String>,
) -> CliResult<Option<ChangeOutput>> {
    let path = path.into();
    let base = fs.base_file(&path)?;
    let lane_bytes = fs.read_file(lane, &path)?;
    if base == lane_bytes {
        return Ok(None);
    }
    let status = match (&base, &lane_bytes) {
        (None, Some(_)) => ChangeStatus::Created,
        (Some(_), None) => ChangeStatus::Deleted,
        (Some(_), Some(_)) => ChangeStatus::Modified,
        (None, None) => return Ok(None),
    };
    Ok(Some(ChangeOutput {
        path,
        status,
        base_size: base.as_ref().map(Vec::len),
        lane_size: lane_bytes.as_ref().map(Vec::len),
        base,
        lane: lane_bytes,
    }))
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

fn open_lane_fs(repo_root: &Path) -> CliResult<(PathBuf, LaneFs<FileWorktree>)> {
    let storage_path = storage_path(repo_root);
    let repo = load_lane_repo(&storage_path)?;
    Ok((
        storage_path,
        LaneFs::new(repo, FileWorktree::new(repo_root)),
    ))
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
        CliError::Message(format!(
            "repo root {} is not readable: {error}",
            path.display()
        ))
    })?;
    if !root.is_dir() {
        return Err(CliError::Message(format!(
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

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

#[derive(Serialize)]
struct CreateOutput<'a> {
    lane: &'a str,
    created: bool,
    repo_root: String,
    storage_path: String,
}

struct WorkerOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    worker_error: Option<String>,
}

#[derive(Serialize)]
struct ExecOutput {
    lane: String,
    repo_root: String,
    storage_path: String,
    workspace_root: String,
    mode: &'static str,
    projected_paths: Vec<FilePath>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    worker_error: Option<String>,
    restored: bool,
    restore_error: Option<String>,
    changed_paths: Vec<FilePath>,
    timings: ExecTimings,
    changes: Vec<ChangeOutput>,
}

#[derive(Serialize)]
struct ExecTimings {
    total_ms: u64,
    lock_wait_ms: u64,
    lock_held_ms: u64,
    storage_lock_wait_ms: u64,
    storage_lock_held_ms: u64,
    raw_lock_wait_ms: u64,
    raw_lock_held_ms: u64,
    pre_worker_lock_ms: u64,
    worker_ms: u64,
    post_worker_lock_ms: u64,
    materialize: MaterializeTimings,
}

#[derive(Serialize)]
struct ChangesOutput<'a> {
    lane: &'a str,
    repo_root: String,
    storage_path: String,
    changes: Vec<ChangeOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct ChangeOutput {
    path: FilePath,
    status: ChangeStatus,
    base_size: Option<usize>,
    lane_size: Option<usize>,
    #[serde(skip_serializing)]
    base: Option<Vec<u8>>,
    #[serde(skip_serializing)]
    lane: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChangeStatus {
    Created,
    Modified,
    Deleted,
}

impl ChangeStatus {
    fn short(self) -> &'static str {
        match self {
            Self::Created => "A",
            Self::Modified => "M",
            Self::Deleted => "D",
        }
    }
}

#[derive(Serialize)]
struct PromoteOutput<'a> {
    lane: &'a str,
    repo_root: String,
    storage_path: String,
    promoted: Vec<ChangeOutput>,
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
pub enum CliError {
    Io(io::Error),
    LaneFs(LaneFsError),
    Lane(crate::LaneError),
    Materialize(String),
    Json(serde_json::Error),
    Message(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::LaneFs(error) => write!(f, "{error}"),
            Self::Lane(error) => write!(f, "{error:?}"),
            Self::Materialize(error) => write!(f, "{error}"),
            Self::Json(error) => write!(f, "{error}"),
            Self::Message(message) => write!(f, "{message}"),
        }
    }
}

impl Error for CliError {}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LaneFsError> for CliError {
    fn from(error: LaneFsError) -> Self {
        Self::LaneFs(error)
    }
}

impl From<crate::LaneError> for CliError {
    fn from(error: crate::LaneError) -> Self {
        Self::Lane(error)
    }
}

impl From<MaterializeError> for CliError {
    fn from(error: MaterializeError) -> Self {
        Self::Materialize(error.to_string())
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
