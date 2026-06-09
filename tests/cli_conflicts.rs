#![cfg(windows)]

mod common;

use common::*;

#[test]
fn cli_resolve_op_handles_delete_conflict_with_replacement_file() {
    let repo = TempRepo::new();
    repo.write("src/mode.txt", b"mode=base\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath 'src/mode.txt'",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/mode.txt', \"mode=safe`n\")",
    ]);

    let review = repo.run_json(["review"]);
    let conflict = &review["paths"][0]["conflicts"][0];
    let op_ids = review_op_ids(&conflict["ops"]);
    assert!(op_ids.contains(&"agent-a:delete".to_owned()));
    assert!(op_ids.iter().any(|op_id| op_id.starts_with("agent-b:")));
    assert_eq!(conflict["ops"][0]["op"]["kind"], "delete");
    assert_eq!(conflict["ops"][0]["base"]["utf8"], "mode=base\n");
    assert_eq!(conflict["ops"][0]["inserted"]["len"], 0);

    let resolution = repo.path().join("delete-resolution.txt");
    fs::write(&resolution, b"mode=merged\n").unwrap();
    let resolved = output_json(&repo.run_vec(vec![
        "resolve-op".to_owned(),
        "agent-a".to_owned(),
        "src/mode.txt".to_owned(),
        "agent-a:delete".to_owned(),
        "--with-file".to_owned(),
        resolution.display().to_string(),
    ]));

    assert_eq!(resolved["resolved_op"]["op_id"], "agent-a:delete");
    assert_eq!(resolved["replacement"]["utf8"], "mode=merged\n");
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/mode.txt")).unwrap(),
        b"mode=merged\n"
    );

    let agent_b = repo.run_json(["review", "agent-b"]);
    let agent_b_path = review_path(&agent_b, "src/mode.txt");
    assert!(!agent_b_path["clean_ops"].as_array().unwrap().is_empty());
    assert!(agent_b_path["conflicts"].as_array().unwrap().is_empty());
}

#[test]
fn cli_review_keeps_whole_file_delete_conflict_grouped_with_boundary_insert() {
    let repo = TempRepo::new();
    repo.write("src/list.txt", b"one\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath 'src/list.txt'",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::AppendAllText('src/list.txt', \"two`n\")",
    ]);

    let review = repo.run_json(["review"]);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);
    assert_eq!(
        review_op_ids(&review["paths"][0]["conflicts"][0]["ops"]),
        vec!["agent-a:delete", "agent-b:1"]
    );
}

#[test]
fn cli_review_keeps_empty_file_delete_conflicted_with_insert() {
    let repo = TempRepo::new();
    repo.write("src/empty.txt", b"");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath 'src/empty.txt'",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/empty.txt', \"b\")",
    ]);

    let review = repo.run_json(["review"]);
    assert_eq!(review["summary"]["clean_ops"], 0);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);
    assert_eq!(
        review_op_ids(&review["paths"][0]["conflicts"][0]["ops"]),
        vec!["agent-a:delete", "agent-b:1"]
    );

    let promoted = repo.run_json(["promote-clean", "agent-a"]);
    assert!(promoted["promoted_ops"].as_array().unwrap().is_empty());
    assert_eq!(
        promoted["conflicts"][0]["ops"][0]["op_id"],
        "agent-a:delete"
    );
    assert!(repo.path().join("src/empty.txt").exists());
    assert_eq!(fs::read(repo.path().join("src/empty.txt")).unwrap(), b"");
}

#[test]
fn cli_resolve_op_handles_create_conflict_with_custom_winner() {
    let repo = TempRepo::new();

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Force -Path src | Out-Null; [IO.File]::WriteAllText('src/new.txt', \"from-a`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Force -Path src | Out-Null; [IO.File]::WriteAllText('src/new.txt', \"from-b`n\")",
    ]);

    let review = repo.run_json(["review"]);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);
    let conflict = &review["paths"][0]["conflicts"][0];
    assert_eq!(
        review_op_ids(&conflict["ops"]),
        vec!["agent-a:1", "agent-b:1"]
    );
    assert_eq!(conflict["ops"][0]["op"]["kind"], "create");
    assert_eq!(conflict["ops"][0]["base"]["len"], 0);

    let resolution = repo.path().join("create-resolution.txt");
    fs::write(&resolution, b"winner\n").unwrap();
    let resolved = output_json(&repo.run_vec(vec![
        "resolve-op".to_owned(),
        "agent-a".to_owned(),
        "src/new.txt".to_owned(),
        "agent-a:1".to_owned(),
        "--with-file".to_owned(),
        resolution.display().to_string(),
    ]));

    assert_eq!(resolved["resolved_op"]["op_id"], "agent-a:1");
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/new.txt")).unwrap(),
        b"winner\n"
    );

    let agent_b = repo.run_json(["review", "agent-b"]);
    let agent_b_path = review_path(&agent_b, "src/new.txt");
    assert_eq!(agent_b_path["clean_ops"][0]["op"]["kind"], "replace");
    assert!(agent_b_path["conflicts"].as_array().unwrap().is_empty());
    repo.run_json(["promote-clean", "agent-b"]);
    assert_eq!(
        fs::read(repo.path().join("src/new.txt")).unwrap(),
        b"from-b\n"
    );
}

#[test]
fn cli_review_and_resolve_op_cover_binary_replacement_conflicts() {
    let repo = TempRepo::new();
    repo.write("src/blob.bin", &[0, 1, 2, 3]);

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllBytes('src/blob.bin', [byte[]](0,1,9,255))",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllBytes('src/blob.bin', [byte[]](0,1,8,255))",
    ]);

    let review = repo.run_json(["review"]);
    let conflict = &review["paths"][0]["conflicts"][0];
    assert_eq!(
        review_op_ids(&conflict["ops"]),
        vec!["agent-a:1", "agent-b:1"]
    );
    assert_eq!(conflict["ops"][0]["op"]["kind"], "replace");
    assert_eq!(conflict["ops"][0]["inserted"]["len"], 4);
    assert_eq!(conflict["ops"][0]["inserted"]["utf8"], Value::Null);
    assert_eq!(
        conflict["ops"][0]["inserted"]["sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );

    let resolution = repo.path().join("binary-resolution.bin");
    fs::write(&resolution, [0, 1, 7, 255]).unwrap();
    let resolved = output_json(&repo.run_vec(vec![
        "resolve-op".to_owned(),
        "agent-a".to_owned(),
        "src/blob.bin".to_owned(),
        "agent-a:1".to_owned(),
        "--with-file".to_owned(),
        resolution.display().to_string(),
    ]));

    assert_eq!(resolved["resolved_op"]["op_id"], "agent-a:1");
    assert_eq!(resolved["replacement"]["utf8"], Value::Null);
    assert_eq!(
        resolved["replacement"]["sha256"].as_str().unwrap().len(),
        64
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/blob.bin")).unwrap(),
        [0, 1, 7, 255]
    );

    let agent_b = repo.run_json(["review", "agent-b"]);
    let agent_b_path = review_path(&agent_b, "src/blob.bin");
    assert_eq!(agent_b_path["clean_ops"][0]["op"]["inserted_len"], 4);
    assert!(agent_b_path["conflicts"].as_array().unwrap().is_empty());
}
