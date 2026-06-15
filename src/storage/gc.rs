use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use super::blobs::{blob_inventory, validate_blob_reference};
use super::doctor::doctor_storage;
use super::lock::acquire_repo_lock;
use super::manifest::{STORE_VERSION, StoredLaneEntryState, StoredRepoManifest};
use super::paths::manifest_path;
use super::serde_util::{invalid_storage, json_error};

pub(crate) fn gc_storage(storage_root: &Path) -> io::Result<StorageGcReport> {
    let _lock = acquire_repo_lock(storage_root)?;
    let doctor = doctor_storage(storage_root)?;
    if !doctor.manifest_present {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot gc lane storage without repo.json",
        ));
    }
    if !doctor.is_healthy() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot gc unhealthy lane storage; run lane doctor first",
        ));
    }

    let referenced_blobs = referenced_blobs(storage_root)?;
    let inventory = blob_inventory(storage_root)?;
    let stale_blobs = inventory
        .files
        .values()
        .filter(|blob| !referenced_blobs.contains(&blob.reference))
        .cloned()
        .collect::<Vec<_>>();

    let mut blobs_removed = 0;
    let mut bytes_removed = 0;
    for blob in stale_blobs {
        fs::remove_file(&blob.path)?;
        blobs_removed += 1;
        bytes_removed += blob.bytes;
    }

    Ok(StorageGcReport {
        blobs_removed,
        bytes_removed,
        blobs_remaining: blob_inventory(storage_root)?.blobs_present,
    })
}

fn referenced_blobs(storage_root: &Path) -> io::Result<BTreeSet<String>> {
    let manifest_path = manifest_path(storage_root);
    let bytes = fs::read(&manifest_path)?;
    let manifest = serde_json::from_slice::<StoredRepoManifest>(&bytes).map_err(json_error)?;
    if manifest.version != STORE_VERSION {
        return Err(invalid_storage(
            &manifest_path,
            format!(
                "unsupported lane storage version {}; expected {STORE_VERSION}",
                manifest.version
            ),
        ));
    }

    let mut references = BTreeSet::new();
    for file in &manifest.files {
        for lane in &file.lanes {
            let StoredLaneEntryState::Present { ops } = &lane.entry else {
                continue;
            };
            for op in ops {
                validate_blob_reference(&op.inserted_blob)?;
                references.insert(op.inserted_blob.clone());
            }
        }
    }
    Ok(references)
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StorageGcReport {
    pub(crate) blobs_removed: usize,
    pub(crate) bytes_removed: u64,
    pub(crate) blobs_remaining: usize,
}
