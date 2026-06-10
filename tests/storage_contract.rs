mod common;

use common::fs;
pub use lane::{
    BaseFingerprint, BaseStorageSnapshot, FileOpStorageSnapshot, FilePath,
    LaneEntryStorageSnapshot, LaneExecState, LaneFileStorageSnapshot, LaneId, LaneRepo,
    LaneRepoStorageSnapshot, ensure_user_lane,
};
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use storage::{
    acquire_repo_lock, doctor_storage, is_lock_contention, load_repo, persist_last_exec,
    persist_repo,
};

// This recompiles the crate-private storage module inside the integration test.
// Keep the lane::* re-exports above aligned with storage.rs crate:: imports.
#[allow(dead_code)]
#[path = "../src/storage.rs"]
mod storage;

static NEXT_UNIQUE_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[test]
fn storage_v2_persists_manifest_blobs_and_last_exec() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();

    persist_repo(temp.path(), &repo).unwrap();
    persist_last_exec(
        temp.path(),
        "agent-a",
        &LaneExecState::new(Some(0), None, "ok\n", "", vec!["src/new.ts".to_owned()]),
    )
    .unwrap();

    assert!(temp.path().join("repo.json").exists());
    assert!(!temp.path().join("repo.lane").exists());
    assert_eq!(doctor_storage(temp.path()).unwrap().blobs_present, 1);
    assert!(temp.path().join("last_exec/agent-a.json").exists());

    let loaded = load_repo(temp.path()).unwrap().unwrap();
    assert_eq!(
        loaded.read_path("src/new.ts", "agent-a", None).unwrap(),
        Some(b"new\n".to_vec())
    );
    assert_eq!(
        loaded.last_exec("agent-a").unwrap().unwrap().changed_paths,
        vec!["src/new.ts"]
    );
}

#[test]
fn storage_v2_deduplicates_repeated_inserted_blobs() {
    let temp = TempStorage::new();
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    for index in 0..64 {
        repo.replace_path(
            &format!("generated/{index:02}.txt"),
            "agent-a",
            None,
            Some(b"same bytes\n".to_vec()),
        )
        .unwrap();
    }

    persist_repo(temp.path(), &repo).unwrap();

    let report = doctor_storage(temp.path()).unwrap();
    assert!(report.is_healthy());
    assert_eq!(report.ops, 64);
    assert_eq!(report.blobs_referenced, 64);
    assert_eq!(report.blobs_present, 1);
}

#[test]
fn corrupt_last_exec_is_advisory_but_doctor_reports_it() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    persist_last_exec(
        temp.path(),
        "agent-a",
        &LaneExecState::new(Some(0), None, "ok\n", "", vec!["src/new.ts".to_owned()]),
    )
    .unwrap();
    fs::write(temp.path().join("last_exec/agent-a.json"), b"not json").unwrap();

    let loaded = load_repo(temp.path()).unwrap().unwrap();
    assert_eq!(
        loaded.read_path("src/new.ts", "agent-a", None).unwrap(),
        Some(b"new\n".to_vec())
    );
    assert!(loaded.last_exec("agent-a").unwrap().is_none());

    let report = doctor_storage(temp.path()).unwrap();
    assert!(!report.is_healthy());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("last_exec file"))
    );
}

#[test]
fn orphan_last_exec_is_warning_not_error() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    fs::create_dir_all(temp.path().join("last_exec")).unwrap();
    fs::write(temp.path().join("last_exec/agent-b.json"), b"not json").unwrap();

    let loaded = load_repo(temp.path()).unwrap().unwrap();
    assert!(loaded.last_exec("agent-a").unwrap().is_none());

    let report = doctor_storage(temp.path()).unwrap();
    assert!(report.is_healthy());
    assert_eq!(report.last_exec_files, 1);
    assert!(report.errors.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("does not belong to a manifest lane"))
    );
}

#[test]
fn missing_blob_breaks_load_and_is_reported_by_doctor() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();

    fs::remove_file(first_blob_path(temp.path())).unwrap();

    let load_error = load_repo(temp.path()).unwrap_err();
    assert_eq!(load_error.kind(), io::ErrorKind::NotFound);
    let report = doctor_storage(temp.path()).unwrap();
    assert!(!report.is_healthy());
    assert_eq!(report.errors.len(), 1);
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("is unreadable"))
    );
    assert!(
        !report
            .errors
            .iter()
            .any(|error| error.contains("referenced blob"))
    );
}

#[test]
fn unreferenced_blob_is_reported_as_warning_not_error() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    let stale_blob = temp
        .path()
        .join("blobs/sha256/0000000000000000000000000000000000000000000000000000000000000000");
    fs::create_dir_all(stale_blob.parent().unwrap()).unwrap();
    fs::write(stale_blob, b"stale").unwrap();

    let report = doctor_storage(temp.path()).unwrap();
    assert!(report.is_healthy());
    assert_eq!(report.blobs_referenced, 1);
    assert_eq!(report.blobs_unreferenced, 1);
    assert!(report.errors.is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("is not referenced by repo.json"))
    );
}

#[test]
fn reserved_manifest_lane_is_reported_by_doctor() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    let path = temp.path().join("repo.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["lanes"] = serde_json::json!(["base", "agent-a"]);
    fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    let load_error = load_repo(temp.path()).unwrap_err();
    assert_eq!(load_error.kind(), io::ErrorKind::InvalidData);
    let report = doctor_storage(temp.path()).unwrap();
    assert!(!report.is_healthy());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("manifest lane \"base\" is invalid"))
    );
}

#[test]
fn lock_contention_includes_windows_permission_denied_errors() {
    assert!(is_lock_contention(&io::Error::new(
        io::ErrorKind::AlreadyExists,
        "lock exists",
    )));
    assert_eq!(
        is_lock_contention(&io::Error::new(
            io::ErrorKind::PermissionDenied,
            "lock denied",
        )),
        cfg!(windows)
    );
    assert!(!is_lock_contention(&io::Error::new(
        io::ErrorKind::NotFound,
        "not contention",
    )));
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn stale_pid_lock_is_reaped_on_acquire() {
    let temp = TempStorage::new();
    fs::write(temp.path().join("repo.lock"), "pid=4294967295\n").unwrap();

    let _lock = acquire_repo_lock(temp.path()).unwrap();

    assert!(temp.path().join("repo.lock").exists());
}

fn repo_with_agent_file() -> LaneRepo {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    repo.replace_path("src/new.ts", "agent-a", None, Some(b"new\n".to_vec()))
        .unwrap();
    repo
}

fn first_blob_path(storage_root: &Path) -> PathBuf {
    fs::read_dir(storage_root.join("blobs").join("sha256"))
        .unwrap()
        .next()
        .expect("test expected one blob file")
        .unwrap()
        .path()
}

struct TempStorage {
    root: PathBuf,
}

impl TempStorage {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .join(format!(
                "lane-storage-test-{}-{}",
                std::process::id(),
                unique_suffix()
            ))
            .join(".lane");
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempStorage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(
            self.root
                .parent()
                .expect("test storage root has parent directory"),
        );
    }
}

fn unique_suffix() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = NEXT_UNIQUE_SUFFIX.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-{sequence}")
}
