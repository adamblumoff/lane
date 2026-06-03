use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode, ExitStatus};
use std::thread;

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use similar::TextDiff;

use crate::storage::{RepoLock, acquire_repo_lock, load_repo, persist_repo, try_acquire_path_lock};
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
    #[command(about = "Run a command inside an isolated lane view")]
    Exec {
        lane: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = "Run multiple lane commands in parallel from a JSON plan")]
    RunPlan {
        plan: PathBuf,
        #[arg(long)]
        json: bool,
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
        Command::RunPlan { plan, json } => {
            run_plan(&repo_root, &plan, json).map(|()| ExitCode::SUCCESS)
        }
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

#[cfg(windows)]
fn exec(repo_root: &Path, lane: &str, command: Vec<String>) -> CliResult<ExitCode> {
    let status = run_lane_status(repo_root, lane, &command)?;
    if status.success() {
        Ok(ExitCode::SUCCESS)
    } else {
        Err(CliError::ChildFailed(status))
    }
}

#[cfg(not(windows))]
fn exec(_repo_root: &Path, _lane: &str, _command: Vec<String>) -> CliResult<ExitCode> {
    Err(CliError::Message(
        "lane exec requires Windows and WinFsp".to_owned(),
    ))
}

#[cfg(windows)]
fn run_lane_status(repo_root: &Path, lane: &str, command: &[String]) -> CliResult<ExitStatus> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliError::Message("missing command for lane exec".to_owned()))?;
    let mounted = mount_exec_lane(repo_root, lane)?;
    let view_root = mounted.view_root();
    Ok(command_with_lane_env(program, args, repo_root, lane, &view_root).status()?)
}

#[cfg(windows)]
fn run_lane_output(
    repo_root: &Path,
    lane: &str,
    command: &[String],
) -> CliResult<LaneCommandOutput> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| CliError::Message("missing command for lane exec".to_owned()))?;
    let mounted = mount_exec_lane(repo_root, lane)?;
    let view_root = mounted.view_root();
    let output = command_with_lane_env(program, args, repo_root, lane, &view_root).output()?;
    Ok(LaneCommandOutput {
        lane: lane.to_owned(),
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(windows)]
fn command_with_lane_env<'a>(
    program: &'a str,
    args: &'a [String],
    repo_root: &'a Path,
    lane: &'a str,
    view_root: &'a Path,
) -> ProcessCommand {
    let mut command = ProcessCommand::new(program);
    command
        .args(args)
        .current_dir(view_root)
        .env("LANE_ID", lane)
        .env("LANE_REPO_ROOT", repo_root)
        .env("LANE_VIEW_ROOT", view_root)
        .env("LANE_STORAGE_PATH", storage_path(repo_root));
    command
}

#[cfg(not(windows))]
fn run_lane_output(
    _repo_root: &Path,
    _lane: &str,
    _command: &[String],
) -> CliResult<LaneCommandOutput> {
    Err(CliError::Message(
        "lane run-plan requires Windows and WinFsp".to_owned(),
    ))
}

struct LaneCommandOutput {
    lane: String,
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_plan(repo_root: &Path, plan_path: &Path, json: bool) -> CliResult<()> {
    let plan_bytes = fs::read(plan_path)?;
    let plan: RunPlanInput = serde_json::from_slice(&plan_bytes)?;
    if plan.lanes.is_empty() {
        return Err(CliError::Message("run plan must include lanes".to_owned()));
    }

    let handles = plan
        .lanes
        .into_iter()
        .map(|lane| {
            let repo_root = repo_root.to_path_buf();
            thread::spawn(move || run_plan_lane(&repo_root, lane))
        })
        .collect::<Vec<_>>();

    let mut lanes = Vec::new();
    for handle in handles {
        lanes.push(
            handle
                .join()
                .map_err(|_| CliError::Message("run-plan worker panicked".to_owned()))??,
        );
    }
    lanes.sort_by(|left, right| left.id.cmp(&right.id));
    let failed = lanes.iter().any(|lane| lane.exit_code != Some(0));
    let output = RunPlanOutput {
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path(repo_root)),
        failed,
        lanes,
    };

    if json {
        print_json(&output)?;
    } else {
        for lane in &output.lanes {
            let status = lane
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated".to_owned());
            println!(
                "{}\texit={}\t{} changes",
                lane.id,
                status,
                lane.changes.len()
            );
        }
    }

    if failed {
        Err(CliError::RunPlanFailed)
    } else {
        Ok(())
    }
}

fn run_plan_lane(repo_root: &Path, lane: RunPlanLaneInput) -> CliResult<RunPlanLaneOutput> {
    if lane.command.is_empty() {
        return Err(CliError::Message(format!(
            "lane {} has an empty command",
            lane.id
        )));
    }
    let run = run_lane_output(repo_root, &lane.id, &lane.command)?;
    let (_, fs) = open_lane_fs(repo_root)?;
    Ok(RunPlanLaneOutput {
        id: lane.id,
        exit_code: run.status.code(),
        stdout: String::from_utf8_lossy(&run.stdout).to_string(),
        stderr: String::from_utf8_lossy(&run.stderr).to_string(),
        changes: collect_changes(&fs, &run.lane)?,
    })
}

