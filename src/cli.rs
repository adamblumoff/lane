use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use similar::TextDiff;

use crate::storage::{acquire_repo_lock, load_repo, persist_repo};
use crate::vfs::{FileWorktree, LaneFs, LaneFsError};
use crate::{FilePath, LaneOpSummary, LaneRepo};

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
    #[command(about = "Run a command in a lane through a virtual mounted lane view")]
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
    #[command(about = "List lane operations that conflict with other lanes")]
    Conflicts {
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
    #[command(about = "Promote selected lane operations into the normal repo")]
    PromoteOps {
        lane: String,
        path: String,
        #[arg(required = true)]
        ops: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Promote every changed file in a lane")]
    PromoteLane {
        lane: String,
        #[arg(long)]
        json: bool,
    },
    #[command(about = "Promote every non-conflicting operation in a lane")]
    PromoteClean {
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
        Command::Exec { lane, command } => exec(&repo_root, &lane, &command),
        Command::Changes { lane, json } => {
            changes(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Conflicts { lane, json } => {
            conflicts(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
        Command::Diff { lane, paths } => diff(&repo_root, &lane, paths).map(|()| ExitCode::SUCCESS),
        Command::Promote { lane, path, json } => {
            promote(&repo_root, &lane, &path, json).map(|()| ExitCode::SUCCESS)
        }
        Command::PromoteOps {
            lane,
            path,
            ops,
            json,
        } => promote_ops(&repo_root, &lane, &path, &ops, json).map(|()| ExitCode::SUCCESS),
        Command::PromoteLane { lane, json } => {
            promote_lane(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
        }
        Command::PromoteClean { lane, json } => {
            promote_clean(&repo_root, &lane, json).map(|()| ExitCode::SUCCESS)
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
fn exec(repo_root: &Path, lane: &str, command: &[String]) -> CliResult<ExitCode> {
    let run = crate::virtual_exec::run_virtual_lane(repo_root, lane, command)
        .map_err(|error| CliError::Message(error.to_string()))?;
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
    Err(CliError::Message(
        "lane exec requires the WinFsp virtual filesystem on Windows".to_owned(),
    ))
}

fn changes(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
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

fn conflicts(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _lock = acquire_repo_lock(&storage_path)?;
    let (_, fs) = open_lane_fs(repo_root)?;
    let output = ConflictsOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(storage_path),
        conflicts: collect_conflicts(&fs, lane)?,
    };

    if json {
        print_json(&output)?;
    } else if output.conflicts.is_empty() {
        println!("no conflicts in lane {lane}");
    } else {
        for change in &output.conflicts {
            for op in &change.ops {
                println!(
                    "{}\t{}\tconflicts with {}",
                    change.path,
                    op.op_id,
                    op.conflicts_with.join(",")
                );
            }
        }
    }
    Ok(())
}

fn diff(repo_root: &Path, lane: &str, paths: Vec<String>) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
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

fn promote_ops(
    repo_root: &Path,
    lane: &str,
    path: &str,
    ops: &[String],
    json: bool,
) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _lock = acquire_repo_lock(&storage_path)?;
    let (storage_path, mut fs) = open_lane_fs(repo_root)?;
    let before = change_for_path(&fs, lane, path)?;
    fs.promote_ops_file(lane, path, ops)?;
    persist_repo(&storage_path, fs.repo())?;

    let promoted = before.into_iter().collect::<Vec<_>>();
    let output = PromoteOpsOutput {
        lane,
        path,
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        promoted_ops: ops.to_vec(),
        promoted,
    };
    if json {
        print_json(&output)?;
    } else {
        println!(
            "promoted {} op(s) from lane {lane}: {path}",
            output.promoted_ops.len()
        );
    }
    Ok(())
}

fn promote_lane(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
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

fn promote_clean(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
    let _lock = acquire_repo_lock(&storage_path)?;
    let (storage_path, mut fs) = open_lane_fs(repo_root)?;
    let before = collect_changes(&fs, lane)?;
    let promoted = filter_change_ops(&before, |op| op.conflicts_with.is_empty());
    let conflicts = filter_change_ops(&before, |op| !op.conflicts_with.is_empty());
    let promoted_ops = grouped_ops(&promoted);

    for path_ops in &promoted_ops {
        fs.promote_ops_file(lane, &path_ops.path, &path_ops.ops)?;
    }
    if !promoted_ops.is_empty() {
        persist_repo(&storage_path, fs.repo())?;
    }

    let output = PromoteCleanOutput {
        lane,
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        promoted_ops,
        promoted,
        conflicts,
    };
    if json {
        print_json(&output)?;
    } else if output.promoted_ops.is_empty() {
        println!("no clean operations promoted from lane {lane}");
    } else {
        for path_ops in &output.promoted_ops {
            println!(
                "promoted {} clean op(s) from lane {lane}: {}",
                path_ops.ops.len(),
                path_ops.path
            );
        }
    }
    Ok(())
}

fn discard(repo_root: &Path, lane: &str, json: bool) -> CliResult<()> {
    let storage_path = storage_path(repo_root);
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

fn collect_conflicts(fs: &LaneFs<FileWorktree>, lane: &str) -> CliResult<Vec<ChangeOutput>> {
    collect_changes(fs, lane)
        .map(|changes| filter_change_ops(&changes, |op| !op.conflicts_with.is_empty()))
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
    let ops = fs.change_ops(lane, &path)?;
    Ok(Some(ChangeOutput {
        path,
        status,
        base_size: base.as_ref().map(Vec::len),
        lane_size: lane_bytes.as_ref().map(Vec::len),
        ops,
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

#[derive(Serialize)]
struct CreateOutput<'a> {
    lane: &'a str,
    created: bool,
    repo_root: String,
    storage_path: String,
}

#[derive(Serialize)]
struct ChangesOutput<'a> {
    lane: &'a str,
    repo_root: String,
    storage_path: String,
    changes: Vec<ChangeOutput>,
}

#[derive(Serialize)]
struct ConflictsOutput<'a> {
    lane: &'a str,
    repo_root: String,
    storage_path: String,
    conflicts: Vec<ChangeOutput>,
}

#[derive(Clone, Debug, Serialize)]
struct ChangeOutput {
    path: FilePath,
    status: ChangeStatus,
    base_size: Option<usize>,
    lane_size: Option<usize>,
    ops: Vec<LaneOpSummary>,
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
pub enum CliError {
    Io(io::Error),
    LaneFs(LaneFsError),
    Lane(crate::LaneError),
    Json(serde_json::Error),
    Message(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::LaneFs(error) => write!(f, "{error}"),
            Self::Lane(error) => write!(f, "{error:?}"),
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

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}
