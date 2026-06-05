use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
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
const WATCHER_SETTLE_TIMEOUT: Duration = Duration::from_millis(5);
const WATCHER_MAX_DRAIN: Duration = Duration::from_millis(50);
const PARALLEL_BACKUP_MIN_FILES: usize = 32;
const PARALLEL_BACKUP_MAX_WORKERS: usize = 8;

pub(crate) fn run_materialized<T>(
    repo_root: &Path,
    fs: &LaneFs<FileWorktree>,
    lane: &str,
    operation: impl FnOnce() -> T,
) -> Result<MaterializedRun<T>, MaterializeError> {
    let total_start = Instant::now();
    let mut timings = MaterializeTimings::default();

    let snapshot_start = Instant::now();
    let (before, restore_backup) = BaseSnapshot::capture_with_backup(repo_root)?;
    timings.snapshot_ms = elapsed_ms(snapshot_start);

    let project_start = Instant::now();
    let projected_paths = project_lane(repo_root, fs, lane)?;
    let projected = before.snapshot_after_projecting(repo_root, &projected_paths)?;
    timings.project_ms = elapsed_ms(project_start);

    let mut restore_candidates = projected_paths.iter().cloned().collect::<BTreeSet<_>>();
    let dirty_tracker = DirtyTracker::start(repo_root);
    timings.pre_worker_ms = elapsed_ms(total_start);

    let operation_start = Instant::now();
    let output = operation();
    timings.operation_ms = elapsed_ms(operation_start);
    timings.worker_ms = timings.operation_ms;

    let post_worker_start = Instant::now();
    let dirty_paths = dirty_tracker.finish(repo_root);
    restore_candidates.extend(dirty_paths.paths.iter().cloned());

    let detect_start = Instant::now();
    let changed_paths = if dirty_paths.fallback {
        projected.changed_paths(repo_root)?
    } else {
        projected.changed_paths_for_candidates(repo_root, &restore_candidates)?
    };
    timings.detect_ms = elapsed_ms(detect_start);

    let capture_start = Instant::now();
    let raw_changes = capture_raw_changes(repo_root, &changed_paths);
    timings.capture_ms = elapsed_ms(capture_start);

    let restore_start = Instant::now();
    let restore_paths = if dirty_paths.fallback {
        before.changed_paths(repo_root)?
    } else {
        before.changed_paths_for_candidates(repo_root, &restore_candidates)?
    };
    let (restored, restore_error) = if restore_paths.is_empty() {
        (false, None)
    } else {
        match before.rollback(repo_root, &restore_backup, &restore_paths) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.to_string())),
        }
    };
    timings.restore_ms = elapsed_ms(restore_start);

    let raw_changes = raw_changes?;
    let changes_to_ingest = restore_error.is_none().then_some(MaterializedChanges {
        projected,
        raw_changes,
    });

    timings.post_worker_ms = elapsed_ms(post_worker_start);
    timings.total_ms = elapsed_ms(total_start);
    Ok(MaterializedRun {
        output,
        projected_paths,
        restored,
        restore_error,
        changed_paths,
        changes_to_ingest,
        timings,
    })
}

pub(crate) struct MaterializedRun<T> {
    pub(crate) output: T,
    pub(crate) projected_paths: Vec<FilePath>,
    pub(crate) restored: bool,
    pub(crate) restore_error: Option<String>,
    pub(crate) changed_paths: Vec<FilePath>,
    pub(crate) changes_to_ingest: Option<MaterializedChanges>,
    pub(crate) timings: MaterializeTimings,
}

pub(crate) struct MaterializedChanges {
    projected: BaseSnapshot,
    raw_changes: Vec<RawChange>,
}