fn changes(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let (_, fs) = open_lane_fs(repo_root)?;
    let output = ChangesOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path(repo_root)),
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
    let _lock = acquire_repo_lock(&storage_path(repo_root))?;
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
    let _lock = acquire_repo_lock(&storage_path(repo_root))?;
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
    let _lock = acquire_repo_lock(&storage_path(repo_root))?;
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

#[cfg(windows)]
struct ExecMount {
    mounted: crate::winfsp_mount::MountedLane,
    _drive_lock: Option<RepoLock>,
}

#[cfg(windows)]
impl ExecMount {
    fn view_root(&self) -> PathBuf {
        self.mounted.view_root()
    }
}

#[cfg(windows)]
fn mount_exec_lane(repo_root: &Path, lane: &str) -> CliResult<ExecMount> {
    use crate::winfsp_mount::{MountOptions, mount_hidden};

    let mut last_error = None;
    for letter in (b'D'..=b'Z').rev() {
        let drive_root = format!("{}:\\", letter as char);
        if Path::new(&drive_root).exists() {
            continue;
        }
        let lock_path = std::env::temp_dir().join(format!("lane-drive-{}.lock", letter as char));
        let Some(drive_lock) = try_acquire_path_lock(&lock_path)? else {
            continue;
        };
        let mount_path = PathBuf::from(format!("{}:", letter as char));
        match mount_hidden(MountOptions {
            repo_root: repo_root.to_path_buf(),
            lane: lane.to_owned(),
            mount_path,
        }) {
            Ok(mounted) => {
                return Ok(ExecMount {
                    mounted,
                    _drive_lock: Some(drive_lock),
                });
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    if let Some(error) = last_error {
        Err(error.into())
    } else {
        Err(CliError::Message(
            "no free drive letter available for lane exec".to_owned(),
        ))
    }
}

fn path_label(path: impl AsRef<Path>) -> String {
    path.as_ref().display().to_string()
}

fn print_json(output: &impl Serialize) -> CliResult<()> {
    println!("{}", serde_json::to_string(output)?);
    Ok(())
}

#[derive(Serialize)]
struct CreateOutput<'a> {
    lane: &'a str,
    created: bool,
    repo_root: String,
    storage_path: String,
}

#[derive(Deserialize)]
struct RunPlanInput {
    lanes: Vec<RunPlanLaneInput>,
}

#[derive(Deserialize)]
struct RunPlanLaneInput {
    id: String,
    command: Vec<String>,
}

#[derive(Serialize)]
struct RunPlanOutput {
    repo_root: String,
    storage_path: String,
    failed: bool,
    lanes: Vec<RunPlanLaneOutput>,
}

#[derive(Serialize)]
struct RunPlanLaneOutput {
    id: String,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    changes: Vec<ChangeOutput>,
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
    Json(serde_json::Error),
    #[cfg(windows)]
    WinFsp(winfsp::FspError),
    ChildFailed(ExitStatus),
    RunPlanFailed,
    Message(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::LaneFs(error) => write!(f, "{error}"),
            Self::Lane(error) => write!(f, "{error:?}"),
            Self::Json(error) => write!(f, "{error}"),
            #[cfg(windows)]
            Self::WinFsp(error) => write!(f, "{}", format_winfsp_error(error)),
            Self::ChildFailed(status) => write!(f, "lane exec command failed with {status}"),
            Self::RunPlanFailed => write!(f, "one or more run-plan lanes failed"),
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

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[cfg(windows)]
impl From<winfsp::FspError> for CliError {
    fn from(error: winfsp::FspError) -> Self {
        Self::WinFsp(error)
    }
}

#[cfg(windows)]
fn format_winfsp_error(error: &winfsp::FspError) -> String {
    match error {
        winfsp::FspError::HRESULT(code) => {
            format!(
                "HRESULT 0x{:08X}; NTSTATUS 0x{:08X}",
                *code as u32,
                error.to_ntstatus() as u32
            )
        }
        winfsp::FspError::WIN32(code) => {
            format!(
                "WIN32 0x{code:08X}; NTSTATUS 0x{:08X}",
                error.to_ntstatus() as u32
            )
        }
        winfsp::FspError::NTSTATUS(code) => format!("NTSTATUS 0x{:08X}", *code as u32),
        winfsp::FspError::IO(kind) => {
            format!("IO {kind:?}; NTSTATUS 0x{:08X}", error.to_ntstatus() as u32)
        }
        _ => format!("{error}; NTSTATUS 0x{:08X}", error.to_ntstatus() as u32),
    }
}
