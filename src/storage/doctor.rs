use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

use serde::Serialize;

use crate::{LaneExecState, ensure_user_lane};

use super::blobs::{read_blob, report_blob_inventory, sha256_hex, validate_blob_reference};
use super::manifest::{
    STORE_VERSION, StoredBase, StoredLaneEntryState, StoredRepoManifest, parse_fingerprint,
};
use super::paths::{last_exec_file_name, legacy_storage_path, manifest_path};
use super::serde_util::json_error;

pub(crate) fn doctor_storage(storage_root: &Path) -> io::Result<StorageDoctorReport> {
    let mut report = StorageDoctorReport::default();

    let legacy_path = legacy_storage_path(storage_root);
    if legacy_path.exists() {
        report.errors.push(format!(
            "legacy storage file {} is unsupported by storage v2",
            legacy_path.display()
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
            "manifest version {} is unsupported; expected {}",
            manifest.version, STORE_VERSION
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
