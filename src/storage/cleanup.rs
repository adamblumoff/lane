use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use super::doctor::inspect_storage;
use super::lock::acquire_repo_lock;

pub(crate) fn cleanup_storage(storage_root: &Path) -> io::Result<StorageCleanupReport> {
    let _lock = acquire_repo_lock(storage_root)?;
    let inspection = inspect_storage(storage_root)?;
    let report = &inspection.report;
    if !report.store_present {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot clean lane storage without lane.sqlite",
        ));
    }
    if !report.is_healthy() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot clean unhealthy lane storage; run lane doctor first",
        ));
    }
    if !inspection.blob_inventory.warnings.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot clean storage with invalid blob files; run lane doctor first",
        ));
    }

    let stale_blobs = inspection
        .blob_inventory
        .files
        .iter()
        .filter(|(reference, _)| !inspection.referenced_blobs.contains(*reference))
        .map(|(_, blob)| blob.clone())
        .collect::<Vec<_>>();

    let mut blobs_removed = 0;
    let mut bytes_removed = 0;
    for blob in stale_blobs {
        fs::remove_file(&blob.path)?;
        blobs_removed += 1;
        bytes_removed += blob.bytes;
    }

    Ok(StorageCleanupReport {
        blobs_removed,
        bytes_removed,
        blobs_remaining: inspection.blob_inventory.blobs_present - blobs_removed,
    })
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StorageCleanupReport {
    pub(crate) blobs_removed: usize,
    pub(crate) bytes_removed: u64,
    pub(crate) blobs_remaining: usize,
}
