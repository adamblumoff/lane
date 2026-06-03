use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::storage::persist_bytes;
use crate::{FilePath, LaneError, LaneRepo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: DirEntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirEntryKind {
    Directory,
    File,
}

pub trait Worktree {
    fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>>;
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> io::Result<()>;
    fn remove_file(&mut self, path: &str) -> io::Result<()>;
    fn list_files(&self) -> io::Result<Vec<FilePath>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryWorktree {
    files: BTreeMap<FilePath, Vec<u8>>,
}

impl MemoryWorktree {
    pub fn new(files: impl IntoIterator<Item = (impl Into<FilePath>, impl Into<Vec<u8>>)>) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|(path, bytes)| (path.into(), bytes.into()))
                .collect(),
        }
    }

    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }
}

impl Worktree for MemoryWorktree {
    fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        Ok(self.files.get(path).cloned())
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> io::Result<()> {
        self.files.insert(path.to_owned(), bytes.to_vec());
        Ok(())
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        self.files.remove(path);
        Ok(())
    }

    fn list_files(&self) -> io::Result<Vec<FilePath>> {
        Ok(self.files.keys().cloned().collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWorktree {
    root_path: PathBuf,
}

impl FileWorktree {
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: root_path.into(),
        }
    }

    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    fn file_path(&self, path: &str) -> PathBuf {
        self.root_path.join(path)
    }
}

impl Worktree for FileWorktree {
    fn read_file(&self, path: &str) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.file_path(path)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> io::Result<()> {
        persist_bytes(&self.file_path(path), bytes)
    }

    fn remove_file(&mut self, path: &str) -> io::Result<()> {
        match fs::remove_file(self.file_path(path)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn list_files(&self) -> io::Result<Vec<FilePath>> {
        let mut files = Vec::new();
        collect_files(&self.root_path, &self.root_path, &mut files)?;
        files.sort();
        Ok(files)
    }
}

#[derive(Clone, Debug)]
pub struct LaneFs<W> {
    repo: LaneRepo,
    worktree: W,
}

fn collect_files(root_path: &Path, directory: &Path, files: &mut Vec<FilePath>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root_path)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        if relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == ".lane")
        {
            continue;
        }

        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_files(root_path, &path, files)?;
        } else if file_type.is_file() {
            files.push(relative.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

impl<W: Worktree> LaneFs<W> {
    pub fn new(repo: LaneRepo, worktree: W) -> Self {
        Self { repo, worktree }
    }

    pub fn repo(&self) -> &LaneRepo {
        &self.repo
    }

    pub fn worktree(&self) -> &W {
        &self.worktree
    }

    pub fn base_file(&self, path: &str) -> Result<Option<Vec<u8>>, LaneFsError> {
        let path = normalize_repo_path(path)?;
        self.worktree.read_file(&path).map_err(LaneFsError::Io)
    }

    pub fn changed_paths(&self, lane: &str) -> Result<Vec<FilePath>, LaneFsError> {
        self.repo
            .overlay_paths(lane)
            .map_err(LaneFsError::Lane)
            .map(|paths| paths.into_iter().map(str::to_owned).collect())
    }

    pub fn discard_lane(&mut self, lane: &str) -> bool {
        self.repo.discard_lane(lane)
    }

    pub fn into_parts(self) -> (LaneRepo, W) {
        (self.repo, self.worktree)
    }

    pub fn create_lane(&mut self, lane: impl Into<String>) -> Result<bool, LaneFsError> {
        self.repo.create_lane(lane).map_err(LaneFsError::Lane)
    }

    pub fn read_file(&self, lane: &str, path: &str) -> Result<Option<Vec<u8>>, LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .read_path(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)
    }

    pub fn write_file(
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

    pub fn delete_file(&mut self, lane: &str, path: &str) -> Result<(), LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        self.repo
            .delete_path(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)
    }

    pub fn rename_file(&mut self, lane: &str, from: &str, to: &str) -> Result<(), LaneFsError> {
        let from = normalize_repo_path(from)?;
        let to = normalize_repo_path(to)?;
        let bytes = self
            .read_file(lane, &from)?
            .ok_or_else(|| LaneFsError::FileMissing { path: from.clone() })?;
        let from_base = self.worktree.read_file(&from).map_err(LaneFsError::Io)?;
        let to_base = self.worktree.read_file(&to).map_err(LaneFsError::Io)?;

        let mut draft = self.repo.clone();
        draft
            .replace_path(&to, lane, to_base.as_deref(), Some(bytes))
            .map_err(LaneFsError::Lane)?;
        draft
            .delete_path(&from, lane, from_base.as_deref())
            .map_err(LaneFsError::Lane)?;
        self.repo = draft;
        Ok(())
    }

    pub fn list_dir(&self, lane: &str, path: &str) -> Result<Vec<DirEntry>, LaneFsError> {
        let directory = normalize_repo_dir(path)?;
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        let mut paths = self.worktree.list_files().map_err(LaneFsError::Io)?;
        paths.extend(
            self.repo
                .overlay_paths(lane)
                .map_err(LaneFsError::Lane)?
                .into_iter()
                .map(str::to_owned),
        );

        let mut entries = BTreeMap::new();
        for path in paths.into_iter().collect::<BTreeSet<_>>() {
            let Some(tail) = path.strip_prefix(&prefix) else {
                continue;
            };
            if tail.is_empty() || tail == path && !directory.is_empty() {
                continue;
            }
            if self.read_file(lane, &path)?.is_none() {
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

    pub fn promote_file(&mut self, lane: &str, path: &str) -> Result<Option<Vec<u8>>, LaneFsError> {
        let path = normalize_repo_path(path)?;
        let base = self.worktree.read_file(&path).map_err(LaneFsError::Io)?;
        let mut draft = self.repo.clone();
        let promoted = draft
            .promote_path(&path, lane, base.as_deref())
            .map_err(LaneFsError::Lane)?;
        match promoted.as_deref() {
            Some(bytes) => self
                .worktree
                .write_file(&path, bytes)
                .map_err(LaneFsError::Io)?,
            None => self.worktree.remove_file(&path).map_err(LaneFsError::Io)?,
        }
        self.repo = draft;
        Ok(promoted)
    }

    pub fn promote_lane(&mut self, lane: &str) -> Result<Vec<FilePath>, LaneFsError> {
        let paths = self
            .repo
            .overlay_paths(lane)
            .map_err(LaneFsError::Lane)?
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let bases = paths
            .iter()
            .map(|path| {
                self.worktree
                    .read_file(path)
                    .map(|base| (path.clone(), base))
                    .map_err(LaneFsError::Io)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut draft = self.repo.clone();
        let promoted = draft.promote_lane(lane, bases).map_err(LaneFsError::Lane)?;
        for file in &promoted {
            match file.bytes.as_deref() {
                Some(bytes) => self
                    .worktree
                    .write_file(&file.path, bytes)
                    .map_err(LaneFsError::Io)?,
                None => self
                    .worktree
                    .remove_file(&file.path)
                    .map_err(LaneFsError::Io)?,
            }
        }
        self.repo = draft;
        Ok(promoted.into_iter().map(|file| file.path).collect())
    }
}

#[derive(Debug)]
pub enum LaneFsError {
    BadPath(String),
    FileMissing { path: FilePath },
    Io(io::Error),
    Lane(LaneError),
}

impl fmt::Display for LaneFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadPath(message) => write!(f, "{message}"),
            Self::FileMissing { path } => write!(f, "file missing in lane view: {path}"),
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
