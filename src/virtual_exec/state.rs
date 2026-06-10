use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows_sys::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_INVALID_PARAMETER, STATUS_NO_SUCH_FILE, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND,
};
use winfsp_wrs::{FileInfo, SecurityDescriptor, U16CStr, WriteMode};

use crate::storage::{acquire_repo_lock, load_repo, persist_last_exec, persist_repo};
use crate::vfs::{DirEntryKind, FileWorktree, LaneFs};
use crate::{FilePath, LaneExecState};

use super::nodes::{
    VirtualNode, change_for_path, child_path, dir_info, file_info, status_from_lane_fs_error,
};
use super::support::elapsed_ms;
use super::types::{VirtualChangeOutput, VirtualExecError, VirtualFsMetrics};

pub(super) struct VirtualLaneState {
    repo_root: PathBuf,
    storage_path: PathBuf,
    lane: String,
    fs: Mutex<LaneFs>,
    dirty: Mutex<BTreeMap<FilePath, DirtyEntry>>,
    versions: Mutex<BTreeMap<FilePath, u64>>,
    next_version: AtomicU64,
    pub(super) security: SecurityDescriptor,
    metrics: Arc<VirtualFsMetrics>,
}

#[derive(Clone, Debug)]
enum DirtyEntry {
    File(Vec<u8>),
    Directory,
    Deleted,
}

