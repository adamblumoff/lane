use std::path::{Path, PathBuf};

const MANIFEST_FILE: &str = "repo.json";
const LEGACY_REPO_FILE: &str = "repo.lane";
pub(super) const REPO_LOCK_FILE: &str = "repo.lock";

pub(super) fn manifest_path(storage_root: &Path) -> PathBuf {
    storage_root.join(MANIFEST_FILE)
}

pub(super) fn legacy_storage_path(storage_root: &Path) -> PathBuf {
    storage_root.join(LEGACY_REPO_FILE)
}

pub(super) fn last_exec_path(storage_root: &Path, lane: &str) -> PathBuf {
    storage_root
        .join("last_exec")
        .join(last_exec_file_name(lane))
}

pub(super) fn last_exec_file_name(lane: &str) -> String {
    format!("{}.json", encode_path_component(lane))
}

pub(crate) fn encode_path_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('~');
            encoded.push_str(&format!("{byte:02x}"));
        }
    }
    encoded
}
