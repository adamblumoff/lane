use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::paths::REPO_LOCK_FILE;

const LOCK_RETRY_ATTEMPTS: usize = 1200;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);

pub(crate) struct RepoLock {
    path: PathBuf,
    _file: File,
}

pub(crate) fn acquire_repo_lock(storage_root: &Path) -> io::Result<RepoLock> {
    acquire_path_lock(&storage_root.join(REPO_LOCK_FILE))
}

fn acquire_path_lock(lock_path: &Path) -> io::Result<RepoLock> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut last_error = None;
    for _ in 0..LOCK_RETRY_ATTEMPTS {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut file) => {
                if let Err(error) = write_lock_owner(&mut file) {
                    let _ = fs::remove_file(lock_path);
                    return Err(error);
                }
                return Ok(RepoLock {
                    path: lock_path.to_path_buf(),
                    _file: file,
                });
            }
            Err(error) if is_lock_contention(&error) => {
                if reap_stale_lock(lock_path)? {
                    continue;
                }
                last_error = Some(error);
                thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for {}", lock_path.display()),
        )
    }))
}

fn write_lock_owner(file: &mut File) -> io::Result<()> {
    writeln!(file, "pid={}", std::process::id())?;
    file.sync_all()
}

fn reap_stale_lock(lock_path: &Path) -> io::Result<bool> {
    if !lock_is_stale(lock_path)? {
        return Ok(false);
    }

    match fs::remove_file(lock_path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) if is_lock_contention(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn lock_is_stale(lock_path: &Path) -> io::Result<bool> {
    let metadata = match fs::metadata(lock_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) if is_lock_contention(&error) => return Ok(false),
        Err(error) => return Err(error),
    };

    match fs::read_to_string(lock_path) {
        Ok(contents) => {
            if let Some(pid) = parse_lock_owner_pid(&contents)
                && let Some(running) = process_is_running(pid)
            {
                return Ok(!running);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) if is_lock_contention(&error) => return Ok(false),
        Err(error) => return Err(error),
    }

    Ok(lock_age_exceeds(&metadata))
}

fn lock_age_exceeds(metadata: &fs::Metadata) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= LOCK_STALE_AFTER)
}

fn parse_lock_owner_pid(contents: &str) -> Option<u32> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("pid=")?.trim().parse().ok())
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle == 0 {
        return Some(false);
    }
    unsafe {
        CloseHandle(handle);
    }
    Some(true)
}

#[cfg(target_os = "linux")]
fn process_is_running(pid: u32) -> Option<bool> {
    if pid == 0 {
        return Some(false);
    }
    Some(Path::new("/proc").join(pid.to_string()).exists())
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn process_is_running(_pid: u32) -> Option<bool> {
    None
}

pub(crate) fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AlreadyExists
        || (cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied)
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
