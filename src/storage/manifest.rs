use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    BaseFingerprint, BaseStorageSnapshot, FileOpStorageSnapshot, FilePath,
    LaneEntryStorageSnapshot, LaneFileStorageSnapshot, LaneId, LaneRepoStorageSnapshot,
};

use super::blobs::{hex, persist_blob, read_blob, sha256_hex};
use super::paths::legacy_storage_path;
use super::serde_util::{invalid_storage, json_error};

const STORE_VERSION: u32 = 2;

pub(super) fn load_manifest_snapshot(
    storage_root: &Path,
    manifest_path: &Path,
) -> io::Result<Option<LaneRepoStorageSnapshot>> {
    let bytes = match fs::read(manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let manifest = parse_manifest(&bytes, manifest_path)?;
    snapshot_from_manifest(storage_root, manifest).map(Some)
}

pub(super) fn persist_manifest_snapshot(
    storage_root: &Path,
    manifest_path: &Path,
    snapshot: &LaneRepoStorageSnapshot,
) -> io::Result<()> {
    let manifest = manifest_from_snapshot(storage_root, snapshot)?;
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(json_error)?;
    super::atomic::persist_bytes(manifest_path, &bytes)
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

pub(super) fn reject_legacy_storage(storage_root: &Path) -> io::Result<()> {
    let legacy = legacy_storage_path(storage_root);
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

pub(super) fn parse_fingerprint(value: &str) -> io::Result<BaseFingerprint> {
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

pub(super) const fn store_version() -> u32 {
    STORE_VERSION
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredRepoManifest {
    pub(super) version: u32,
    pub(super) lanes: Vec<LaneId>,
    pub(super) files: Vec<StoredFile>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredFile {
    pub(super) path: FilePath,
    pub(super) base: StoredBase,
    pub(super) lanes: Vec<StoredLaneEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum StoredBase {
    Present { fingerprint: String },
    Missing,
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredLaneEntry {
    pub(super) lane: LaneId,
    pub(super) entry: StoredLaneEntryState,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum StoredLaneEntryState {
    Present { ops: Vec<StoredOp> },
    Deleted,
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredOp {
    pub(super) id: u64,
    pub(super) base_start: u64,
    pub(super) base_len: u64,
    pub(super) order_key: String,
    pub(super) inserted_blob: String,
    pub(super) inserted_len: u64,
}