impl VirtualLaneState {
    pub(super) fn new(
        repo_root: &Path,
        storage_path: &Path,
        lane: &str,
        fs: LaneFs,
        security: SecurityDescriptor,
        metrics: Arc<VirtualFsMetrics>,
    ) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            storage_path: storage_path.to_path_buf(),
            lane: lane.to_owned(),
            fs: Mutex::new(fs),
            dirty: Mutex::new(BTreeMap::new()),
            versions: Mutex::new(BTreeMap::new()),
            next_version: AtomicU64::new(1),
            security,
            metrics,
        }
    }

    pub(super) fn node_for_name(&self, file_name: &U16CStr) -> Result<VirtualNode, i32> {
        let path = super::nodes::path_from_winfsp(file_name)?;
        self.node_for_path(&path)?
            .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    pub(super) fn node_for_path(&self, path: &str) -> Result<Option<VirtualNode>, i32> {
        if path.is_empty() {
            return Ok(Some(VirtualNode::Directory {
                path: String::new(),
            }));
        }

        if let Some(entry) = self.dirty_entry(path)? {
            return Ok(match entry {
                DirtyEntry::File(bytes) => Some(VirtualNode::File {
                    path: path.to_owned(),
                    len: bytes.len() as u64,
                }),
                DirtyEntry::Directory => Some(VirtualNode::Directory {
                    path: path.to_owned(),
                }),
                DirtyEntry::Deleted => None,
            });
        }
        if self.dirty_ancestor_hides(path)? {
            return Ok(None);
        }

        self.with_fs_read(|fs| {
            let overlay_paths = fs
                .changed_paths(&self.lane)
                .map_err(status_from_lane_fs_error)?;
            let has_overlay = overlay_paths.iter().any(|overlay| overlay == path);
            if let Some(bytes) = fs
                .read_file(&self.lane, path)
                .map_err(status_from_lane_fs_error)?
            {
                return Ok(Some(VirtualNode::File {
                    path: path.to_owned(),
                    len: bytes.len() as u64,
                }));
            }
            if has_overlay {
                return Ok(None);
            }
            if self.repo_root.join(path).is_dir()
                || self.path_has_visible_children(fs, path)?
                || self.dirty_has_visible_children(path)?
            {
                return Ok(Some(VirtualNode::Directory {
                    path: path.to_owned(),
                }));
            }
            Ok(None)
        })
    }

    fn path_has_visible_children(&self, fs: &LaneFs, path: &str) -> Result<bool, i32> {
        fs.list_dir(&self.lane, path)
            .map(|entries| !entries.is_empty())
            .map_err(status_from_lane_fs_error)
    }

    pub(super) fn read_file(&self, path: &str) -> Result<Vec<u8>, i32> {
        if let Some(entry) = self.dirty_entry(path)? {
            return match entry {
                DirtyEntry::File(bytes) => Ok(bytes),
                DirtyEntry::Directory | DirtyEntry::Deleted => Err(STATUS_NO_SUCH_FILE),
            };
        }
        if self.dirty_ancestor_hides(path)? {
            return Err(STATUS_NO_SUCH_FILE);
        }

        self.with_fs_read(|fs| {
            fs.read_file(&self.lane, path)
                .map_err(status_from_lane_fs_error)?
                .ok_or(STATUS_NO_SUCH_FILE)
        })
    }

    pub(super) fn write_file(&self, path: &str, bytes: Vec<u8>) -> Result<u64, i32> {
        self.set_dirty_entry(path, DirtyEntry::File(bytes))
    }

    pub(super) fn write_file_range(
        &self,
        path: &str,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, u64), i32> {
        let mut bytes = self.read_file_for_write(path)?;
        let offset = match mode {
            WriteMode::Normal { offset } | WriteMode::ConstrainedIO { offset } => {
                usize::try_from(offset).map_err(|_| STATUS_INVALID_PARAMETER)?
            }
            WriteMode::WriteToEOF => bytes.len(),
        };
        if offset > bytes.len() {
            bytes.resize(offset, 0);
        }
        if matches!(mode, WriteMode::ConstrainedIO { .. }) {
            let writable = buffer.len().min(bytes.len().saturating_sub(offset));
            bytes[offset..offset + writable].copy_from_slice(&buffer[..writable]);
        } else {
            let end = offset
                .checked_add(buffer.len())
                .ok_or(STATUS_INVALID_PARAMETER)?;
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[offset..end].copy_from_slice(buffer);
        }
        let len = bytes.len();
        let version = self.set_dirty_entry(path, DirtyEntry::File(bytes))?;
        Ok((len, version))
    }

    pub(super) fn resize_file(&self, path: &str, size: usize) -> Result<(usize, u64), i32> {
        let mut bytes = self.read_file_for_write(path)?;
        bytes.resize(size, 0);
        let version = self.set_dirty_entry(path, DirtyEntry::File(bytes))?;
        Ok((size, version))
    }

    pub(super) fn delete_file(&self, path: &str) -> Result<(), i32> {
        self.set_dirty_entry(path, DirtyEntry::Deleted).map(|_| ())
    }

    pub(super) fn rename_file(
        &self,
        from: &str,
        to: &str,
        replace_if_exists: bool,
    ) -> Result<u64, i32> {
        if from == to {
            return self.path_version(from);
        }
        if from.eq_ignore_ascii_case(to) {
            let bytes = self.read_file(from)?;
            return self.set_dirty_entry(to, DirtyEntry::File(bytes));
        }
        if self.node_for_path(to)?.is_some() && !replace_if_exists {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        if self.node_for_path(to)?.is_some_and(|node| node.is_dir()) {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }

        let bytes = self.read_file(from)?;
        self.set_dirty_entry(to, DirtyEntry::File(bytes))?;
        self.set_dirty_entry(from, DirtyEntry::Deleted)
    }

    pub(super) fn rename_dir(
        &self,
        from: &str,
        to: &str,
        replace_if_exists: bool,
    ) -> Result<u64, i32> {
        if from == to {
            return self.path_version(from);
        }
        if to.starts_with(&child_prefix(from)) {
            return Err(STATUS_ACCESS_DENIED);
        }
        if self.node_for_path(to)?.is_some() {
            if !replace_if_exists {
                return Err(STATUS_OBJECT_NAME_COLLISION);
            }
            if self.dir_has_children(to)? {
                return Err(STATUS_DIRECTORY_NOT_EMPTY);
            }
        }

        let mut files = Vec::new();
        self.collect_state_visible_files(from, &mut files)?;
        for path in files {
            let target = child_path(to, path.strip_prefix(&child_prefix(from)).unwrap_or(&path));
            let bytes = self.read_file(&path)?;
            self.set_dirty_entry(&target, DirtyEntry::File(bytes))?;
            self.set_dirty_entry(&path, DirtyEntry::Deleted)?;
        }
        self.set_dirty_entry(to, DirtyEntry::Directory)?;
        self.set_dirty_entry(from, DirtyEntry::Deleted)
    }

    pub(super) fn create_dir(&self, path: &str) -> Result<u64, i32> {
        self.set_dirty_entry(path, DirtyEntry::Directory)
    }

    pub(super) fn delete_file_if_current(&self, path: &str, version: u64) -> Result<(), i32> {
        if self.path_version(path)? == version {
            self.delete_file(path)?;
        }
        Ok(())
    }

    pub(super) fn delete_dir_if_current(&self, path: &str, version: u64) -> Result<(), i32> {
        if self.path_version(path)? == version {
            self.delete_dir(path)?;
        }
        Ok(())
    }

    pub(super) fn delete_dir(&self, path: &str) -> Result<(), i32> {
        let mut deleted = Vec::new();
        self.with_fs_read(|fs| collect_visible_files(fs, &self.lane, path, &mut deleted))?;
        let prefix = child_prefix(path);
        {
            let dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
            deleted.extend(dirty.iter().filter_map(|(dirty_path, entry)| {
                matches!(entry, DirtyEntry::File(_) | DirtyEntry::Directory)
                    .then_some(dirty_path)
                    .filter(|dirty_path| dirty_path.starts_with(&prefix))
                    .cloned()
            }));
        }

        let mut dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        for deleted_path in deleted {
            self.set_dirty_entry_locked(&mut dirty, deleted_path, DirtyEntry::Deleted)?;
        }
        self.set_dirty_entry_locked(&mut dirty, path.to_owned(), DirtyEntry::Deleted)?;
        Ok(())
    }

    fn read_file_for_write(&self, path: &str) -> Result<Vec<u8>, i32> {
        match self.read_file(path) {
            Ok(bytes) => Ok(bytes),
            Err(STATUS_NO_SUCH_FILE | STATUS_OBJECT_NAME_NOT_FOUND) => Ok(Vec::new()),
            Err(status) => Err(status),
        }
    }

    fn dir_has_children(&self, path: &str) -> Result<bool, i32> {
        self.dir_entries(path)
            .map(|entries| entries.iter().any(|(name, _)| name != "." && name != ".."))
    }

    fn collect_state_visible_files(
        &self,
        path: &str,
        files: &mut Vec<FilePath>,
    ) -> Result<(), i32> {
        for (name, _) in self.dir_entries(path)? {
            if name == "." || name == ".." {
                continue;
            }
            let child = child_path(path, &name);
            match self.node_for_path(&child)? {
                Some(node) if node.is_dir() => self.collect_state_visible_files(&child, files)?,
                Some(_) => files.push(child),
                None => {}
            }
        }
        Ok(())
    }

    pub(super) fn dir_entries(&self, path: &str) -> Result<Vec<(String, FileInfo)>, i32> {
        if self.dirty_ancestor_hides(path)? {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }

        self.with_fs_read(|fs| {
            let mut entries: BTreeMap<String, FileInfo> = vec![
                (".".to_owned(), dir_info(path)),
                ("..".to_owned(), dir_info("")),
            ]
            .into_iter()
            .collect();
            for entry in fs
                .list_dir(&self.lane, path)
                .map_err(status_from_lane_fs_error)?
            {
                let child = child_path(path, &entry.name);
                let info = match entry.kind {
                    DirEntryKind::Directory => dir_info(&child),
                    DirEntryKind::File => {
                        let len = fs
                            .read_file(&self.lane, &child)
                            .map_err(status_from_lane_fs_error)?
                            .map(|bytes| bytes.len() as u64)
                            .unwrap_or(0);
                        file_info(&child, len)
                    }
                };
                entries.insert(entry.name, info);
            }
            for (dirty_path, dirty) in self.dirty_entries()? {
                let Some((name, kind)) = dir_entry_for_dirty_path(path, &dirty_path) else {
                    continue;
                };
                match (kind, dirty) {
                    (DirEntryKind::Directory, DirtyEntry::File(_) | DirtyEntry::Directory) => {
                        entries.insert(name.clone(), dir_info(&child_path(path, &name)));
                    }
                    (DirEntryKind::Directory, DirtyEntry::Deleted) => {}
                    (DirEntryKind::File, DirtyEntry::File(bytes)) => {
                        entries.insert(name, file_info(&dirty_path, bytes.len() as u64));
                    }
                    (DirEntryKind::File, DirtyEntry::Directory) => {
                        entries.insert(name, dir_info(&dirty_path));
                    }
                    (DirEntryKind::File, DirtyEntry::Deleted) => {
                        entries.remove(&name);
                    }
                }
            }
            Ok(entries.into_iter().collect())
        })
    }

    fn with_fs_read<T>(&self, operation: impl FnOnce(&LaneFs) -> Result<T, i32>) -> Result<T, i32> {
        let fs = self.fs.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        operation(&fs)
    }

    pub(super) fn flush(&self) -> Result<(), VirtualExecError> {
        let dirty = self
            .dirty
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane dirty map lock poisoned"))?
            .clone();
        if dirty.is_empty() {
            return Ok(());
        }
        let versions = self
            .versions
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane version map lock poisoned"))?
            .clone();

        with_lane_fs_write(
            &self.repo_root,
            &self.storage_path,
            &self.metrics,
            |latest| {
                latest
                    .create_lane(&self.lane)
                    .map_err(status_from_lane_fs_error)?;
                let dirty = canonicalize_dirty_entries(latest, &self.lane, &dirty, &versions)?;
                apply_dirty_entries(
                    latest,
                    &self.lane,
                    dirty.iter().map(|(path, entry)| (path.as_str(), entry)),
                )?;
                Ok(())
            },
        )
    }

    pub(super) fn collect_changes(&self) -> Result<Vec<VirtualChangeOutput>, VirtualExecError> {
        let dirty = self
            .dirty
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane dirty map lock poisoned"))?
            .clone();
        let versions = self
            .versions
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane version map lock poisoned"))?
            .clone();
        let fs = self
            .fs
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane session lock poisoned"))?;
        let mut draft = LaneFs::new(fs.repo().clone(), FileWorktree::new(&self.repo_root));
        let dirty = canonicalize_dirty_entries(&draft, &self.lane, &dirty, &versions).map_err(
            |status| VirtualExecError::from_status("collect dirty virtual changes", status),
        )?;
        apply_dirty_entries(
            &mut draft,
            &self.lane,
            dirty.iter().map(|(path, entry)| (path.as_str(), entry)),
        )
        .map_err(|status| VirtualExecError::from_status("collect dirty virtual changes", status))?;
        draft
            .changed_paths(&self.lane)
            .map_err(status_from_lane_fs_error)
            .and_then(|paths| {
                paths
                    .into_iter()
                    .map(|path| change_for_path(&draft, &self.lane, path))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|changes| changes.into_iter().flatten().collect())
            })
            .map_err(|status| VirtualExecError::from_status("collect projected lane paths", status))
    }

    pub(super) fn record_last_exec(
        &self,
        exec_state: LaneExecState,
    ) -> Result<(), VirtualExecError> {
        let wait_start = Instant::now();
        let lock = acquire_repo_lock(&self.storage_path).map_err(|error| {
            VirtualExecError::message(format!(
                "failed to acquire lane storage lock {}: {error}",
                self.storage_path.display()
            ))
        })?;
        let wait_ms = elapsed_ms(wait_start);
        let held_start = Instant::now();
        persist_last_exec(&self.storage_path, &self.lane, &exec_state).map_err(|error| {
            VirtualExecError::message(format!(
                "failed to persist last_exec metadata {}: {error}",
                self.storage_path.display()
            ))
        })?;
        let held_ms = elapsed_ms(held_start);
        drop(lock);
        self.metrics.record_write(wait_ms, held_ms);
        Ok(())
    }

    pub(super) fn projected_paths(&self) -> Result<Vec<FilePath>, VirtualExecError> {
        self.fs
            .lock()
            .map_err(|_| VirtualExecError::message("virtual lane session lock poisoned"))?
            .changed_paths(&self.lane)
            .map_err(status_from_lane_fs_error)
            .map_err(|status| VirtualExecError::from_status("collect projected lane paths", status))
    }

    pub(super) fn worker_changed_paths(&self) -> Result<Vec<FilePath>, VirtualExecError> {
        self.canonical_dirty_entries()
            .map(|dirty| dirty.into_iter().map(|(path, _)| path).collect())
            .map_err(|status| VirtualExecError::from_status("collect worker changed paths", status))
    }

    fn dirty_entry(&self, path: &str) -> Result<Option<DirtyEntry>, i32> {
        let dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        let versions = self.versions.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        Ok(newest_dirty_entry_for_path(&dirty, &versions, path).cloned())
    }

    fn dirty_entries(&self) -> Result<Vec<(FilePath, DirtyEntry)>, i32> {
        self.dirty
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|dirty| {
                dirty
                    .iter()
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect()
            })
    }

    fn dirty_has_visible_children(&self, path: &str) -> Result<bool, i32> {
        self.dirty
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|dirty| {
                dirty.iter().any(|(dirty_path, entry)| {
                    is_descendant_path(dirty_path, path)
                        && matches!(entry, DirtyEntry::File(_) | DirtyEntry::Directory)
                })
            })
    }

    fn dirty_ancestor_hides(&self, path: &str) -> Result<bool, i32> {
        let dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        let versions = self.versions.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        Ok(dirty_ancestor_hides(&dirty, &versions, path))
    }

    fn canonical_dirty_entries(&self) -> Result<Vec<(FilePath, DirtyEntry)>, i32> {
        let dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?.clone();
        let versions = self
            .versions
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)?
            .clone();
        self.with_fs_read(|fs| canonicalize_dirty_entries(fs, &self.lane, &dirty, &versions))
    }

    pub(super) fn path_version(&self, path: &str) -> Result<u64, i32> {
        self.versions
            .lock()
            .map_err(|_| STATUS_ACCESS_DENIED)
            .map(|versions| versions.get(path).copied().unwrap_or(0))
    }

    fn set_dirty_entry(&self, path: &str, entry: DirtyEntry) -> Result<u64, i32> {
        let mut dirty = self.dirty.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        self.set_dirty_entry_locked(&mut dirty, path.to_owned(), entry)
    }

    fn set_dirty_entry_locked(
        &self,
        dirty: &mut BTreeMap<FilePath, DirtyEntry>,
        path: FilePath,
        entry: DirtyEntry,
    ) -> Result<u64, i32> {
        let version = self.next_version.fetch_add(1, Ordering::Relaxed);
        let mut versions = self.versions.lock().map_err(|_| STATUS_ACCESS_DENIED)?;
        dirty.insert(path.clone(), entry);
        versions.insert(path, version);
        Ok(version)
    }
}

