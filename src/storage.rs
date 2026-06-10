use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    BaseFingerprint, BaseStorageSnapshot, FileOpStorageSnapshot, FilePath,
    LaneEntryStorageSnapshot, LaneExecState, LaneFileStorageSnapshot, LaneId, LaneRepo,
    LaneRepoStorageSnapshot, ensure_user_lane,
};

const MANIFEST_FILE: &str = "repo.json";
const LEGACY_REPO_FILE: &str = "repo.lane";
const REPO_LOCK_FILE: &str = "repo.lock";
const STORE_VERSION: u32 = 2;
const SHA256_HEX_LEN: usize = 64;

const LOCK_RETRY_ATTEMPTS: usize = 1200;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);
const LOCK_STALE_AFTER: Duration = Duration::from_secs(5);
const REPLACE_RETRY_ATTEMPTS: usize = 40;
const REPLACE_RETRY_DELAY: Duration = Duration::from_millis(25);

static NEXT_TEMP_PATH_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn load_repo(storage_root: &Path) -> io::Result<Option<LaneRepo>> {
    reject_legacy_storage(storage_root)?;
    let manifest_path = manifest_path(storage_root);
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let manifest = parse_manifest(&bytes, &manifest_path)?;
    let mut snapshot = snapshot_from_manifest(storage_root, manifest)?;
    snapshot.last_exec = load_last_exec(storage_root, &snapshot.lanes);
    LaneRepo::from_storage_snapshot(snapshot)
        .map(Some)
        .map_err(|error| invalid_storage(&manifest_path, error))
}

pub(crate) fn persist_repo(storage_root: &Path, repo: &LaneRepo) -> io::Result<()> {
    let snapshot = repo.storage_snapshot();
    fs::create_dir_all(storage_root)?;
    reject_legacy_storage(storage_root)?;

    let manifest = manifest_from_snapshot(storage_root, &snapshot)?;
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(json_error)?;
    persist_bytes(&manifest_path(storage_root), &bytes)?;
    prune_stale_last_exec_files(storage_root, &snapshot.lanes);
    Ok(())
}

pub(crate) fn persist_last_exec(
    storage_root: &Path,
    lane: &str,
    state: &LaneExecState,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(state).map_err(json_error)?;
    persist_bytes(&last_exec_path(storage_root, lane), &bytes)
}

