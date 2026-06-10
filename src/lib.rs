mod cli;
mod core;
mod storage;
mod vfs;
#[cfg(windows)]
pub(crate) mod virtual_exec;

pub use cli::{CliError, run};
pub use core::{
    BaseFingerprint, BaseStorageSnapshot, DecodeError, FileOpStorageSnapshot, FilePath,
    LaneEntryStorageSnapshot, LaneError, LaneExecState, LaneFileStorageSnapshot, LaneId,
    LaneOpDetail, LaneOpKind, LaneOpSummary, LaneRepo, LaneRepoStorageSnapshot, LaneTextPreview,
    ensure_user_lane,
};
pub(crate) use core::{is_git_metadata_path, is_lane_state_path};