fn apply_dirty_entries<'a>(
    fs: &mut LaneFs,
    lane: &str,
    dirty: impl IntoIterator<Item = (&'a str, &'a DirtyEntry)>,
) -> Result<(), i32> {
    fs.create_lane(lane).map_err(status_from_lane_fs_error)?;
    for (path, entry) in dirty {
        match entry {
            DirtyEntry::File(bytes) => fs
                .write_file(lane, path, bytes.as_slice())
                .map_err(status_from_lane_fs_error)?,
            DirtyEntry::Directory => {
                if fs
                    .read_file(lane, path)
                    .map_err(status_from_lane_fs_error)?
                    .is_some()
                {
                    fs.delete_file(lane, path)
                        .map_err(status_from_lane_fs_error)?;
                }
            }
            DirtyEntry::Deleted => fs
                .delete_file(lane, path)
                .map_err(status_from_lane_fs_error)?,
        }
    }
    Ok(())
}

fn canonicalize_dirty_entries(
    fs: &LaneFs,
    lane: &str,
    dirty: &BTreeMap<FilePath, DirtyEntry>,
    versions: &BTreeMap<FilePath, u64>,
) -> Result<Vec<(FilePath, DirtyEntry)>, i32> {
    let mut merged: BTreeMap<String, (u64, FilePath, DirtyEntry)> = BTreeMap::new();
    for (path, entry) in dirty {
        let canonical_path = canonicalize_visible_path(fs, lane, path)?;
        let canonical_key = canonical_path.to_ascii_lowercase();
        let version = versions.get(path).copied().unwrap_or(0);
        let replace = match merged.get(&canonical_key) {
            Some((existing_version, _, _)) => version >= *existing_version,
            None => true,
        };
        if replace {
            merged.insert(canonical_key, (version, canonical_path, entry.clone()));
        }
    }
    Ok(merged
        .into_values()
        .map(|(_, path, entry)| (path, entry))
        .collect())
}