pub(crate) fn doctor_storage(storage_root: &Path) -> io::Result<StorageDoctorReport> {
    let mut report = StorageDoctorReport::default();

    if storage_root.join(LEGACY_REPO_FILE).exists() {
        report.errors.push(format!(
            "legacy storage file {} is unsupported by storage v2",
            storage_root.join(LEGACY_REPO_FILE).display()
        ));
    }

    let manifest_path = manifest_path(storage_root);
    let bytes = match fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            report_blob_inventory(storage_root, &BTreeSet::new(), &mut report)?;
            report.last_exec_files = count_json_files(&storage_root.join("last_exec"))?;
            return Ok(report);
        }
        Err(error) => return Err(error),
    };

    report.manifest_present = true;
    let manifest = match serde_json::from_slice::<StoredRepoManifest>(&bytes) {
        Ok(manifest) => manifest,
        Err(error) => {
            report.errors.push(format!(
                "manifest {} is invalid JSON: {error}",
                manifest_path.display()
            ));
            report_blob_inventory(storage_root, &BTreeSet::new(), &mut report)?;
            report.last_exec_files = count_json_files(&storage_root.join("last_exec"))?;
            return Ok(report);
        }
    };

    report.version = Some(manifest.version);
    if manifest.version != STORE_VERSION {
        report.errors.push(format!(
            "manifest version {} is unsupported; expected {STORE_VERSION}",
            manifest.version
        ));
    }

    report.lanes = manifest.lanes.len();
    report.files = manifest.files.len();
    for lane in &manifest.lanes {
        if let Err(error) = ensure_user_lane(lane) {
            report
                .errors
                .push(format!("manifest lane {lane:?} is invalid: {error:?}"));
        }
    }
    let expected_last_exec = manifest
        .lanes
        .iter()
        .map(|lane| last_exec_file_name(lane))
        .collect::<BTreeSet<_>>();
    let mut referenced_blobs = BTreeSet::new();

    for file in &manifest.files {
        if let StoredBase::Present { fingerprint } = &file.base
            && parse_fingerprint(fingerprint).is_err()
        {
            report.errors.push(format!(
                "file {} has invalid base fingerprint {}",
                file.path, fingerprint
            ));
        }
        for lane_entry in &file.lanes {
            if !manifest.lanes.contains(&lane_entry.lane) {
                report.errors.push(format!(
                    "file {} references missing lane {}",
                    file.path, lane_entry.lane
                ));
            }
            if let StoredLaneEntryState::Present { ops } = &lane_entry.entry {
                report.ops += ops.len();
                for op in ops {
                    report.blobs_referenced += 1;
                    if let Err(error) = validate_blob_reference(&op.inserted_blob) {
                        report.errors.push(format!(
                            "file {} op {} has invalid blob reference {}: {error}",
                            file.path, op.id, op.inserted_blob
                        ));
                        continue;
                    }
                    referenced_blobs.insert(op.inserted_blob.clone());
                    match read_blob(storage_root, &op.inserted_blob) {
                        Ok(bytes) => {
                            if bytes.len() as u64 != op.inserted_len {
                                report.errors.push(format!(
                                    "blob {} length is {}; expected {}",
                                    op.inserted_blob,
                                    bytes.len(),
                                    op.inserted_len
                                ));
                            }
                            let actual = sha256_hex(&bytes);
                            if format!("sha256/{actual}") != op.inserted_blob {
                                report.errors.push(format!(
                                    "blob {} content hash is sha256/{actual}",
                                    op.inserted_blob
                                ));
                            }
                        }
                        Err(error) => report
                            .errors
                            .push(format!("blob {} is unreadable: {error}", op.inserted_blob)),
                    }
                }
            }
        }
    }

    let last_exec_dir = storage_root.join("last_exec");
    report.last_exec_files = count_json_files(&last_exec_dir)?;
    if last_exec_dir.exists() {
        for entry in fs::read_dir(&last_exec_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !file_name.ends_with(".json") {
                continue;
            }
            if !expected_last_exec.contains(file_name) {
                report.warnings.push(format!(
                    "last_exec file {} does not belong to a manifest lane",
                    path.display()
                ));
                continue;
            }
            match fs::read(&path).and_then(|bytes| {
                serde_json::from_slice::<LaneExecState>(&bytes).map_err(json_error)
            }) {
                Ok(_) => {}
                Err(error) => report.errors.push(format!(
                    "last_exec file {} is invalid: {error}",
                    path.display()
                )),
            }
        }
    }

    report_blob_inventory(storage_root, &referenced_blobs, &mut report)?;
    Ok(report)
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct StorageDoctorReport {
    pub(crate) manifest_present: bool,
    pub(crate) version: Option<u32>,
    pub(crate) lanes: usize,
    pub(crate) files: usize,
    pub(crate) ops: usize,
    pub(crate) blobs_referenced: usize,
    pub(crate) blobs_present: usize,
    pub(crate) blobs_unreferenced: usize,
    pub(crate) last_exec_files: usize,
    pub(crate) warnings: Vec<String>,
    pub(crate) errors: Vec<String>,
}

impl StorageDoctorReport {
    pub(crate) fn is_healthy(&self) -> bool {
        self.errors.is_empty()
    }
}

pub(crate) struct RepoLock {
    path: PathBuf,
    _file: File,
}

pub(crate) fn acquire_repo_lock(storage_root: &Path) -> io::Result<RepoLock> {
    acquire_path_lock(&storage_root.join(REPO_LOCK_FILE))
}

fn parse_manifest(bytes: &[u8], path: &Path) -> io::Result<StoredRepoManifest> {
    let manifest = serde_json::from_slice::<StoredRepoManifest>(bytes).map_err(json_error)?;
    if manifest.version != STORE_VERSION {
        return Err(invalid_storage(
            path,
            format!(
                "unsupported lane storage version {}; expected {STORE_VERSION}",
                manifest.version
            ),
        ));
    }
    Ok(manifest)
}

fn snapshot_from_manifest(
    storage_root: &Path,
    manifest: StoredRepoManifest,
) -> io::Result<LaneRepoStorageSnapshot> {
    let mut files = BTreeMap::new();
    for stored_file in manifest.files {
        files.insert(
            stored_file.path,
            LaneFileStorageSnapshot {
                base: match stored_file.base {
                    StoredBase::Present { fingerprint } => {
                        BaseStorageSnapshot::Present(parse_fingerprint(&fingerprint)?)
                    }
                    StoredBase::Missing => BaseStorageSnapshot::Missing,
                },
                lanes: stored_file
                    .lanes
                    .into_iter()
                    .map(|entry| {
                        let lane = entry.lane.clone();
                        stored_entry_to_snapshot(storage_root, entry)
                            .map(|entry_state| (lane, entry_state))
                    })
                    .collect::<io::Result<_>>()?,
            },
        );
    }

    Ok(LaneRepoStorageSnapshot {
        lanes: manifest.lanes.into_iter().collect(),
        last_exec: BTreeMap::new(),
        files,
    })
}

