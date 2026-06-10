use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::atomic::persist_bytes;
use super::doctor::StorageDoctorReport;

const SHA256_HEX_LEN: usize = 64;

pub(super) fn persist_blob(storage_root: &Path, reference: &str, bytes: &[u8]) -> io::Result<()> {
    let path = blob_path(storage_root, reference)?;
    if path.exists() {
        return Ok(());
    }
    persist_bytes(&path, bytes)
}

pub(super) fn read_blob(storage_root: &Path, reference: &str) -> io::Result<Vec<u8>> {
    fs::read(blob_path(storage_root, reference)?)
}

fn blob_path(storage_root: &Path, reference: &str) -> io::Result<PathBuf> {
    validate_blob_reference(reference)?;
    let hash = reference.trim_start_matches("sha256/");
    Ok(storage_root.join("blobs").join("sha256").join(hash))
}

pub(super) fn validate_blob_reference(reference: &str) -> io::Result<()> {
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

pub(super) fn report_blob_inventory(
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

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