fn canonicalize_visible_path(fs: &LaneFs, lane: &str, path: &str) -> Result<FilePath, i32> {
    let mut canonical = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let name = fs
            .list_dir(lane, &canonical)
            .map_err(status_from_lane_fs_error)?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(part))
            .map(|entry| entry.name)
            .unwrap_or_else(|| part.to_owned());
        canonical = child_path(&canonical, &name);
    }
    Ok(canonical)
}

fn collect_visible_files(
    fs: &LaneFs,
    lane: &str,
    path: &str,
    files: &mut Vec<FilePath>,
) -> Result<(), i32> {
    for entry in fs.list_dir(lane, path).map_err(status_from_lane_fs_error)? {
        let child = child_path(path, &entry.name);
        match entry.kind {
            DirEntryKind::Directory => collect_visible_files(fs, lane, &child, files)?,
            DirEntryKind::File => files.push(child),
        }
    }
    Ok(())
}

fn child_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    }
}

fn dir_entry_for_dirty_path(directory: &str, dirty_path: &str) -> Option<(String, DirEntryKind)> {
    let tail = if directory.is_empty() {
        dirty_path
    } else {
        dirty_path_descendant_tail(dirty_path, directory)?
    };
    if tail.is_empty() || tail == dirty_path && !directory.is_empty() {
        return None;
    }
    match tail.split_once('/') {
        Some((name, _)) => Some((name.to_owned(), DirEntryKind::Directory)),
        None => Some((tail.to_owned(), DirEntryKind::File)),
    }
}

