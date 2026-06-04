use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

use serde::Serialize;

use crate::FilePath;
use crate::storage::persist_bytes;
use crate::vfs::{FileWorktree, LaneFs, LaneFsError};

const BASE_GUARD_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".lane",
    "coverage",
    "dist",
    "node_modules",
    "target",
];

pub(crate) fn run_materialized<T>(
    repo_root: &Path,
    fs: &mut LaneFs<FileWorktree>,
    lane: &str,
    operation: impl FnOnce() -> T,
) -> Result<MaterializedRun<T>, MaterializeError> {
    let total_start = Instant::now();
    let mut timings = MaterializeTimings::default();

    let snapshot_start = Instant::now();
    let before = BaseSnapshot::capture(repo_root)?;
    timings.snapshot_ms = elapsed_ms(snapshot_start);

    let project_start = Instant::now();
    let projected_paths = project_lane(repo_root, fs, lane)?;
    let projected = BaseSnapshot::capture(repo_root)?;
    timings.project_ms = elapsed_ms(project_start);

    let operation_start = Instant::now();
    let output = operation();
    timings.operation_ms = elapsed_ms(operation_start);

    let detect_start = Instant::now();
    let changed_paths = projected.changed_paths(repo_root)?;
    timings.detect_ms = elapsed_ms(detect_start);

    let capture_start = Instant::now();
    let raw_changes = capture_raw_changes(repo_root, &changed_paths);
    timings.capture_ms = elapsed_ms(capture_start);

    let restore_start = Instant::now();
    let restore_paths = before.changed_paths(repo_root)?;
    let (restored, restore_error) = if restore_paths.is_empty() {
        (false, None)
    } else {
        match before.rollback(repo_root, &restore_paths) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.to_string())),
        }
    };
    timings.restore_ms = elapsed_ms(restore_start);

    let raw_changes = raw_changes?;
    if restore_error.is_none() {
        let ingest_start = Instant::now();
        ingest_raw_changes(fs, lane, &projected, raw_changes)?;
        timings.ingest_ms = elapsed_ms(ingest_start);
    }

    timings.total_ms = elapsed_ms(total_start);
    Ok(MaterializedRun {
        output,
        projected_paths,
        restored,
        restore_error,
        changed_paths,
        timings,
    })
}

pub(crate) struct MaterializedRun<T> {
    pub(crate) output: T,
    pub(crate) projected_paths: Vec<FilePath>,
    pub(crate) restored: bool,
    pub(crate) restore_error: Option<String>,
    pub(crate) changed_paths: Vec<FilePath>,
    pub(crate) timings: MaterializeTimings,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct MaterializeTimings {
    pub(crate) total_ms: u64,
    pub(crate) snapshot_ms: u64,
    pub(crate) project_ms: u64,
    pub(crate) operation_ms: u64,
    pub(crate) detect_ms: u64,
    pub(crate) capture_ms: u64,
    pub(crate) restore_ms: u64,
    pub(crate) ingest_ms: u64,
}

#[derive(Debug)]
pub(crate) enum MaterializeError {
    Io(io::Error),
    LaneFs(LaneFsError),
}

impl fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::LaneFs(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for MaterializeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LaneFsError> for MaterializeError {
    fn from(error: LaneFsError) -> Self {
        Self::LaneFs(error)
    }
}

fn project_lane(
    repo_root: &Path,
    fs: &LaneFs<FileWorktree>,
    lane: &str,
) -> Result<Vec<FilePath>, MaterializeError> {
    let paths = fs.changed_paths(lane)?;
    for path in &paths {
        let target = repo_root.join(path);
        match fs.read_file(lane, path)? {
            Some(bytes) => restore_file(&target, &bytes)?,
            None => remove_projected_path(&target)?,
        }
    }
    Ok(paths)
}

fn capture_raw_changes(repo_root: &Path, paths: &[FilePath]) -> io::Result<Vec<RawChange>> {
    paths
        .iter()
        .map(|path| {
            let target = repo_root.join(path);
            let content = if target.is_dir() {
                None
            } else {
                match fs::read(target) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) if error.kind() == io::ErrorKind::IsADirectory => None,
                    Err(error) if error.kind() == io::ErrorKind::NotADirectory => None,
                    Err(error) => return Err(error),
                }
            };
            Ok(RawChange {
                path: path.clone(),
                content,
            })
        })
        .collect()
}

