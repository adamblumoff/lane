use std::error::Error;
use std::fmt;
use std::io;

use crate::vfs::LaneFsError;

pub(super) type CliResult<T> = Result<T, CliError>;

#[derive(Debug)]
pub struct CliError {
    message: String,
}

impl CliError {
    pub(super) fn message(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl Error for CliError {}

impl From<io::Error> for CliError {
    fn from(error: io::Error) -> Self {
        Self::message(error)
    }
}

impl From<LaneFsError> for CliError {
    fn from(error: LaneFsError) -> Self {
        Self::message(error)
    }
}

impl From<crate::LaneError> for CliError {
    fn from(error: crate::LaneError) -> Self {
        Self::message(format!("{error:?}"))
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::message(error)
    }
}
