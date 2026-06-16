pub use lane::{
    BaseFingerprint, BaseStorageSnapshot, FileOpStorageSnapshot, FilePath,
    LaneEntryStorageSnapshot, LaneFileStorageSnapshot, LaneId, LaneRepo, LaneRepoStorageSnapshot,
    LaneRunState, ensure_user_lane,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use storage::{
    acquire_repo_lock, cleanup_storage, doctor_storage, is_lock_contention, load_last_run,
    load_repo, persist_last_run, persist_repo,
};

// This recompiles the crate-private storage module inside the integration test.
// Keep the lane::* re-exports above aligned with storage.rs crate:: imports.
#[allow(dead_code, unused_imports)]
#[path = "../src/storage.rs"]
mod storage;

static NEXT_UNIQUE_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[test]
fn storage_v2_persists_manifest_blobs_and_last_run() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();

    persist_repo(temp.path(), &repo).unwrap();
    persist_last_run(
        temp.path(),
        "agent-a",
        &LaneRunState::new(Some(0), None, "ok\n", "", vec!["src/new.ts".to_owned()]),
    )
    .unwrap();

    assert!(temp.path().join("repo.json").exists());
    assert_eq!(doctor_storage(temp.path()).unwrap().blobs_present, 1);
    assert!(temp.path().join("last_run/agent-a.json").exists());

    let loaded = load_repo(temp.path()).unwrap().unwrap();
    assert_eq!(
        loaded.read_path("src/new.ts", "agent-a", None).unwrap(),
        Some(b"new\n".to_vec())
    );
    let last_run = load_last_run(temp.path(), &lane_set(&loaded));
    let last_run = last_run.get("agent-a").unwrap();
    assert_eq!(last_run.exit_code, Some(0));
    assert_eq!(last_run.stdout.text, "ok\n");
    assert!(!last_run.stdout.truncated);
    assert_eq!(last_run.changed_paths, vec!["src/new.ts"]);
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
fn corrupt_last_run_is_advisory_but_doctor_reports_it() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    persist_last_run(
        temp.path(),
        "agent-a",
        &LaneRunState::new(Some(0), None, "ok\n", "", vec!["src/new.ts".to_owned()]),
    )
    .unwrap();
    fs::write(temp.path().join("last_run/agent-a.json"), b"not json").unwrap();

    let loaded = load_repo(temp.path()).unwrap().unwrap();
    assert_eq!(
        loaded.read_path("src/new.ts", "agent-a", None).unwrap(),
        Some(b"new\n".to_vec())
    );
    assert!(load_last_run(temp.path(), &lane_set(&loaded)).is_empty());

    let report = doctor_storage(temp.path()).unwrap();
    assert!(!report.is_healthy());
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("last_run file"))
    );
}

#[test]
fn orphan_last_run_is_warning_not_error() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    fs::create_dir_all(temp.path().join("last_run")).unwrap();
    fs::write(temp.path().join("last_run/agent-b.json"), b"not json").unwrap();

    let loaded = load_repo(temp.path()).unwrap().unwrap();
    assert!(load_last_run(temp.path(), &lane_set(&loaded)).is_empty());

    let report = doctor_storage(temp.path()).unwrap();
    assert!(report.is_healthy());
    assert_eq!(report.last_run_files, 1);
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
            .any(|warning| warning.contains("not referenced by repo.json"))
    );
}

#[test]
fn storage_cleanup_removes_unreferenced_blobs_without_touching_referenced_blobs() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    let referenced_blob = first_blob_path(temp.path());
    let stale_blob = stale_blob_path(temp.path());
    fs::write(&stale_blob, b"stale").unwrap();

    let cleanup = cleanup_storage(temp.path()).unwrap();

    assert_eq!(cleanup.blobs_removed, 1);
    assert_eq!(cleanup.bytes_removed, 5);
    assert_eq!(cleanup.blobs_remaining, 1);
    assert!(referenced_blob.exists());
    assert!(!stale_blob.exists());

    let report = doctor_storage(temp.path()).unwrap();
    assert!(report.is_healthy());
    assert_eq!(report.blobs_unreferenced, 0);
    assert!(report.warnings.is_empty());
}

#[test]
fn storage_cleanup_rejects_missing_manifest_without_deleting_blobs() {
    let temp = TempStorage::new();
    let stale_blob = stale_blob_path(temp.path());
    fs::write(&stale_blob, b"stale").unwrap();

    let error = cleanup_storage(temp.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("without repo.json"));
    assert!(stale_blob.exists());
}

#[test]
fn storage_cleanup_rejects_corrupt_manifest_without_deleting_blobs() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    let referenced_blob = first_blob_path(temp.path());
    let stale_blob = stale_blob_path(temp.path());
    fs::write(&stale_blob, b"stale").unwrap();
    fs::write(temp.path().join("repo.json"), b"not json").unwrap();

    let error = cleanup_storage(temp.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("cannot clean unhealthy"));
    assert!(referenced_blob.exists());
    assert!(stale_blob.exists());
}

#[test]
fn storage_cleanup_rejects_invalid_blob_file_without_deleting_blobs() {
    let temp = TempStorage::new();
    let repo = repo_with_agent_file();
    persist_repo(temp.path(), &repo).unwrap();
    let referenced_blob = first_blob_path(temp.path());
    let stale_blob = stale_blob_path(temp.path());
    let invalid_blob = temp.path().join("blobs/sha256/not-a-sha");
    fs::write(&stale_blob, b"stale").unwrap();
    fs::write(&invalid_blob, b"invalid").unwrap();

    let error = cleanup_storage(temp.path()).unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("invalid blob files"));
    assert!(referenced_blob.exists());
    assert!(stale_blob.exists());
    assert!(invalid_blob.exists());
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

fn lane_set(repo: &LaneRepo) -> BTreeSet<LaneId> {
    repo.lane_ids().map(str::to_owned).collect()
}

fn first_blob_path(storage_root: &Path) -> PathBuf {
    fs::read_dir(storage_root.join("blobs").join("sha256"))
        .unwrap()
        .next()
        .expect("test expected one blob file")
        .unwrap()
        .path()
}

fn stale_blob_path(storage_root: &Path) -> PathBuf {
    let path = storage_root
        .join("blobs/sha256/0000000000000000000000000000000000000000000000000000000000000000");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
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
