use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::LaneRepo;

pub fn load_repo(path: &Path) -> io::Result<Option<LaneRepo>> {
    match fs::read(path) {
        Ok(bytes) => LaneRepo::from_bytes(&bytes).map(Some).map_err(decode_error),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn persist_repo(path: &Path, repo: &LaneRepo) -> io::Result<()> {
    persist_bytes(path, &repo.to_bytes())
}

pub fn persist_bytes(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp_path = temp_path_for(path)?;
    let result = (|| {
        let mut temp_file = fs::File::create(&temp_path)?;
        temp_file.write_all(bytes)?;
        temp_file.sync_all()?;
        drop(temp_file);
        replace_file(&temp_path, path)
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
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".tmp");
    Ok(path.with_file_name(temp_name))
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

fn decode_error(error: crate::DecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