fn ingest_raw_changes(
    fs: &mut LaneFs<FileWorktree>,
    lane: &str,
    before: &BaseSnapshot,
    changes: Vec<RawChange>,
) -> Result<(), MaterializeError> {
    for change in changes {
        match change.content {
            Some(bytes) => fs.write_file(lane, &change.path, bytes)?,
            None if before.files.contains_key(&change.path) => {
                fs.delete_file(lane, &change.path)?;
            }
            None => {}
        }
    }
    Ok(())
}

struct RawChange {
    path: FilePath,
    content: Option<Vec<u8>>,
}

#[derive(Debug)]
struct BaseSnapshot {
    files: BTreeMap<FilePath, SnapshotFile>,
    dirs: BTreeSet<FilePath>,
}

impl BaseSnapshot {
    fn capture(repo_root: &Path) -> io::Result<Self> {
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        collect_base_snapshot(repo_root, repo_root, &mut files, &mut dirs)?;
        Ok(Self { files, dirs })
    }

    fn changed_paths(&self, repo_root: &Path) -> io::Result<Vec<FilePath>> {
        let after = Self::capture(repo_root)?;
        let mut paths = BTreeSet::new();
        paths.extend(self.files.keys().cloned());
        paths.extend(after.files.keys().cloned());
        paths.extend(self.dirs.iter().cloned());
        paths.extend(after.dirs.iter().cloned());
        Ok(paths
            .into_iter()
            .filter(|path| {
                self.files.get(path) != after.files.get(path)
                    || self.dirs.contains(path) != after.dirs.contains(path)
            })
            .collect())
    }

    fn rollback(&self, repo_root: &Path, paths: &[FilePath]) -> io::Result<()> {
        let after = Self::capture(repo_root)?;
        for path in paths {
            if self.files.get(path) == after.files.get(path) {
                continue;
            }
            match self.files.get(path) {
                Some(file) => restore_file(&repo_root.join(path), &file.bytes)?,
                None => remove_created_file(&repo_root.join(path))?,
            }
        }
        for path in paths.iter().filter(|path| self.dirs.contains(*path)) {
            if !repo_root.join(path).is_dir() {
                fs::create_dir_all(repo_root.join(path))?;
            }
        }
        let mut created_dirs = paths
            .iter()
            .filter(|path| {
                !self.dirs.contains(*path)
                    && after.dirs.contains(*path)
                    && !self.files.contains_key(*path)
            })
            .collect::<Vec<_>>();
        created_dirs.sort_by_key(|path| std::cmp::Reverse(path_depth(path)));
        for path in created_dirs {
            remove_created_dir(repo_root, path)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotFile {
    bytes: Vec<u8>,
}

fn collect_base_snapshot(
    repo_root: &Path,
    directory: &Path,
    files: &mut BTreeMap<FilePath, SnapshotFile>,
    dirs: &mut BTreeSet<FilePath>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(repo_root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_exec_guard_dir(relative) {
                continue;
            }
            let label = relative.to_string_lossy().replace('\\', "/");
            if !label.is_empty() {
                dirs.insert(label);
            }
            collect_base_snapshot(repo_root, &path, files, dirs)?;
        } else if file_type.is_file() {
            let Some(file) = snapshot_file(&path)? else {
                continue;
            };
            files.insert(relative.to_string_lossy().replace('\\', "/"), file);
        }
    }
    Ok(())
}

fn should_skip_exec_guard_dir(relative: &Path) -> bool {
    relative.components().next().is_some_and(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        BASE_GUARD_IGNORED_DIRS.contains(&name.as_str())
    })
}

fn snapshot_file(path: &Path) -> io::Result<Option<SnapshotFile>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(SnapshotFile { bytes })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn restore_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)?;
    }
    persist_bytes(path, bytes)
}

fn remove_projected_path(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        remove_created_file(path)
    }
}

fn remove_created_file(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_created_dir(repo_root: &Path, path: &str) -> io::Result<()> {
    let directory = repo_root.join(path);
    match fs::remove_dir(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error),
    }
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}