fn stored_entry_to_snapshot(
    storage_root: &Path,
    entry: StoredLaneEntry,
) -> io::Result<LaneEntryStorageSnapshot> {
    match entry.entry {
        StoredLaneEntryState::Deleted => Ok(LaneEntryStorageSnapshot::Deleted),
        StoredLaneEntryState::Present { ops } => Ok(LaneEntryStorageSnapshot::Present(
            ops.into_iter()
                .map(|op| {
                    Ok(FileOpStorageSnapshot {
                        id: op.id,
                        base_start: op.base_start,
                        base_len: op.base_len,
                        order_key: op.order_key,
                        inserted: read_blob(storage_root, &op.inserted_blob)?,
                    })
                })
                .collect::<io::Result<_>>()?,
        )),
    }
}

fn manifest_from_snapshot(
    storage_root: &Path,
    snapshot: &LaneRepoStorageSnapshot,
) -> io::Result<StoredRepoManifest> {
    let mut files = Vec::new();
    let mut persisted_blobs = BTreeSet::new();
    for (path, file) in &snapshot.files {
        files.push(StoredFile {
            path: path.clone(),
            base: match file.base {
                BaseStorageSnapshot::Present(fingerprint) => StoredBase::Present {
                    fingerprint: hex(&fingerprint),
                },
                BaseStorageSnapshot::Missing => StoredBase::Missing,
            },
            lanes: file
                .lanes
                .iter()
                .map(|(lane, entry)| {
                    stored_entry_from_snapshot(storage_root, entry, &mut persisted_blobs).map(
                        |entry| StoredLaneEntry {
                            lane: lane.clone(),
                            entry,
                        },
                    )
                })
                .collect::<io::Result<_>>()?,
        });
    }

    Ok(StoredRepoManifest {
        version: STORE_VERSION,
        lanes: snapshot.lanes.iter().cloned().collect(),
        files,
    })
}

fn stored_entry_from_snapshot(
    storage_root: &Path,
    entry: &LaneEntryStorageSnapshot,
    persisted_blobs: &mut BTreeSet<String>,
) -> io::Result<StoredLaneEntryState> {
    match entry {
        LaneEntryStorageSnapshot::Deleted => Ok(StoredLaneEntryState::Deleted),
        LaneEntryStorageSnapshot::Present(ops) => Ok(StoredLaneEntryState::Present {
            ops: ops
                .iter()
                .map(|op| {
                    let hash = sha256_hex(&op.inserted);
                    let inserted_blob = format!("sha256/{hash}");
                    if persisted_blobs.insert(inserted_blob.clone()) {
                        persist_blob(storage_root, &inserted_blob, &op.inserted)?;
                    }
                    Ok(StoredOp {
                        id: op.id,
                        base_start: op.base_start,
                        base_len: op.base_len,
                        order_key: op.order_key.clone(),
                        inserted_blob,
                        inserted_len: op.inserted.len() as u64,
                    })
                })
                .collect::<io::Result<_>>()?,
        }),
    }
}

fn load_last_exec(
    storage_root: &Path,
    lanes: &BTreeSet<LaneId>,
) -> BTreeMap<LaneId, LaneExecState> {
    lanes
        .iter()
        .filter_map(|lane| {
            let path = last_exec_path(storage_root, lane);
            let bytes = fs::read(path).ok()?;
            let state = serde_json::from_slice(&bytes).ok()?;
            Some((lane.clone(), state))
        })
        .collect()
}

fn prune_stale_last_exec_files(storage_root: &Path, lanes: &BTreeSet<LaneId>) {
    let last_exec_dir = storage_root.join("last_exec");
    let expected = lanes
        .iter()
        .map(|lane| last_exec_file_name(lane))
        .collect::<BTreeSet<_>>();

    // last_exec is advisory; failed cleanup must not block repo persistence.
    if let Ok(entries) = fs::read_dir(&last_exec_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !expected.contains(file_name) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn reject_legacy_storage(storage_root: &Path) -> io::Result<()> {
    let legacy = storage_root.join(LEGACY_REPO_FILE);
    if legacy.exists() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "legacy lane storage {} is unsupported; remove .lane to reset",
                legacy.display()
            ),
        ));
    }
    Ok(())
}

