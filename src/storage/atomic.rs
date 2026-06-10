use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

const REPLACE_RETRY_ATTEMPTS: usize = 40;
const REPLACE_RETRY_DELAY: Duration = Duration::from_millis(25);

static NEXT_TEMP_PATH_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn persist_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp_path = temp_path_for(path)?;
    let result = (|| {
        let mut temp_file = fs::File::create(&temp_path)
            .map_err(|error| path_error("create temp file", &temp_path, error))?;
        temp_file
            .write_all(bytes)
            .map_err(|error| path_error("write temp file", &temp_path, error))?;
        temp_file
            .sync_all()
            .map_err(|error| path_error("sync temp file", &temp_path, error))?;
        drop(temp_file);
        replace_file_with_retry(&temp_path, path)
            .map_err(|error| path_error("replace file", path, error))
    })();

    if let Err(error) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn temp_path_for(path: &Path) -> io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?;
    let mut temp_name = OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP_PATH_ID.fetch_add(1, Ordering::Relaxed)
    ));
    Ok(path.with_file_name(temp_name))
}

fn replace_file_with_retry(from: &Path, to: &Path) -> io::Result<()> {
    for attempt in 1..=REPLACE_RETRY_ATTEMPTS {
        match replace_file(from, to) {
            Ok(()) => return Ok(()),
            Err(error)
                if attempt < REPLACE_RETRY_ATTEMPTS && is_transient_replace_error(&error) =>
            {
                thread::sleep(REPLACE_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("replace retry loop always returns");
}

fn is_transient_replace_error(error: &io::Error) -> bool {
    cfg!(windows)
        && (error.kind() == io::ErrorKind::PermissionDenied
            || matches!(error.raw_os_error(), Some(5 | 32)))
}

fn path_error(operation: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let from = windows_path(from);
    let to = windows_path(to);
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };

    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn windows_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
