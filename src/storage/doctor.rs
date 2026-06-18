use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use crate::{LaneRunState, ensure_user_lane};

use super::blobs::{
    BlobInventory, blob_inventory, read_blob, record_blob_inventory, sha256_hex,
    validate_blob_reference,
};
use super::db::{self, StoredLaneEntry};
use super::paths::{db_path, last_run_file_name};
use super::serde_util::json_error;

pub(crate) fn doctor_storage(storage_root: &Path) -> io::Result<StorageDoctorReport> {
    inspect_storage(storage_root).map(|inspection| inspection.report)
}

pub(super) fn inspect_storage(storage_root: &Path) -> io::Result<StorageDoctorInspection> {
    let mut report = StorageDoctorReport::default();

    let store = match db::load_stored_repo(storage_root) {
        Ok(Some(store)) => store,
        Ok(None) => {
            let blob_inventory = blob_inventory(storage_root)?;
            let referenced_blobs = BTreeSet::new();
            record_blob_inventory(&blob_inventory, &referenced_blobs, &mut report);
            report.last_run_files = count_json_files(&storage_root.join("last_run"))?;
            return Ok(StorageDoctorInspection {
                report,
                referenced_blobs,
                blob_inventory,
            });
        }
        Err(error) => {
            report.store_present = db_path(storage_root).exists();
            report.errors.push(error.to_string());
            let blob_inventory = blob_inventory(storage_root)?;
            let referenced_blobs = BTreeSet::new();
            record_blob_inventory(&blob_inventory, &referenced_blobs, &mut report);
            report.last_run_files = count_json_files(&storage_root.join("last_run"))?;
            return Ok(StorageDoctorInspection {
                report,
                referenced_blobs,
                blob_inventory,
            });
        }
    };

    report.store_present = true;
    report.version = Some(store.version);
    report.lanes = store.lanes.len();
    report.files = store.files.len();
    for lane in &store.lanes {
        if let Err(error) = ensure_user_lane(lane) {
            report
                .errors
                .push(format!("database lane {lane:?} is invalid: {error}"));
        }
    }
    let expected_last_run = store
        .lanes
        .iter()
        .map(|lane| last_run_file_name(lane.as_str()))
        .collect::<BTreeSet<_>>();
    let referenced_blob_lengths = collect_referenced_blobs(&store);
    let referenced_blobs = referenced_blob_lengths
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();

    for (path, file) in &store.files {
        for (lane, entry) in &file.lanes {
            if !store.lanes.contains(lane) {
                report.errors.push(format!(
                    "file {} references missing lane {}",
                    path.as_str(),
                    lane.as_str()
                ));
            }
            if let StoredLaneEntry::Present(ops) = entry {
                report.ops += ops.len();
                for op in ops {
                    report.blobs_referenced += 1;
                    if let Err(error) = validate_blob_reference(&op.inserted_blob) {
                        report.errors.push(format!(
                            "file {} op {} has invalid blob reference {}: {error}",
                            path.as_str(),
                            op.id,
                            op.inserted_blob
                        ));
                    }
                }
            }
        }
    }
    validate_referenced_blobs(storage_root, &referenced_blob_lengths, &mut report);

    let last_run_dir = storage_root.join("last_run");
    report.last_run_files = count_json_files(&last_run_dir)?;
    if last_run_dir.exists() {
        for entry in fs::read_dir(&last_run_dir)? {
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
            if !expected_last_run.contains(file_name) {
                report.warnings.push(format!(
                    "last_run file {} does not belong to a database lane",
                    path.display()
                ));
                continue;
            }
            match fs::read(&path).and_then(|bytes| {
                serde_json::from_slice::<LaneRunState>(&bytes).map_err(json_error)
            }) {
                Ok(_) => {}
                Err(error) => report.errors.push(format!(
                    "last_run file {} is invalid: {error}",
                    path.display()
                )),
            }
        }
    }

    let blob_inventory = blob_inventory(storage_root)?;
    record_blob_inventory(&blob_inventory, &referenced_blobs, &mut report);
    Ok(StorageDoctorInspection {
        report,
        referenced_blobs,
        blob_inventory,
    })
}

fn collect_referenced_blobs(store: &db::StoredRepo) -> BTreeMap<String, BTreeSet<u64>> {
    let mut referenced: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for file in store.files.values() {
        for entry in file.lanes.values() {
            let StoredLaneEntry::Present(ops) = entry else {
                continue;
            };
            for op in ops {
                referenced
                    .entry(op.inserted_blob.clone())
                    .or_default()
                    .insert(op.inserted_len);
            }
        }
    }
    referenced
}

fn validate_referenced_blobs(
    storage_root: &Path,
    referenced_blobs: &BTreeMap<String, BTreeSet<u64>>,
    report: &mut StorageDoctorReport,
) {
    for (reference, expected_lengths) in referenced_blobs {
        if validate_blob_reference(reference).is_err() {
            continue;
        }
        match read_blob(storage_root, reference) {
            Ok(bytes) => {
                for expected_len in expected_lengths {
                    if bytes.len() as u64 != *expected_len {
                        report.errors.push(format!(
                            "blob {reference} length is {}; expected {expected_len}",
                            bytes.len()
                        ));
                    }
                }
                let actual = sha256_hex(&bytes);
                if format!("sha256/{actual}") != *reference {
                    report
                        .errors
                        .push(format!("blob {reference} content hash is sha256/{actual}"));
                }
            }
            Err(error) => report
                .errors
                .push(format!("blob {reference} is unreadable: {error}")),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct StorageDoctorInspection {
    pub(super) report: StorageDoctorReport,
    pub(super) referenced_blobs: BTreeSet<String>,
    pub(super) blob_inventory: BlobInventory,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct StorageDoctorReport {
    pub(crate) store_present: bool,
    pub(crate) version: Option<u32>,
    pub(crate) lanes: usize,
    pub(crate) files: usize,
    pub(crate) ops: usize,
    pub(crate) blobs_referenced: usize,
    pub(crate) blobs_present: usize,
    pub(crate) blobs_unreferenced: usize,
    pub(crate) last_run_files: usize,
    pub(crate) warnings: Vec<String>,
    pub(crate) errors: Vec<String>,
}

impl StorageDoctorReport {
    pub(crate) fn is_healthy(&self) -> bool {
        self.errors.is_empty()
    }
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