fn manifest_path(storage_root: &Path) -> PathBuf {
    storage_root.join(MANIFEST_FILE)
}

fn persist_blob(storage_root: &Path, reference: &str, bytes: &[u8]) -> io::Result<()> {
    let path = blob_path(storage_root, reference)?;
    if path.exists() {
        return Ok(());
    }
    persist_bytes(&path, bytes)
}

fn read_blob(storage_root: &Path, reference: &str) -> io::Result<Vec<u8>> {
    fs::read(blob_path(storage_root, reference)?)
}

fn blob_path(storage_root: &Path, reference: &str) -> io::Result<PathBuf> {
    validate_blob_reference(reference)?;
    let hash = reference.trim_start_matches("sha256/");
    Ok(storage_root.join("blobs").join("sha256").join(hash))
}

fn validate_blob_reference(reference: &str) -> io::Result<()> {
    let Some(hash) = reference.strip_prefix("sha256/") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "blob reference must start with sha256/",
        ));
    };
    if hash.len() != SHA256_HEX_LEN || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "blob reference must contain a 64-character hex sha256",
        ));
    }
    Ok(())
}

fn last_exec_path(storage_root: &Path, lane: &str) -> PathBuf {
    storage_root
        .join("last_exec")
        .join(last_exec_file_name(lane))
}

fn last_exec_file_name(lane: &str) -> String {
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

fn report_blob_inventory(
    storage_root: &Path,
    referenced_blobs: &BTreeSet<String>,
    report: &mut StorageDoctorReport,
) -> io::Result<()> {
    let present_blobs = present_blob_references(storage_root, report)?;
    for blob in present_blobs.difference(referenced_blobs) {
        report.blobs_unreferenced += 1;
        report
            .warnings
            .push(format!("blob {blob} is not referenced by repo.json"));
    }
    Ok(())
}

fn present_blob_references(
    storage_root: &Path,
    report: &mut StorageDoctorReport,
) -> io::Result<BTreeSet<String>> {
    let dir = storage_root.join("blobs").join("sha256");
    if !dir.exists() {
        return Ok(BTreeSet::new());
    }
    let mut blobs = BTreeSet::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        report.blobs_present += 1;
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            report
                .warnings
                .push(format!("blob file {} has a non-UTF-8 name", path.display()));
            continue;
        };
        let reference = format!("sha256/{file_name}");
        match validate_blob_reference(&reference) {
            Ok(()) => {
                blobs.insert(reference);
            }
            Err(error) => report.warnings.push(format!(
                "blob file {} is not a valid sha256 blob name: {error}",
                path.display()
            )),
        }
    }
    Ok(blobs)
}

fn count_json_files(dir: &Path) -> io::Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            count += 1;
        }
    }
    Ok(count)
}

fn parse_fingerprint(value: &str) -> io::Result<BaseFingerprint> {
    let bytes = parse_hex(value)?;
    if bytes.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "base fingerprint must be 32 bytes",
        ));
    }
    let mut fingerprint = [0; 32];
    fingerprint.copy_from_slice(&bytes);
    Ok(fingerprint)
}

fn parse_hex(value: &str) -> io::Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hex value must have an even length",
        ));
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|chunk| {
            let hi = hex_digit(chunk[0])?;
            let lo = hex_digit(chunk[1])?;
            Ok((hi << 4) | lo)
        })
        .collect()
}

fn hex_digit(byte: u8) -> io::Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hex value contains a non-hex digit",
        )),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn invalid_storage(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid lane storage {}: {error}", path.display()),
    )
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

#[derive(Serialize, Deserialize)]
struct StoredRepoManifest {
    version: u32,
    lanes: Vec<LaneId>,
    files: Vec<StoredFile>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    path: FilePath,
    base: StoredBase,
    lanes: Vec<StoredLaneEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum StoredBase {
    Present { fingerprint: String },
    Missing,
}

#[derive(Serialize, Deserialize)]
struct StoredLaneEntry {
    lane: LaneId,
    entry: StoredLaneEntryState,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum StoredLaneEntryState {
    Present { ops: Vec<StoredOp> },
    Deleted,
}

#[derive(Serialize, Deserialize)]
struct StoredOp {
    id: u64,
    base_start: u64,
    base_len: u64,
    order_key: String,
    inserted_blob: String,
    inserted_len: u64,
}
