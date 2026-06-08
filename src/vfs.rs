use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::storage::persist_bytes;
use crate::{FilePath, LaneError, LaneExecState, LaneOpDetail, LaneOpSummary, LaneRepo};

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
            if path.is_empty() && name == ".lane" {
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

    pub(crate) fn base_file(&self, path: &str) -> Result<Option<Vec<u8>>, LaneFsError> {
        let path = normalize_repo_path(path)?;
        self.worktree.read_file(&path).map_err(LaneFsError::Io)
    }

    pub(crate) fn changed_paths(&self, lane: &str) -> Result<Vec<FilePath>, LaneFsError> {
        self.repo
            .overlay_paths(lane)
            .map_err(LaneFsError::Lane)
            .map(|paths| paths.into_iter().map(str::to_owned).collect())
    }

    pub(crate) fn change_ops(
        &self,
        lane: &str,
        path: &str,
    ) -> Result<Vec<LaneOpSummary>, LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .change_ops(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)
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

    pub(crate) fn record_last_exec(
        &mut self,
        lane: &str,
        state: LaneExecState,
    ) -> Result<(), LaneFsError> {
        self.repo
            .record_last_exec(lane, state)
            .map_err(LaneFsError::Lane)
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

        self.apply_promoted_files(&promoted)?;
        self.repo = draft;
        Ok(())
    }

    pub(crate) fn resolve_op_file(
        &mut self,
        lane: &str,
        path: &str,
        op_id: &str,
        replacement: impl Into<Vec<u8>>,
    ) -> Result<(), LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        let mut draft = self.repo.clone();
        let promoted = draft
            .resolve_op_path(&path, lane, base.as_deref(), op_id, replacement)
            .map_err(LaneFsError::Lane)?;
        self.apply_promoted_files(&[(path, promoted)])?;
        self.repo = draft;
        Ok(())
    }

    fn apply_promoted_files(
        &mut self,
        promoted: &[(FilePath, Option<Vec<u8>>)],
    ) -> Result<(), LaneFsError> {
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
    let reserved_label = label.to_ascii_lowercase();
    if reserved_label == ".lane" || reserved_label.starts_with(".lane/") {
        return Err(LaneFsError::BadPath(
            "cannot project lane state files".to_owned(),
        ));
    }
    Ok(label)
}
