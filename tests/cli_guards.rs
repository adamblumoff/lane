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
        let output = repo.run_unchecked(&[
            "exec",
            lane,
            "--",
            "pwsh",
            "-NoProfile",
            "-Command",
            "exit 0",
        ]);
        assert_command_fails_with(&output, "ReservedLane");
    }
    assert!(!repo.path().join(".lane/repo.json").exists());
}

#[test]
fn cli_path_commands_reject_repo_state_absolute_and_parent_paths() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"base");
    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "exit 0",
    ]);
    let replacement = repo.path().join("replacement.txt");
    fs::write(&replacement, b"replacement").unwrap();
    let absolute_path = repo.path().join("src/example.ts").display().to_string();

    assert!(
        repo.run_text(["diff", "agent-a", "./src/example.ts"])
            .contains("no changes in lane agent-a")
    );

    for (args, message) in [
        (
            vec!["diff".to_owned(), "agent-a".to_owned(), "".to_owned()],
            "missing path",
        ),
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
                "diff".to_owned(),
                "agent-a".to_owned(),
                ".GIT/config".to_owned(),
            ],
            "cannot project git metadata files",
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