fn dirty_path_descendant_tail<'a>(dirty_path: &'a str, directory: &str) -> Option<&'a str> {
    let separator = dirty_path.as_bytes().get(directory.len())?;
    let prefix = dirty_path.get(..directory.len())?;
    if *separator != b'/' || !prefix.eq_ignore_ascii_case(directory) {
        return None;
    }
    dirty_path.get(directory.len() + 1..)
}

fn is_descendant_path(path: &str, directory: &str) -> bool {
    if directory.is_empty() {
        return !path.is_empty();
    }
    dirty_path_descendant_tail(path, directory).is_some()
}

fn dirty_ancestor_hides(
    dirty: &BTreeMap<FilePath, DirtyEntry>,
    versions: &BTreeMap<FilePath, u64>,
    path: &str,
) -> bool {
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if let Some(entry) = newest_dirty_entry_for_path(dirty, versions, parent) {
            return matches!(entry, DirtyEntry::Deleted | DirtyEntry::File(_));
        }
        current = parent;
    }
    false
}

fn newest_dirty_entry_for_path<'a>(
    dirty: &'a BTreeMap<FilePath, DirtyEntry>,
    versions: &BTreeMap<FilePath, u64>,
    path: &str,
) -> Option<&'a DirtyEntry> {
    dirty
        .iter()
        .filter(|(dirty_path, _)| dirty_path.eq_ignore_ascii_case(path))
        .max_by_key(|(dirty_path, _)| versions.get(*dirty_path).copied().unwrap_or(0))
        .map(|(_, entry)| entry)
}

