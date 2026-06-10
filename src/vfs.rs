use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::storage::persist_bytes;
use crate::{
    FilePath, LaneError, LaneOpDetail, LaneOpSummary, LaneRepo, is_git_metadata_path,
    is_lane_state_path,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirEntry {
    pub(crate) name: String,
    pub(crate) kind: DirEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirEntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaneFileChange {
    pub(crate) path: FilePath,
    pub(crate) status: LaneFileChangeStatus,
    pub(crate) base_size: Option<usize>,
    pub(crate) lane_size: Option<usize>,
    pub(crate) ops: Vec<LaneOpSummary>,
    pub(crate) base_bytes: Option<Vec<u8>>,
    pub(crate) lane_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneFileChangeStatus {
    Created,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileWorktree {
    root_path: PathBuf,
}

impl FileWorktree {
    pub(crate) fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: root_path.into(),
        }
    }

    fn file_path(&self, path: &str) -> PathBuf {
        self.root_path.join(path)
    }

    pub(crate) fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        let file_path = self.file_path(path);
        if file_path.is_dir() {
            return Ok(None);
        }
        match fs::read(&file_path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::IsADirectory => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied && file_path.is_dir() => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn write_file(&mut self, path: &str, bytes: &[u8]) -> io::Result<()> {
        let file_path = self.file_path(path);
        if file_path.is_dir() {
            fs::remove_dir_all(&file_path)?;
        }
        persist_bytes(&file_path, bytes)
    }

    pub(crate) fn remove_file(&mut self, path: &str) -> io::Result<()> {
        match fs::remove_file(self.file_path(path)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn transaction(&self, paths: &[FilePath]) -> io::Result<FileWorktreeTransaction> {
        let mut snapshot_paths = Vec::new();
        for path in paths {
            push_unique(&mut snapshot_paths, path.clone());
            for ancestor in ancestor_paths(path) {
                let file_path = self.file_path(&ancestor);
                if !file_path.exists() || file_path.is_file() {
                    push_unique(&mut snapshot_paths, ancestor);
                }
            }
        }
        snapshot_paths.sort_by(|left, right| {
            path_depth(right)
                .cmp(&path_depth(left))
                .then_with(|| right.cmp(left))
        });

        let mut snapshots = snapshot_paths
            .into_iter()
            .map(|path| self.snapshot_path(path))
            .collect::<io::Result<Vec<_>>>()?;
        let directory_paths = snapshots
            .iter()
            .filter_map(FileWorktreeSnapshot::directory_path)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        snapshots.retain(|snapshot| {
            !directory_paths.iter().any(|directory| {
                snapshot.path() != directory && is_descendant_path(snapshot.path(), directory)
            })
        });
        Ok(FileWorktreeTransaction {
            root_path: self.root_path.clone(),
            snapshots,
        })
    }

    fn snapshot_path(&self, path: FilePath) -> io::Result<FileWorktreeSnapshot> {
        let file_path = self.file_path(&path);
        if file_path.is_file() {
            return fs::read(file_path).map(|bytes| FileWorktreeSnapshot::File { path, bytes });
        }
        if file_path.is_dir() {
            let mut directories = Vec::new();
            let mut files = Vec::new();
            collect_directory_contents(&file_path, "", &mut directories, &mut files)?;
            directories.sort();
            files.sort_by(|(left, _), (right, _)| left.cmp(right));
            return Ok(FileWorktreeSnapshot::Directory {
                path,
                directories,
                files,
            });
        }
        Ok(FileWorktreeSnapshot::Missing { path })
    }

    pub(crate) fn list_dir(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        let directory = self.file_path(path);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error),
        };
        let mut dir_entries = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_empty()
                && (name.eq_ignore_ascii_case(".lane") || name.eq_ignore_ascii_case(".git"))
            {
                continue;
            }
            let file_type = entry.file_type()?;
            let kind = if file_type.is_dir() {
                DirEntryKind::Directory
            } else if file_type.is_file() {
                DirEntryKind::File
            } else {
                continue;
            };
            dir_entries.push(DirEntry { name, kind });
        }
        dir_entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(dir_entries)
    }
}

struct FileWorktreeTransaction {
    root_path: PathBuf,
    snapshots: Vec<FileWorktreeSnapshot>,
}

impl FileWorktreeTransaction {
    fn rollback(&self) -> io::Result<()> {
        for snapshot in &self.snapshots {
            snapshot.restore(&self.root_path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("failed to restore {}: {error}", snapshot.path()),
                )
            })?;
        }
        Ok(())
    }
}

enum FileWorktreeSnapshot {
    Missing {
        path: FilePath,
    },
    File {
        path: FilePath,
        bytes: Vec<u8>,
    },
    Directory {
        path: FilePath,
        directories: Vec<FilePath>,
        files: Vec<(FilePath, Vec<u8>)>,
    },
}

