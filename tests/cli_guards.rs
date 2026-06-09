#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_rejects_collapsed_review_and_promotion_shortcuts() {
    let repo = TempRepo::new();

    for args in [
        vec!["changes", "agent-a"],
        vec!["conflicts", "agent-a"],
        vec!["promote", "agent-a", "src/example.ts"],
        vec!["promote-lane", "agent-a"],
    ] {
        let output = repo.run_unchecked(&args);
        assert!(
            !output.status.success(),
            "collapsed command unexpectedly succeeded: {args:?}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"),
            "stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn cli_rejects_reserved_lane_names_at_entry_points() {
    let repo = TempRepo::new();

    for lane in ["base", "   "] {
        let output = repo.run_unchecked(&["create", lane]);
        assert_command_fails_with(&output, "ReservedLane");
    }
    assert!(!repo.path().join(".lane/repo.json").exists());
}

#[test]
fn cli_path_commands_reject_repo_state_absolute_and_parent_paths() {
    let repo = TempRepo::new();
    repo.run_json(["create", "agent-a"]);
    let replacement = repo.path().join("replacement.txt");
    fs::write(&replacement, b"replacement").unwrap();
    let absolute_path = repo.path().join("src/example.ts").display().to_string();

    for (args, message) in [
        (
            vec![
                "diff".to_owned(),
                "agent-a".to_owned(),
                ".lane/repo.json".to_owned(),
            ],
            "cannot project lane state files",
        ),
        (
            vec![
                "show-op".to_owned(),
                "agent-a".to_owned(),
                ".lane/repo.json".to_owned(),
                "agent-a:1".to_owned(),
            ],
            "cannot project lane state files",
        ),
        (
            vec![
                "resolve-op".to_owned(),
                "agent-a".to_owned(),
                ".lane/repo.json".to_owned(),
                "agent-a:1".to_owned(),
                "--with-file".to_owned(),
                replacement.display().to_string(),
            ],
            "cannot project lane state files",
        ),
        (
            vec![
                "promote-ops".to_owned(),
                "agent-a".to_owned(),
                "..\\outside.ts".to_owned(),
                "agent-a:1".to_owned(),
            ],
            "path must stay inside the repo",
        ),
        (
            vec![
                "promote-ops".to_owned(),
                "agent-a".to_owned(),
                absolute_path,
                "agent-a:1".to_owned(),
            ],
            "path must be repo-relative",
        ),
    ] {
        let output = repo.run_vec_unchecked(args);
        assert_command_fails_with(&output, message);
    }

    assert_eq!(repo.run_json(["doctor"])["healthy"], true);
    assert!(!repo.path().join("outside.ts").exists());
}

#[test]
fn cli_exec_rejects_incompatible_pre_alpha_lane_storage_without_reset() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;\n");
    repo.write(".lane/repo.lane", b"old pre-alpha format");

    let output = run_lane_exec(
        repo.path(),
        "fresh-vfs",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path src/fresh.ts -Value \"export const fresh = true;\" -NoNewline",
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("legacy lane storage"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(repo.path().join(".lane/repo.lane")).unwrap(),
        b"old pre-alpha format"
    );
    assert!(!repo.path().join("src/fresh.ts").exists());
}
