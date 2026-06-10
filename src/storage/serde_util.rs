use std::io;
use std::path::Path;

pub(super) fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub(super) fn invalid_storage(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid lane storage {}: {error}", path.display()),
    )
}
