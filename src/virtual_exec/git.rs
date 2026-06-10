use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::support::path_label;
use super::types::VirtualExecError;

static NEXT_TEMP_GIT_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct GitView {
    temp_dir: TempGitDir,
}

impl GitView {
    pub(super) fn path(&self) -> &Path {
        &self.temp_dir.path
    }
}

struct TempGitDir {
    path: PathBuf,
}

impl Drop for TempGitDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn prepare_git_view(repo_root: &Path) -> Result<Option<GitView>, VirtualExecError> {
    let Some(source_git_dir) = resolve_git_dir(repo_root)? else {
        return Ok(None);
    };
    if !source_git_dir.is_dir() {
        return Ok(None);
    }

    let temp_dir = create_temp_git_dir()?;
    snapshot_git_metadata(&source_git_dir, &temp_dir.path)?;
    Ok(Some(GitView { temp_dir }))
}

fn resolve_git_dir(repo_root: &Path) -> Result<Option<PathBuf>, VirtualExecError> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Ok(Some(dot_git));
    }
    if !dot_git.is_file() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&dot_git).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to read git metadata pointer {}: {error}",
            dot_git.display()
        ))
    })?;
    let Some(git_dir) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
    else {
        return Ok(None);
    };
    let git_dir = PathBuf::from(git_dir);
    if git_dir.is_absolute() {
        Ok(Some(git_dir))
    } else {
        Ok(Some(repo_root.join(git_dir)))
    }
}

fn create_temp_git_dir() -> Result<TempGitDir, VirtualExecError> {
    let root = env::temp_dir().join("lane").join("git");
    fs::create_dir_all(&root).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to create temporary git metadata directory {}: {error}",
            root.display()
        ))
    })?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    for _ in 0..100 {
        let id = NEXT_TEMP_GIT_ID.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("{}-{timestamp}-{id}.git", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(TempGitDir { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(VirtualExecError::message(format!(
                    "failed to create temporary git metadata directory {}: {error}",
                    path.display()
                )));
            }
        }
    }
    Err(VirtualExecError::message(
        "failed to allocate temporary git metadata directory after 100 attempts",
    ))
}

fn snapshot_git_metadata(source: &Path, target: &Path) -> Result<(), VirtualExecError> {
    for file in ["HEAD", "config", "index", "packed-refs", "shallow"] {
        copy_file_if_exists(&source.join(file), &target.join(file))?;
    }
    copy_dir_if_exists(&source.join("refs"), &target.join("refs"))?;
    copy_dir_if_exists(&source.join("info"), &target.join("info"))?;

    let objects_info = target.join("objects").join("info");
    fs::create_dir_all(&objects_info).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to create temporary git object metadata {}: {error}",
            objects_info.display()
        ))
    })?;
    fs::write(
        objects_info.join("alternates"),
        format!("{}\n", git_path_label(source.join("objects"))),
    )
    .map_err(|error| {
        VirtualExecError::message(format!(
            "failed to write temporary git alternates for {}: {error}",
            source.display()
        ))
    })
}

fn copy_file_if_exists(source: &Path, target: &Path) -> Result<(), VirtualExecError> {
    if !source.is_file() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            VirtualExecError::message(format!(
                "failed to create temporary git metadata directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    fs::copy(source, target).map(|_| ()).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to copy git metadata {} to {}: {error}",
            source.display(),
            target.display()
        ))
    })
}

fn copy_dir_if_exists(source: &Path, target: &Path) -> Result<(), VirtualExecError> {
    if !source.is_dir() {
        return Ok(());
    }
    copy_dir(source, target)
}

fn copy_dir(source: &Path, target: &Path) -> Result<(), VirtualExecError> {
    fs::create_dir_all(target).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to create temporary git metadata directory {}: {error}",
            target.display()
        ))
    })?;
    for entry in fs::read_dir(source).map_err(|error| {
        VirtualExecError::message(format!(
            "failed to read git metadata directory {}: {error}",
            source.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            VirtualExecError::message(format!(
                "failed to read git metadata entry in {}: {error}",
                source.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            VirtualExecError::message(format!(
                "failed to inspect git metadata entry {}: {error}",
                entry.path().display()
            ))
        })?;
        let target_path = target.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target_path)?;
        } else if file_type.is_file() {
            copy_file_if_exists(&entry.path(), &target_path)?;
        }
    }
    Ok(())
}

pub(super) fn git_path_label(path: impl AsRef<Path>) -> String {
    path_label(path).replace('\\', "/")
}
