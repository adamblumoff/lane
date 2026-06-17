#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_rejects_collapsed_review_and_acceptance_shortcuts() {
    let repo = TempRepo::new();

    for args in [
        vec!["changes", "agent-a"],
        vec!["conflicts", "agent-a"],
        vec!["accept-lane", "agent-a"],
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

    let base = repo.run_unchecked(&[
        "run",
        "base",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "exit 0",
    ]);
    assert_command_fails_with(&base, "reserved lane name");

    let whitespace = repo.run_unchecked(&[
        "run",
        "   ",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "exit 0",
    ]);
    assert_command_fails_with(&whitespace, "invalid lane name");

    assert!(!repo.path().join(".lane/repo.json").exists());
}

#[test]
fn cli_path_commands_reject_repo_state_absolute_and_parent_paths() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"base");
    repo.run_json([
        "run",
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
        repo.run_text(["review", "agent-a", "--diff", "./src/example.ts"])
            .contains("no changes in lane agent-a")
    );

    for (args, message) in [
        (
            vec![
                "review".to_owned(),
                "agent-a".to_owned(),
                "--diff".to_owned(),
                "".to_owned(),
            ],
            "missing path",
        ),
        (
            vec![
                "review".to_owned(),
                "agent-a".to_owned(),
                "--diff".to_owned(),
                ".lane/repo.json".to_owned(),
            ],
            "cannot project lane state files",
        ),
        (
            vec![
                "review".to_owned(),
                "agent-a".to_owned(),
                ".lane/repo.json".to_owned(),
                "agent-a:1".to_owned(),
            ],
            "cannot project lane state files",
        ),
        (
            vec![
                "accept".to_owned(),
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
                "review".to_owned(),
                "agent-a".to_owned(),
                "--diff".to_owned(),
                ".GIT/config".to_owned(),
            ],
            "cannot project git metadata files",
        ),
        (
            vec![
                "accept".to_owned(),
                "agent-a".to_owned(),
                "..\\outside.ts".to_owned(),
                "agent-a:1".to_owned(),
            ],
            "path must stay inside the repo",
        ),
        (
            vec![
                "accept".to_owned(),
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