impl FileWorktreeSnapshot {
    fn path(&self) -> &str {
        match self {
            Self::Missing { path } | Self::File { path, .. } | Self::Directory { path, .. } => path,
        }
    }

    fn directory_path(&self) -> Option<&str> {
        match self {
            Self::Directory { path, .. } => Some(path),
            _ => None,
        }
    }

    fn restore(&self, root_path: &Path) -> io::Result<()> {
        match self {
            Self::Missing { path } => remove_path_if_exists(&root_path.join(path)),
            Self::File { path, bytes } => {
                remove_path_if_exists(&root_path.join(path))?;
                persist_bytes(&root_path.join(path), bytes)
            }
            Self::Directory {
                path,
                directories,
                files,
            } => {
                let directory = root_path.join(path);
                recreate_directory(&directory)?;
                for relative_path in directories {
                    fs::create_dir_all(directory.join(relative_path))?;
                }
                for (relative_path, bytes) in files {
                    persist_bytes(&directory.join(relative_path), bytes)?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LaneFs {
    repo: LaneRepo,
    worktree: FileWorktree,
}

impl LaneFs {
    pub(crate) fn new(repo: LaneRepo, worktree: FileWorktree) -> Self {
        Self { repo, worktree }
    }

    pub(crate) fn repo(&self) -> &LaneRepo {
        &self.repo
    }

    pub(crate) fn changed_paths(&self, lane: &str) -> Result<Vec<FilePath>, LaneFsError> {
        self.repo
            .overlay_paths(lane)
            .map_err(LaneFsError::Lane)
            .map(|paths| paths.into_iter().map(str::to_owned).collect())
    }

    pub(crate) fn change_for_path(
        &self,
        lane: &str,
        path: impl Into<String>,
    ) -> Result<Option<LaneFileChange>, LaneFsError> {
        let path = normalize_repo_path(&path.into())?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        let lane_bytes = self
            .repo
            .read_path(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)?;
        if base == lane_bytes {
            return Ok(None);
        }

        let status = match (&base, &lane_bytes) {
            (None, Some(_)) => LaneFileChangeStatus::Created,
            (Some(_), None) => LaneFileChangeStatus::Deleted,
            (Some(_), Some(_)) => LaneFileChangeStatus::Modified,
            (None, None) => return Ok(None),
        };
        let ops = self
            .repo
            .change_ops(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)?;

        Ok(Some(LaneFileChange {
            path,
            status,
            base_size: base.as_ref().map(Vec::len),
            lane_size: lane_bytes.as_ref().map(Vec::len),
            ops,
            base_bytes: base,
            lane_bytes,
        }))
    }

    pub(crate) fn op_detail(
        &self,
        lane: &str,
        path: &str,
        op_id: &str,
    ) -> Result<LaneOpDetail, LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .op_detail(&path, lane, base.as_deref(), op_id)
            .map_err(LaneFsError::Lane)
    }

    pub(crate) fn discard_lane(&mut self, lane: &str) -> bool {
        self.repo.discard_lane(lane)
    }

    pub(crate) fn create_lane(&mut self, lane: impl Into<String>) -> Result<bool, LaneFsError> {
        self.repo.create_lane(lane).map_err(LaneFsError::Lane)
    }

    pub(crate) fn read_file(&self, lane: &str, path: &str) -> Result<Option<Vec<u8>>, LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .read_path(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)
    }

    pub(crate) fn write_file(
        &mut self,
        lane: &str,
        path: &str,
        bytes: impl Into<Vec<u8>>,
    ) -> Result<(), LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .replace_path(&path, lane, base.as_deref(), Some(bytes.into()))
            .map_err(LaneFsError::Lane)
    }

    pub(crate) fn delete_file(&mut self, lane: &str, path: &str) -> Result<(), LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .delete_path(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)
    }

    pub(crate) fn list_dir(&self, lane: &str, path: &str) -> Result<Vec<DirEntry>, LaneFsError> {
        let directory = normalize_repo_dir(path)?;
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        let mut entries = BTreeMap::new();
        for entry in self
            .worktree
            .list_dir(&directory)
            .map_err(LaneFsError::Io)?
        {
            let child = child_path(&directory, &entry.name);
            match entry.kind {
                DirEntryKind::Directory => {
                    if self
                        .repo
                        .read_path(&child, lane, None)
                        .is_ok_and(|entry| entry.is_none())
                    {
                        entries.insert(entry.name, DirEntryKind::Directory);
                    }
                }
                DirEntryKind::File => {
                    if self.read_file(lane, &child)?.is_some() {
                        entries.insert(entry.name, DirEntryKind::File);
                    }
                }
            }
        }

        for path in self
            .repo
            .overlay_paths(lane)
            .map_err(LaneFsError::Lane)?
            .into_iter()
        {
            let Some(tail) = path.strip_prefix(&prefix) else {
                continue;
            };
            if tail.is_empty() || tail == path && !directory.is_empty() {
                continue;
            }
            if self.read_file(lane, path)?.is_none() {
                continue;
            }
            let (name, kind) = match tail.split_once('/') {
                Some((name, _)) => (name, DirEntryKind::Directory),
                None => (tail, DirEntryKind::File),
            };
            entries
                .entry(name.to_owned())
                .and_modify(|entry| {
                    if kind == DirEntryKind::Directory {
                        *entry = DirEntryKind::Directory;
                    }
                })
                .or_insert(kind);
        }

        Ok(entries
            .into_iter()
            .map(|(name, kind)| DirEntry { name, kind })
            .collect())
    }

    pub(crate) fn promote_ops_files(
        &mut self,
        lane: &str,
        path_ops: &[(FilePath, Vec<String>)],
        persist: impl FnOnce(&LaneRepo) -> Result<(), LaneFsError>,
    ) -> Result<(), LaneFsError> {
        let path_ops = path_ops
            .iter()
            .map(|(path, ops)| normalize_repo_path(path).map(|path| (path, ops)))
            .collect::<Result<Vec<_>, _>>()?;
        let bases = path_ops
            .iter()
            .map(|(path, _)| {
                self.worktree
                    .read_file(path)
                    .map(|base| (path.clone(), base))
                    .map_err(LaneFsError::Io)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;

        let mut draft = self.repo.clone();
        let mut promoted = Vec::new();
        for (path, ops) in path_ops {
            let base = bases.get(&path).and_then(Option::as_deref);
            let bytes = draft
                .promote_ops_path(&path, lane, base, ops)
                .map_err(LaneFsError::Lane)?;
            promoted.push((path, bytes));
        }

        self.commit_promoted_files(draft, &promoted, persist)
    }

    pub(crate) fn resolve_op_file(
        &mut self,
        lane: &str,
        path: &str,
        op_id: &str,
        replacement: impl Into<Vec<u8>>,
        persist: impl FnOnce(&LaneRepo) -> Result<(), LaneFsError>,
    ) -> Result<(), LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        let mut draft = self.repo.clone();
        let promoted = draft
            .resolve_op_path(&path, lane, base.as_deref(), op_id, replacement)
            .map_err(LaneFsError::Lane)?;
        self.commit_promoted_files(draft, &[(path, promoted)], persist)
    }

    fn commit_promoted_files(
        &mut self,
        draft: LaneRepo,
        promoted: &[(FilePath, Option<Vec<u8>>)],
        persist: impl FnOnce(&LaneRepo) -> Result<(), LaneFsError>,
    ) -> Result<(), LaneFsError> {
        let promoted_paths = promoted
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let transaction = self
            .worktree
            .transaction(&promoted_paths)
            .map_err(LaneFsError::Io)?;

        let old_repo = self.repo.clone();
        let result = (|| {
            let mut deletes = promoted
                .iter()
                .filter_map(|(path, bytes)| bytes.is_none().then_some(path.as_str()))
                .collect::<Vec<_>>();
            deletes.sort_by(|left, right| {
                path_depth(right)
                    .cmp(&path_depth(left))
                    .then_with(|| right.cmp(left))
            });
            for path in deletes {
                self.worktree.remove_file(path).map_err(LaneFsError::Io)?;
            }

            let mut writes = promoted
                .iter()
                .filter_map(|(path, bytes)| bytes.as_deref().map(|bytes| (path.as_str(), bytes)))
                .collect::<Vec<_>>();
            writes.sort_by(|(left, _), (right, _)| {
                path_depth(left)
                    .cmp(&path_depth(right))
                    .then_with(|| left.cmp(right))
            });
            for (path, bytes) in writes {
                self.worktree
                    .write_file(path, bytes)
                    .map_err(LaneFsError::Io)?;
            }
            self.repo = draft;
            persist(&self.repo)?;
            Ok(())
        })();

        if let Err(error) = result {
            self.repo = old_repo;
            if let Err(rollback_error) = transaction.rollback() {
                return Err(LaneFsError::Io(io::Error::other(format!(
                    "failed to commit promoted files: {error}; rollback failed: {rollback_error}"
                ))));
            }
            return Err(error);
        }

        Ok(())
    }
}

fn child_path(parent: &str, child: &str) -> FilePath {
    if parent.is_empty() {
        child.to_owned()
    } else {
        format!("{parent}/{child}")
    }
}

fn path_depth(path: &str) -> usize {
    path.split('/').filter(|part| !part.is_empty()).count()
}

fn push_unique(paths: &mut Vec<FilePath>, path: FilePath) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

fn ancestor_paths(path: &str) -> Vec<FilePath> {
    let mut ancestors = Vec::new();
    let mut parts = path.split('/').collect::<Vec<_>>();
    while parts.len() > 1 {
        parts.pop();
        ancestors.push(parts.join("/"));
    }
    ancestors
}

fn is_descendant_path(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor)
        .is_some_and(|tail| tail.starts_with('/'))
}

fn collect_directory_contents(
    directory: &Path,
    relative_prefix: &str,
    directories: &mut Vec<FilePath>,
    files: &mut Vec<(FilePath, Vec<u8>)>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let relative_path = child_path(relative_prefix, &name);
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            directories.push(relative_path.clone());
            collect_directory_contents(&path, &relative_path, directories, files)?;
        } else if file_type.is_file() {
            files.push((relative_path, fs::read(path)?));
        }
    }
    Ok(())
}

