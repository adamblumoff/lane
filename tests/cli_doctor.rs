#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_review_ignores_corrupt_last_exec_but_doctor_reports_it() {
    let repo = repo_with_agent_exec();
    fs::write(
        repo.path().join(".lane/last_exec/agent-a.json"),
        b"not json",
    )
    .unwrap();

    let review = repo.run_json(["review", "agent-a"]);
    assert_eq!(review["lanes"][0]["last_exec"], Value::Null);
    assert_eq!(review["summary"]["changed_paths"], 1);

    let doctor_output = repo.run_unchecked(&["doctor"]);
    assert!(!doctor_output.status.success());
    let doctor: Value = serde_json::from_slice(&doctor_output.stdout).unwrap();
    assert_eq!(doctor["healthy"], false);
    assert!(
        doctor["report"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("last_exec file"))
    );
}

#[test]
fn cli_discard_prunes_last_exec_metadata_for_removed_lane() {
    let repo = repo_with_agent_exec();
    assert!(repo.path().join(".lane/last_exec/agent-a.json").exists());

    let discarded = repo.run_json(["discard", "agent-a"]);
    assert_eq!(discarded["removed"], true);
    assert!(!repo.path().join(".lane/last_exec/agent-a.json").exists());

    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert_eq!(doctor["report"]["last_exec_files"], 0);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());
}

#[test]
fn cli_doctor_warns_for_orphan_last_exec_without_failing() {
    let repo = repo_with_agent_exec();
    repo.run_json(["discard", "agent-a"]);
    fs::create_dir_all(repo.path().join(".lane/last_exec")).unwrap();
    repo.write(".lane/last_exec/agent-a.json", b"not json");

    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert_eq!(doctor["report"]["last_exec_files"], 1);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());
    assert!(
        doctor["report"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("does not belong to a manifest lane"))
    );
}

#[test]
fn cli_doctor_reports_corrupt_repo_manifest_shape() {
    let repo = repo_with_agent_exec();
    fs::write(repo.path().join(".lane/repo.json"), b"not json").unwrap();

    let output = repo.run_unchecked(&["doctor"]);
    assert!(!output.status.success());
    let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doctor["healthy"], false);
    assert_eq!(doctor["report"]["manifest_present"], true);
    assert_eq!(doctor["report"]["blobs_present"].as_u64().unwrap(), 1);
    assert!(
        doctor["report"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("invalid JSON"))
    );
}

#[test]
fn cli_doctor_reports_missing_blob_shape() {
    let repo = repo_with_agent_exec();
    let missing_blob = first_blob_path(&repo);
    fs::remove_file(&missing_blob).unwrap();

    let output = repo.run_unchecked(&["doctor"]);
    assert!(!output.status.success());
    let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doctor["healthy"], false);
    assert_eq!(doctor["report"]["blobs_referenced"], 1);
    assert_eq!(doctor["report"]["blobs_present"], 0);
    assert_eq!(doctor["report"]["blobs_unreferenced"], 0);
    let errors = doctor["report"]["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert!(
        errors
            .iter()
            .any(|error| error.as_str().unwrap().contains("is unreadable"))
    );
    assert!(
        !errors
            .iter()
            .any(|error| error.as_str().unwrap().contains("referenced blob"))
    );
}

#[test]
fn cli_doctor_rejects_reserved_manifest_lane() {
    let repo = repo_with_agent_exec();
    let manifest_path = repo.path().join(".lane/repo.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["lanes"] = serde_json::json!(["base", "agent-a"]);
    fs::write(manifest_path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();

    let output = repo.run_unchecked(&["doctor"]);
    assert!(!output.status.success());
    let doctor: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(doctor["healthy"], false);
    assert!(
        doctor["report"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error
                .as_str()
                .unwrap()
                .contains("manifest lane \"base\" is invalid"))
    );
}

#[test]
fn cli_doctor_reports_unreferenced_blob_without_failing() {
    let repo = repo_with_agent_exec();
    repo.write(
        ".lane/blobs/sha256/0000000000000000000000000000000000000000000000000000000000000000",
        b"stale",
    );

    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert_eq!(doctor["report"]["blobs_referenced"], 1);
    assert_eq!(doctor["report"]["blobs_unreferenced"], 1);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());
    assert!(
        doctor["report"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("is not referenced by repo.json"))
    );
}