pub(super) fn prepare_session_fs(
    repo_root: &Path,
    storage_path: &Path,
    lane: &str,
    metrics: &VirtualFsMetrics,
) -> Result<LaneFs, VirtualExecError> {
    with_lane_fs_write(repo_root, storage_path, metrics, |fs| {
        fs.create_lane(lane).map_err(status_from_lane_fs_error)?;
        Ok(LaneFs::new(fs.repo().clone(), FileWorktree::new(repo_root)))
    })
}

fn with_lane_fs_write<T>(
    repo_root: &Path,
    storage_path: &Path,
    metrics: &VirtualFsMetrics,
    operation: impl FnOnce(&mut LaneFs) -> Result<T, i32>,
) -> Result<T, VirtualExecError> {
    let wait_start = Instant::now();
    let lock = acquire_repo_lock(storage_path).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to acquire lane storage lock {}: {error}",
            storage_path.display()
        ))
    })?;
    let wait_ms = elapsed_ms(wait_start);
    let held_start = Instant::now();
    let repo = load_repo(storage_path)
        .map_err(|error| {
            VirtualExecError::message(format!(
                "failed to load lane storage {}: {error}",
                storage_path.display()
            ))
        })?
        .unwrap_or_default();
    let mut fs = LaneFs::new(repo, FileWorktree::new(repo_root));
    let result = operation(&mut fs);
    if result.is_ok() {
        persist_repo(storage_path, fs.repo()).map_err(|error| {
            VirtualExecError::message(format!(
                "failed to persist lane storage {}: {error}",
                storage_path.display()
            ))
        })?;
    }
    let held_ms = elapsed_ms(held_start);
    drop(lock);
    metrics.record_write(wait_ms, held_ms);
    result.map_err(|status| VirtualExecError::from_status("apply lane storage update", status))
}