fn remove_path_if_exists(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn recreate_directory(path: &Path) -> io::Result<()> {
    for attempt in 0..3 {
        remove_path_if_exists(path)?;
        match fs::create_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(error)
                if attempt < 2
                    && matches!(
                        error.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                    ) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("retry loop always exits via return on the final attempt")
}

#[derive(Debug)]
pub(crate) enum LaneFsError {
    BadPath(String),
    Io(io::Error),
    Lane(LaneError),
}

impl fmt::Display for LaneFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadPath(message) => write!(f, "{message}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Lane(error) => write!(f, "{error:?}"),
        }
    }
}

impl std::error::Error for LaneFsError {}

fn normalize_repo_path(path: &str) -> Result<String, LaneFsError> {
    let label = normalize_repo_label(path)?;
    if label.is_empty() {
        Err(LaneFsError::BadPath("missing path".to_owned()))
    } else {
        Ok(label)
    }
}

fn normalize_repo_dir(path: &str) -> Result<String, LaneFsError> {
    normalize_repo_label(path)
}

fn normalize_repo_label(path: &str) -> Result<String, LaneFsError> {
    if path.trim().is_empty() || path == "." {
        return Ok(String::new());
    }

    let raw_path = Path::new(path);
    if raw_path.is_absolute() {
        return Err(LaneFsError::BadPath(
            "path must be repo-relative".to_owned(),
        ));
    }

    let mut parts = Vec::new();
    for component in raw_path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            _ => {
                return Err(LaneFsError::BadPath(
                    "path must stay inside the repo".to_owned(),
                ));
            }
        }
    }

    let label = parts.join("/");
    if is_lane_state_path(&label) {
        return Err(LaneFsError::BadPath(
            "cannot project lane state files".to_owned(),
        ));
    }
    if is_git_metadata_path(&label) {
        return Err(LaneFsError::BadPath(
            "cannot project git metadata files".to_owned(),
        ));
    }
    Ok(label)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn promotion_rolls_back_worktree_when_persist_fails() {
        let root = temp_dir();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/example.txt"), b"base").unwrap();

        let mut repo = LaneRepo::new();
        repo.create_lane("agent-a").unwrap();
        repo.replace_path(
            "src/example.txt",
            "agent-a",
            Some(b"base"),
            Some(b"lane".to_vec()),
        )
        .unwrap();
        let op_ids = repo
            .change_ops("src/example.txt", "agent-a", Some(b"base"))
            .unwrap()
            .into_iter()
            .map(|op| op.op_id)
            .collect::<Vec<_>>();
        let mut fs = LaneFs::new(repo, FileWorktree::new(&root));

        let result =
            fs.promote_ops_files("agent-a", &[("src/example.txt".to_owned(), op_ids)], |_| {
                Err(LaneFsError::Io(io::Error::other(
                    "synthetic persist failure",
                )))
            });

        assert!(result.is_err());
        assert_eq!(
            std::fs::read(root.join("src/example.txt")).unwrap(),
            b"base"
        );
        assert_eq!(
            fs.read_file("agent-a", "src/example.txt").unwrap(),
            Some(b"lane".to_vec())
        );
        assert_eq!(
            fs.change_for_path("agent-a", "src/example.txt")
                .unwrap()
                .unwrap()
                .status,
            LaneFileChangeStatus::Modified
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn path_normalization_rejects_repo_metadata_case_insensitively() {
        assert!(matches!(
            normalize_repo_path(".LANE/repo.json"),
            Err(LaneFsError::BadPath(message)) if message == "cannot project lane state files"
        ));
        assert!(matches!(
            normalize_repo_path(".GIT/config"),
            Err(LaneFsError::BadPath(message)) if message == "cannot project git metadata files"
        ));
        assert_eq!(
            normalize_repo_path("src/.git/config").unwrap(),
            "src/.git/config"
        );
    }

    fn temp_dir() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lane-vfs-test-{}-{timestamp}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
