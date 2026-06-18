#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_review_ignores_corrupt_last_run_but_doctor_reports_it() {
    let repo = repo_with_agent_run();
    fs::write(repo.path().join(".lane/last_run/agent-a.json"), b"not json").unwrap();

    let review = repo.run_json(["review", "agent-a"]);
    assert_eq!(review["lanes"][0]["last_run"], Value::Null);
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
            .any(|error| error.as_str().unwrap().contains("last_run file"))
    );
}

#[test]
fn cli_discard_prunes_last_run_metadata_for_removed_lane() {
    let repo = repo_with_agent_run();
    assert!(repo.path().join(".lane/last_run/agent-a.json").exists());

    let discarded = repo.run_json(["discard", "agent-a"]);
    assert_eq!(discarded["removed"], true);
    assert!(!repo.path().join(".lane/last_run/agent-a.json").exists());

    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert_eq!(doctor["report"]["last_run_files"], 0);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());
}

#[test]
fn cli_doctor_warns_for_orphan_last_run_without_failing() {
    let repo = repo_with_agent_run();
    repo.run_json(["discard", "agent-a"]);
    fs::create_dir_all(repo.path().join(".lane/last_run")).unwrap();
    repo.write(".lane/last_run/agent-a.json", b"not json");

    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert_eq!(doctor["report"]["last_run_files"], 1);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());
    assert!(
        doctor["report"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("does not belong to a database lane"))
    );
}

#[test]
fn cli_cleanup_removes_unreferenced_blobs_and_doctor_drops_to_zero() {
    let repo = repo_with_agent_run();
    let stale_blob =
        ".lane/blobs/sha256/0000000000000000000000000000000000000000000000000000000000000000";
    repo.write(stale_blob, b"stale");

    let before = repo.run_json(["doctor"]);
    assert_eq!(before["report"]["blobs_unreferenced"], 1);

    let cleanup = repo.run_json(["doctor", "--cleanup"]);
    assert_eq!(cleanup["blobs_removed"], 1);
    assert_eq!(cleanup["bytes_removed"], 5);
    assert_eq!(cleanup["blobs_remaining"], 1);
    assert!(!repo.path().join(stale_blob).exists());

    let after = repo.run_json(["doctor"]);
    assert_eq!(after["healthy"], true);
    assert_eq!(after["report"]["blobs_unreferenced"], 0);
    assert!(after["report"]["warnings"].as_array().unwrap().is_empty());
}

#[test]
fn cli_cleanup_rejects_corrupt_store_without_deleting_blobs() {
    let repo = repo_with_agent_run();
    let stale_blob =
        ".lane/blobs/sha256/0000000000000000000000000000000000000000000000000000000000000000";
    repo.write(stale_blob, b"stale");
    fs::write(repo.path().join(".lane/lane.sqlite"), b"not sqlite").unwrap();

    let output = repo.run_unchecked(&["doctor", "--cleanup"]);

    assert_command_fails_with(&output, "cannot clean unhealthy");
    assert!(repo.path().join(stale_blob).exists());
}

#[test]
fn cli_cleanup_rejects_invalid_blob_file_without_deleting_blobs() {
    let repo = repo_with_agent_run();
    let stale_blob =
        ".lane/blobs/sha256/0000000000000000000000000000000000000000000000000000000000000000";
    let invalid_blob = ".lane/blobs/sha256/not-a-sha";
    repo.write(stale_blob, b"stale");
    repo.write(invalid_blob, b"invalid");

    let output = repo.run_unchecked(&["doctor", "--cleanup"]);

    assert_command_fails_with(&output, "invalid blob files");
    assert!(repo.path().join(stale_blob).exists());
    assert!(repo.path().join(invalid_blob).exists());
}