impl MaterializedChanges {
    pub(crate) fn ingest_into(
        self,
        fs: &mut LaneFs<FileWorktree>,
        lane: &str,
    ) -> Result<(), MaterializeError> {
        ingest_raw_changes(fs, lane, &self.projected, self.raw_changes)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub(crate) struct MaterializeTimings {
    pub(crate) total_ms: u64,
    pub(crate) pre_worker_ms: u64,
    pub(crate) worker_ms: u64,
    pub(crate) post_worker_ms: u64,
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

#[derive(Clone, Debug)]
struct BaseSnapshot {
    files: BTreeMap<FilePath, SnapshotFile>,
    dirs: BTreeSet<FilePath>,
}

impl BaseSnapshot {
    fn capture(repo_root: &Path) -> io::Result<Self> {
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        collect_base_snapshot(repo_root, repo_root, &mut files, &mut dirs, None)?;
        Ok(Self { files, dirs })
    }

    fn capture_with_backup(repo_root: &Path) -> io::Result<(Self, RestoreBackup)> {
        let mut files = BTreeMap::new();
        let mut dirs = BTreeSet::new();
        let mut backup_targets = Vec::new();
        collect_base_snapshot(
            repo_root,
            repo_root,
            &mut files,
            &mut dirs,
            Some(&mut backup_targets),
        )?;
        let backup = read_restore_backup(backup_targets, &mut files)?;
        Ok((Self { files, dirs }, backup))
    }

    fn snapshot_after_projecting(
        &self,
        repo_root: &Path,
        projected_paths: &[FilePath],
    ) -> io::Result<Self> {
        let mut projected = self.clone();
        for path in projected_paths {
            projected.refresh_path_from_worktree(repo_root, path)?;
        }
        Ok(projected)
    }

    fn refresh_path_from_worktree(&mut self, repo_root: &Path, path: &str) -> io::Result<()> {
        remove_snapshot_path_and_descendants(path, &mut self.files, &mut self.dirs);
        let target = repo_root.join(path);
        match fs::metadata(&target) {
            Ok(metadata) if metadata.is_file() => {
                self.files
                    .insert(path.to_owned(), SnapshotFile::from_metadata(&metadata));
            }
            Ok(metadata) if metadata.is_dir() => {
                self.dirs.insert(path.to_owned());
                collect_base_snapshot(repo_root, &target, &mut self.files, &mut self.dirs, None)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(())
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

    fn changed_paths_for_candidates(
        &self,
        repo_root: &Path,
        candidates: &BTreeSet<FilePath>,
    ) -> io::Result<Vec<FilePath>> {
        let paths = self.expand_candidate_paths(repo_root, candidates)?;
        paths
            .into_iter()
            .filter_map(|path| match self.path_changed(repo_root, &path) {
                Ok(true) => Some(Ok(path)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn expand_candidate_paths(
        &self,
        repo_root: &Path,
        candidates: &BTreeSet<FilePath>,
    ) -> io::Result<BTreeSet<FilePath>> {
        let mut paths = BTreeSet::new();
        for candidate in candidates {
            insert_path_and_ancestors(candidate, &mut paths);
            self.insert_known_descendants(candidate, &mut paths);
            insert_current_descendants(repo_root, candidate, &mut paths)?;
        }
        Ok(paths)
    }

    fn insert_known_descendants(&self, path: &str, paths: &mut BTreeSet<FilePath>) {
        let prefix = format!("{path}/");
        paths.extend(
            self.files
                .keys()
                .chain(self.dirs.iter())
                .filter(|candidate| candidate.starts_with(&prefix))
                .cloned(),
        );
    }

    fn path_changed(&self, repo_root: &Path, path: &str) -> io::Result<bool> {
        Ok(
            self.files.get(path) != snapshot_file(&repo_root.join(path))?.as_ref()
                || self.dirs.contains(path) != path_is_dir(&repo_root.join(path)),
        )
    }

    fn rollback(
        &self,
        repo_root: &Path,
        backup: &RestoreBackup,
        paths: &[FilePath],
    ) -> io::Result<()> {
        for path in paths {
            if self.files.get(path) == snapshot_file(&repo_root.join(path))?.as_ref() {
                continue;
            }
            match self.files.get(path) {
                Some(_) => restore_file(&repo_root.join(path), backup.file(path)?)?,
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
                    && path_is_dir(&repo_root.join(path))
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
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug)]
struct RestoreBackup {
    files: BTreeMap<FilePath, Vec<u8>>,
}

impl RestoreBackup {
    fn file(&self, path: &str) -> io::Result<&[u8]> {
        self.files.get(path).map(Vec::as_slice).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("missing restore backup for {path}"),
            )
        })
    }
}

struct DirtyPathSet {
    paths: BTreeSet<FilePath>,
    fallback: bool,
}

struct DirtyTracker {
    _watcher: Option<RecommendedWatcher>,
    receiver: Option<mpsc::Receiver<notify::Result<Event>>>,
    fallback: bool,
}

impl DirtyTracker {
    fn start(repo_root: &Path) -> Self {
        if env::var_os("LANE_EXEC_DISABLE_WATCHER").is_some() {
            return Self::fallback();
        }

        let (sender, receiver) = mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |result| {
            let _ = sender.send(result);
        }) {
            Ok(watcher) => watcher,
            Err(_) => return Self::fallback(),
        };

        if watcher.watch(repo_root, RecursiveMode::Recursive).is_err() {
            return Self::fallback();
        }

        Self {
            _watcher: Some(watcher),
            receiver: Some(receiver),
            fallback: false,
        }
    }

    fn fallback() -> Self {
        Self {
            _watcher: None,
            receiver: None,
            fallback: true,
        }
    }

    fn finish(mut self, repo_root: &Path) -> DirtyPathSet {
        let Some(receiver) = self.receiver.take() else {
            return DirtyPathSet {
                paths: BTreeSet::new(),
                fallback: true,
            };
        };

        let mut paths = BTreeSet::new();
        let drain_start = Instant::now();
        loop {
            match receiver.recv_timeout(WATCHER_SETTLE_TIMEOUT) {
                Ok(Ok(event)) => insert_event_paths(repo_root, event, &mut paths),
                Ok(Err(_)) => self.fallback = true,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if drain_start.elapsed() >= WATCHER_MAX_DRAIN {
                self.fallback = true;
                break;
            }
        }
        if paths.is_empty() {
            self.fallback = true;
        }

        DirtyPathSet {
            paths,
            fallback: self.fallback,
        }
    }
}

fn collect_base_snapshot(
    repo_root: &Path,
    directory: &Path,
    files: &mut BTreeMap<FilePath, SnapshotFile>,
    dirs: &mut BTreeSet<FilePath>,
    mut backup: Option<&mut Vec<BackupTarget>>,
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
            collect_base_snapshot(repo_root, &path, files, dirs, backup.as_deref_mut())?;
        } else if file_type.is_file() {
            let label = relative.to_string_lossy().replace('\\', "/");
            if let Some(backup) = backup.as_deref_mut() {
                let Some(file) = snapshot_file(&path)? else {
                    continue;
                };
                backup.push(BackupTarget {
                    label,
                    path: path.clone(),
                    file,
                });
            } else {
                let Some(file) = snapshot_file(&path)? else {
                    continue;
                };
                files.insert(label, file);
            }
        }
    }
    Ok(())
}

fn remove_snapshot_path_and_descendants(
    path: &str,
    files: &mut BTreeMap<FilePath, SnapshotFile>,
    dirs: &mut BTreeSet<FilePath>,
) {
    let prefix = format!("{path}/");
    files.retain(|candidate, _| candidate != path && !candidate.starts_with(&prefix));
    dirs.retain(|candidate| candidate != path && !candidate.starts_with(&prefix));
}

#[derive(Clone, Debug)]
struct BackupTarget {
    label: FilePath,
    path: PathBuf,
    file: SnapshotFile,
}

struct BackupEntry {
    label: FilePath,
    file: SnapshotFile,
    bytes: Vec<u8>,
}

fn read_restore_backup(
    targets: Vec<BackupTarget>,
    files: &mut BTreeMap<FilePath, SnapshotFile>,
) -> io::Result<RestoreBackup> {
    let entries = read_backup_entries(&targets)?;
    let mut backup_files = BTreeMap::new();
    for entry in entries {
        files.insert(entry.label.clone(), entry.file);
        backup_files.insert(entry.label, entry.bytes);
    }
    Ok(RestoreBackup {
        files: backup_files,
    })
}

fn read_backup_entries(targets: &[BackupTarget]) -> io::Result<Vec<BackupEntry>> {
    let worker_count = backup_worker_count(targets.len());
    if worker_count <= 1 {
        return read_backup_chunk(targets);
    }

    let chunk_size = targets.len().div_ceil(worker_count);
    thread::scope(|scope| {
        let handles = targets
            .chunks(chunk_size)
            .map(|chunk| scope.spawn(move || read_backup_chunk(chunk)))
            .collect::<Vec<_>>();

        let mut entries = Vec::new();
        for handle in handles {
            entries.extend(handle.join().unwrap()?);
        }
        Ok(entries)
    })
}

fn backup_worker_count(file_count: usize) -> usize {
    if file_count < PARALLEL_BACKUP_MIN_FILES {
        return 1;
    }
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(PARALLEL_BACKUP_MAX_WORKERS)
        .min(file_count)
}

fn read_backup_chunk(targets: &[BackupTarget]) -> io::Result<Vec<BackupEntry>> {
    let mut entries = Vec::new();
    for target in targets {
        let Some(bytes) = read_backup_file(&target.path)? else {
            continue;
        };
        entries.push(BackupEntry {
            label: target.label.clone(),
            file: target.file.clone(),
            bytes,
        });
    }
    Ok(entries)
}

fn read_backup_file(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::IsADirectory => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(None),
        Err(error) => Err(error),
    }
}

fn should_skip_exec_guard_dir(relative: &Path) -> bool {
    relative.components().next().is_some_and(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        BASE_GUARD_IGNORED_DIRS.contains(&name.as_str())
    })
}

fn snapshot_file(path: &Path) -> io::Result<Option<SnapshotFile>> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(SnapshotFile::from_metadata(&metadata))),
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn path_is_dir(path: &Path) -> bool {
    path.is_dir()
}

fn insert_event_paths(repo_root: &Path, event: Event, paths: &mut BTreeSet<FilePath>) {
    for path in event.paths {
        let path = absolute_event_path(repo_root, path);
        let Ok(relative) = path.strip_prefix(repo_root) else {
            continue;
        };
        if relative.as_os_str().is_empty() || should_skip_exec_guard_dir(relative) {
            continue;
        }
        paths.insert(relative.to_string_lossy().replace('\\', "/"));
    }
}

fn absolute_event_path(repo_root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    }
}

fn insert_path_and_ancestors(path: &str, paths: &mut BTreeSet<FilePath>) {
    let mut current = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(part);
        paths.insert(current.clone());
    }
}

fn insert_current_descendants(
    repo_root: &Path,
    path: &str,
    paths: &mut BTreeSet<FilePath>,
) -> io::Result<()> {
    let directory = repo_root.join(path);
    if !directory.is_dir() {
        return Ok(());
    }
    collect_current_descendants(repo_root, &directory, paths)
}

fn collect_current_descendants(
    repo_root: &Path,
    directory: &Path,
    paths: &mut BTreeSet<FilePath>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(repo_root)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if relative.as_os_str().is_empty() || should_skip_exec_guard_dir(relative) {
            continue;
        }
        let label = relative.to_string_lossy().replace('\\', "/");
        paths.insert(label.clone());
        if entry.file_type()?.is_dir() {
            collect_current_descendants(repo_root, &path, paths)?;
        }
    }
    Ok(())
}

impl SnapshotFile {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        }
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
