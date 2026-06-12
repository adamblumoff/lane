#![cfg(windows)]

mod common;

use common::*;
use std::collections::BTreeSet;

#[test]
fn cli_exec_preserves_parallel_lane_outputs() {
    let repo = TempRepo::new();
    repo.write("src/feature.ts", b"export const approach = 'base';");

    let root_a = repo.path().to_path_buf();
    let root_b = repo.path().to_path_buf();
    let job_a = thread::spawn(move || {
        run_lane_exec(
            &root_a,
            "approach-a",
            "$ErrorActionPreference = \"Stop\"; Start-Sleep -Milliseconds 400; Set-Content -Path src/feature.ts -Value \"export const approach = 'a';\" -NoNewline; Set-Content -Path src/a.ts -Value \"export const a = true;\" -NoNewline",
        )
    });
    let job_b = thread::spawn(move || {
        run_lane_exec(
            &root_b,
            "approach-b",
            "$ErrorActionPreference = \"Stop\"; Start-Sleep -Milliseconds 400; Set-Content -Path src/feature.ts -Value \"export const approach = 'b';\" -NoNewline; Set-Content -Path src/b.ts -Value \"export const b = true;\" -NoNewline",
        )
    });

    let output_a = assert_success(job_a.join().unwrap());
    let output_b = assert_success(job_b.join().unwrap());
    let exec_a = output_json(&output_a);
    let exec_b = output_json(&output_b);
    assert_eq!(exec_a["lane"], "approach-a");
    assert_eq!(exec_b["lane"], "approach-b");
    assert_eq!(exec_a["exit_code"], 0);
    assert_eq!(exec_b["exit_code"], 0);
    assert_eq!(exec_a["worker_error"], Value::Null);
    assert_eq!(exec_b["worker_error"], Value::Null);

    assert_eq!(
        fs::read(repo.path().join("src/feature.ts")).unwrap(),
        b"export const approach = 'base';"
    );
    assert_eq!(
        review_change_statuses(&repo.run_json(["review", "approach-a"]), "approach-a"),
        {
            let mut expected = BTreeMap::new();
            expected.insert("src/a.ts".to_owned(), "created".to_owned());
            expected.insert("src/feature.ts".to_owned(), "modified".to_owned());
            expected
        }
    );
    assert_eq!(
        review_change_statuses(&repo.run_json(["review", "approach-b"]), "approach-b"),
        {
            let mut expected = BTreeMap::new();
            expected.insert("src/b.ts".to_owned(), "created".to_owned());
            expected.insert("src/feature.ts".to_owned(), "modified".to_owned());
            expected
        }
    );

    let approach_b = repo.run_json(["review", "approach-b"]);
    let promoted_clean = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&approach_b, "approach-b")["actions"],
            "promote_clean",
            "approach-b",
        ),
    );
    assert_eq!(change_statuses_from_key(&promoted_clean, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/b.ts".to_owned(), "created".to_owned());
        expected
    });

    let conflict_review = repo.run_json(["review"]);
    let conflict_actions =
        &review_path(&conflict_review, "src/feature.ts")["conflicts"][0]["actions"];
    let shown = run_review_action_json(
        &repo,
        review_action(conflict_actions, "show_op", "approach-b"),
    );
    let resolution = repo.path().join("approach-b-resolution.txt");
    fs::write(
        &resolution,
        shown["inserted"]["utf8"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(conflict_actions, "resolve_op", "approach-b"),
        &resolution,
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());

    assert_eq!(
        fs::read(repo.path().join("src/feature.ts")).unwrap(),
        b"export const approach = 'b';"
    );
    assert!(!repo.path().join("src/a.ts").exists());
    assert_eq!(
        fs::read(repo.path().join("src/b.ts")).unwrap(),
        b"export const b = true;"
    );
}

#[test]
fn cli_promote_ops_promotes_selected_same_file_op_and_preserves_other_lane_ops() {
    let repo = TempRepo::new();
    repo.write("src/math.txt", b"alpha=1\nbeta=2\ngamma=3\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/math.txt', \"alpha=10`nbeta=2`ngamma=30`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/math.txt', \"alpha=1`nbeta=20`ngamma=3`n\")",
    ]);

    let review = repo.run_json(["review", "agent-a"]);
    let ops = review_path(&review, "src/math.txt")["clean_ops"]
        .as_array()
        .unwrap();
    assert_eq!(ops.len(), 2);
    let op_id = ops[0]["op"]["op_id"].as_str().unwrap().to_owned();

    let promoted = repo.run_json(["promote-ops", "agent-a", "src/math.txt", op_id.as_str()]);
    assert_eq!(string_array(&promoted["promoted_ops"]), vec![op_id.clone()]);
    assert_eq!(promoted["promoted"][0]["ops"].as_array().unwrap().len(), 1);
    assert_eq!(promoted["promoted"][0]["ops"][0]["op_id"], op_id);
    assert_eq!(
        fs::read(repo.path().join("src/math.txt")).unwrap(),
        b"alpha=10\nbeta=2\ngamma=3\n"
    );

    let remaining_a = repo.run_json(["review", "agent-a"]);
    let remaining_a_ops = review_path(&remaining_a, "src/math.txt")["clean_ops"]
        .as_array()
        .unwrap();
    assert_eq!(remaining_a_ops.len(), 1);
    assert_eq!(remaining_a_ops[0]["op"]["op_id"], "agent-a:2");
    let remaining_b = repo.run_json(["review", "agent-b"]);
    assert_eq!(review_change_statuses(&remaining_b, "agent-b"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/math.txt".to_owned(), "modified".to_owned());
        expected
    });

    repo.run_json(["promote-clean", "agent-b"]);
    assert_eq!(
        fs::read(repo.path().join("src/math.txt")).unwrap(),
        b"alpha=10\nbeta=20\ngamma=3\n"
    );
}

#[test]
fn cli_conflicts_and_promote_clean_drive_op_level_orchestration() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=C`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=3`n\")",
    ]);

    let conflicts = repo.run_json(["review", "agent-a"]);
    let conflict_groups = review_path(&conflicts, "src/vars.txt")["conflicts"]
        .as_array()
        .unwrap();
    assert_eq!(conflict_groups.len(), 1);
    let conflict_ops = conflict_groups[0]["ops"].as_array().unwrap();
    assert_eq!(conflict_ops.len(), 1);
    assert_eq!(conflict_ops[0]["op"]["op_id"], "agent-a:2");
    assert_eq!(
        string_array(&conflict_ops[0]["op"]["conflicts_with"]),
        vec!["agent-b"]
    );

    let promoted = repo.run_json(["promote-clean", "agent-a"]);
    assert_eq!(promoted["promoted_ops"].as_array().unwrap().len(), 1);
    assert_eq!(promoted["promoted_ops"][0]["path"], "src/vars.txt");
    assert_eq!(
        string_array(&promoted["promoted_ops"][0]["ops"]),
        vec!["agent-a:1", "agent-a:3"]
    );
    assert_eq!(promoted["conflicts"][0]["ops"][0]["op_id"], "agent-a:2");
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=2\nc=C\n"
    );

    let remaining_a = repo.run_json(["review", "agent-a"]);
    let remaining_a_ops = review_path(&remaining_a, "src/vars.txt")["conflicts"][0]["ops"]
        .as_array()
        .unwrap();
    assert_eq!(remaining_a_ops.len(), 1);
    assert_eq!(remaining_a_ops[0]["op"]["op_id"], "agent-a:2");
    assert_eq!(
        string_array(&remaining_a_ops[0]["op"]["conflicts_with"]),
        vec!["agent-b"]
    );

    let conflict_review = repo.run_json(["review"]);
    let conflict_actions =
        &review_path(&conflict_review, "src/vars.txt")["conflicts"][0]["actions"];
    let shown =
        run_review_action_json(&repo, review_action(conflict_actions, "show_op", "agent-b"));
    let resolution = repo.path().join("agent-b-resolution.txt");
    fs::write(
        &resolution,
        shown["inserted"]["utf8"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(conflict_actions, "resolve_op", "agent-b"),
        &resolution,
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=X\nc=C\n"
    );
}

#[test]
fn cli_promote_clean_rolls_back_worktree_when_later_write_fails() {
    let repo = TempRepo::new();
    repo.write("src/swap/original.txt", b"original");

    let result = repo.run_json([
        "exec",
        "rollback-lane",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path a.txt -Value created -NoNewline; Remove-Item -Recurse -LiteralPath src/swap; Set-Content -Path src/swap -Value \"now a file\" -NoNewline; New-Item -ItemType Directory -Path zz-blocked -Force | Out-Null; Set-Content -Path zz-blocked/nested.txt -Value \"cannot write\" -NoNewline",
    ]);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert!(!repo.path().join("a.txt").exists());
    assert!(!repo.path().join("zz-blocked").exists());

    fs::write(repo.path().join("zz-blocked"), b"still a file").unwrap();
    let output = repo.run_unchecked(&["promote-clean", "rollback-lane"]);
    assert!(
        !output.status.success(),
        "promotion unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "failing promotion should not emit JSON stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    assert!(!repo.path().join("a.txt").exists());
    assert!(repo.path().join("src/swap").is_dir());
    assert_eq!(
        fs::read(repo.path().join("src/swap/original.txt")).unwrap(),
        b"original"
    );
    assert_eq!(
        fs::read(repo.path().join("zz-blocked")).unwrap(),
        b"still a file"
    );
    assert_eq!(
        review_change_statuses(&repo.run_json(["review", "rollback-lane"]), "rollback-lane"),
        {
            let mut expected = BTreeMap::new();
            expected.insert("a.txt".to_owned(), "created".to_owned());
            expected.insert("src/swap".to_owned(), "created".to_owned());
            expected.insert("src/swap/original.txt".to_owned(), "deleted".to_owned());
            expected.insert("zz-blocked/nested.txt".to_owned(), "created".to_owned());
            expected
        }
    );
}

#[test]
fn cli_review_groups_clean_ops_and_conflict_decisions_json_first() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\nd=4\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=3`nd=4`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=C`nd=4`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-c",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=2`nc=3`nd=D`n\")",
    ]);

    let review = repo.run_json(["review"]);
    assert_eq!(review["lane"], Value::Null);
    assert_eq!(review["summary"]["lanes"], 3);
    assert_eq!(review["summary"]["changed_paths"], 1);
    assert_eq!(review["summary"]["clean_ops"], 3);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);

    let path = &review["paths"][0];
    assert_eq!(path["path"], "src/vars.txt");
    assert_eq!(path["lanes"].as_array().unwrap().len(), 3);
    assert_eq!(
        review_op_ids(&path["clean_ops"]),
        vec!["agent-a:1", "agent-b:2", "agent-c:1"]
    );

    let conflict = &path["conflicts"][0];
    assert_eq!(conflict["range_start"], 6);
    assert_eq!(conflict["range_end"], 7);
    assert_eq!(string_array(&conflict["lanes"]), vec!["agent-a", "agent-b"]);
    assert_eq!(
        review_op_ids(&conflict["ops"]),
        vec!["agent-a:2", "agent-b:1"]
    );
    assert_eq!(conflict["ops"][0]["base"]["utf8"], "2");
    assert_eq!(conflict["ops"][0]["inserted"]["utf8"], "B");
    assert_eq!(conflict["ops"][1]["inserted"]["utf8"], "X");

    let agent_a_review = repo.run_json(["review", "agent-a"]);
    assert_eq!(agent_a_review["lane"], "agent-a");
    assert_eq!(agent_a_review["summary"]["lanes"], 1);
    assert_eq!(agent_a_review["summary"]["clean_ops"], 1);
    assert_eq!(agent_a_review["summary"]["conflicted_ops"], 1);
    assert_eq!(
        review_op_ids(&agent_a_review["paths"][0]["conflicts"][0]["ops"]),
        vec!["agent-a:2"]
    );

    repo.run_json(["promote-clean", "agent-a"]);
    let after_clean = repo.run_json(["review", "agent-a"]);
    assert_eq!(after_clean["summary"]["clean_ops"], 0);
    assert_eq!(after_clean["summary"]["conflicted_ops"], 1);
    assert_eq!(
        review_op_ids(&after_clean["paths"][0]["conflicts"][0]["ops"]),
        vec!["agent-a:2"]
    );
}

#[test]
fn cli_review_human_groups_by_path_with_copyable_commands() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\nd=4\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=3`nd=4`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=C`nd=4`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-c",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Force -Path src | Out-Null; Set-Content -Path \"src/owner's.txt\" -Value 'owned \"quote\"' -NoNewline",
    ]);

    let json_default = repo.run_json(["review"]);
    assert_eq!(json_default["summary"]["clean_ops"], 3);
    assert_eq!(json_default["summary"]["conflicted_ops"], 2);

    let human = repo.run_text(["review", "--human"]);
    assert!(human.starts_with("Lane review\nscope: all lanes\n"));
    assert!(human.contains(
        "summary: 3 lanes, 2 changed paths, 3 clean ops, 2 conflicted ops, 1 conflict group"
    ));
    assert!(human.contains(
        "Lane status\n  - agent-a: 1 changed path, 1 clean op, 1 conflicted op, last exec ok, exec touched 1 path"
    ));
    assert!(human.contains(
        "Promotable now\n  - agent-a: 1 clean op across 1 path, 1 changed path total, last exec ok, exec touched 1 path\n    command: lane promote-clean agent-a"
    ));
    assert!(human.contains(
        "Needs decision\n  - src/vars.txt group 1 [6..7), 2 ops, lanes: agent-a, agent-b"
    ));
    assert!(human.contains(
        "src/vars.txt\n  |- lanes\n  |  - agent-a modified, 2 ops (1 clean, 1 conflicted)"
    ));
    assert!(human.contains("  |- clean ops\n  |  - agent-a agent-a:1 replace [2..3), inserts 1 B"));
    assert!(human.contains("  |    base: \"1\"\n  |    inserted: \"A\""));
    assert!(human.contains("  |    promote: lane promote-ops agent-a src/vars.txt agent-a:1"));
    let owner_promote = human
        .lines()
        .find_map(|line| line.strip_prefix("  |    promote: "))
        .filter(|command| command.contains("owner''s.txt"))
        .unwrap_or_else(|| panic!("missing quoted owner promote command:\n{human}"));
    assert_eq!(
        owner_promote,
        "lane promote-ops agent-c 'src/owner''s.txt' agent-c:1"
    );
    assert_success(run_human_command(&repo, owner_promote));
    assert_eq!(
        fs::read(repo.path().join("src/owner's.txt")).unwrap(),
        b"owned \"quote\""
    );
    assert!(human.contains("  |    inserted: \"owned \\\"quote\\\"\""));
    assert!(human.contains("  `- conflict groups\n     - group 1 [6..7), lanes: agent-a, agent-b"));
    assert!(human.contains("         base: \"2\"\n         inserted: \"B\""));
    assert!(human.contains("         inserted: \"X\""));
    assert!(human.contains(
        "         resolve: lane resolve-op agent-a src/vars.txt agent-a:2 --with-file <replacement-file>"
    ));
    assert!(!human.contains("inspect:"));
    assert!(human.contains(
        "Discard lanes\n  - agent-a: lane discard agent-a\n  - agent-b: lane discard agent-b"
    ));

    let agent_a_human = repo.run_text(["review", "--human", "agent-a"]);
    assert!(agent_a_human.contains("scope: agent-a"));
    assert!(agent_a_human.contains("agent-a agent-a:1"));
    assert!(!agent_a_human.contains("agent-b agent-b:2"));
}

#[test]
fn cli_review_human_escapes_and_bounds_inline_previews() {
    let repo = TempRepo::new();

    repo.run_json([
        "exec",
        "preview-agent",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Force -Path src | Out-Null; Set-Content -Path src/quoted.txt -Value 'say \"hi\"' -NoNewline; [IO.File]::WriteAllText('src/tabs.txt', \"`t\" * 200)",
    ]);

    let human = repo.run_text(["review", "--human"]);
    assert!(human.contains("inserted: \"say \\\"hi\\\"\""));
    assert!(human.contains("inserted: \"\\t\\t\\t"));
    assert!(human.contains("...\" (200 B, sha256 "));
}

#[test]
fn cli_show_op_and_resolve_op_complete_conflicted_operation_flow() {
    let repo = TempRepo::new();
    repo.write("src/vars.txt", b"a=1\nb=2\nc=3\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=A`nb=B`nc=C`n\")",
    ]);
    repo.run_json([
        "exec",
        "agent-b",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/vars.txt', \"a=1`nb=X`nc=3`n\")",
    ]);

    repo.run_json(["promote-clean", "agent-a"]);

    let shown = repo.run_json(["show-op", "agent-a", "src/vars.txt", "agent-a:2"]);
    assert_eq!(shown["op"]["op_id"], "agent-a:2");
    assert_eq!(shown["base"]["utf8"], "2");
    assert_eq!(shown["inserted"]["utf8"], "B");
    assert_eq!(
        string_array(&shown["op"]["conflicts_with"]),
        vec!["agent-b"]
    );

    let replacement = repo.path().join("resolution.txt");
    fs::write(&replacement, b"Y").unwrap();
    let resolved = output_json(&repo.run_vec(vec![
        "resolve-op".to_owned(),
        "agent-a".to_owned(),
        "src/vars.txt".to_owned(),
        "agent-a:2".to_owned(),
        "--with-file".to_owned(),
        replacement.display().to_string().to_owned(),
    ]));

    assert_eq!(resolved["resolved_op"]["op_id"], "agent-a:2");
    assert_eq!(resolved["replacement"]["utf8"], "Y");
    assert!(resolved["remaining"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=Y\nc=C\n"
    );

    let agent_b = repo.run_json(["review", "agent-b"]);
    let agent_b_path = review_path(&agent_b, "src/vars.txt");
    let agent_b_ops = agent_b_path["clean_ops"].as_array().unwrap();
    assert_eq!(agent_b_ops.len(), 1);
    assert_eq!(agent_b_ops[0]["op"]["inserted_len"], 1);
    assert!(agent_b_path["conflicts"].as_array().unwrap().is_empty());
}

#[test]
fn cli_try_check_compare_lists_attempt_evidence_without_ranking() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");

    let attempted = repo.run_json([
        "try",
        "--name",
        "login",
        "--attempts",
        "3",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; if ($env:LANE_ID -eq \"login-2\") { Set-Content -Path src/login.tsx -Value \"export const design = 'preferred';\" -NoNewline; Set-Content -Path src/preferred.ts -Value \"export const preferred = true;\" -NoNewline } else { Set-Content -Path src/login.tsx -Value \"export const design = '$env:LANE_ID';\" -NoNewline }",
    ]);
    assert_eq!(attempted["run"]["name"], "login");
    assert_eq!(attempted["run"]["attempts"].as_array().unwrap().len(), 3);
    assert!(repo.path().join(".lane/runs/login.json").exists());
    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'base';"
    );

    let checked_output = repo.run_unchecked(&[
        "check",
        "login",
        "--name",
        "pick-second",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Continue\"; Set-Content -Path check-artifact.txt -Value artifact -NoNewline; if ($env:LANE_ID -ne \"login-2\") { Write-Error \"not selected\"; exit 9 }",
    ]);
    assert!(
        !checked_output.status.success(),
        "lane check should fail when any check attempt fails"
    );
    let checked = output_json(&checked_output);
    assert_eq!(checked["check"]["name"], "pick-second");
    assert_eq!(checked["check"]["attempts"].as_array().unwrap().len(), 3);
    assert_eq!(checked["check"]["attempts"][0]["exec"]["exit_code"], 9);
    assert_eq!(checked["check"]["attempts"][1]["exec"]["exit_code"], 0);

    let preferred_review = repo.run_json(["review", "login-2"]);
    let preferred_paths = review_paths(&preferred_review);
    assert_eq!(preferred_paths, vec!["src/login.tsx", "src/preferred.ts"]);
    assert!(!preferred_paths.contains(&"check-artifact.txt"));

    let compared = repo.run_json(["compare", "login"]);
    assert_eq!(compared["run"]["checks"].as_array().unwrap().len(), 1);
    assert_eq!(compared["attempts"][0]["lane"], "login-1");
    assert_eq!(compared["attempts"][1]["lane"], "login-2");
    assert_eq!(compared["attempts"][2]["lane"], "login-3");
    assert_eq!(compared["attempts"][1]["checks_passed"], 1);
    assert_eq!(compared["attempts"][1]["checks_failed"], 0);
    assert!(
        compared["attempts"][1]["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["kind"] == "promote_clean")
    );
    assert!(
        compared["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["command"] == serde_json::json!(["discard-run", "login"]))
    );
    assert_eq!(compared["review"]["summary"]["lanes"], 3);
    assert_eq!(compared["review"]["summary"]["changed_paths"], 2);

    let runs = repo.run_json(["runs"]);
    assert_eq!(runs["runs"].as_array().unwrap().len(), 1);
    let run_summary = &runs["runs"][0];
    assert_eq!(run_summary["name"], "login");
    assert_eq!(run_summary["attempts"], 3);
    assert_eq!(
        string_array(&run_summary["attempt_lanes"]),
        vec!["login-1", "login-2", "login-3"]
    );
    assert_eq!(run_summary["attempts_ok"], 3);
    assert_eq!(run_summary["attempts_failed"], 0);
    assert_eq!(run_summary["checks"], 1);
    assert_eq!(run_summary["checks_passed"], 1);
    assert_eq!(run_summary["checks_failed"], 2);
    assert!(
        run_summary["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["command"] == serde_json::json!(["run", "login"]))
    );

    let run_detail = repo.run_json(["run", "login"]);
    assert_eq!(run_detail["run"]["name"], "login");
    assert_eq!(run_detail["attempts"][1]["lane"], "login-2");
    assert_eq!(run_detail["attempts"][1]["checks_passed"], 1);

    let human = repo.run_text(["compare", "login", "--human"]);
    assert!(human.starts_with("Lane compare\nrun: login\n"));
    assert!(
        human.contains("Run actions\n  - runs: lane runs\n  - discard_run: lane discard-run login")
    );
    assert!(human.contains("Attempts\n  - login-1: attempt ok, checks 0/1"));
    assert!(human.contains("  - login-2: attempt ok, checks 1/1"));
    assert!(human.contains("promote_clean: lane promote-clean login-2"));
    assert!(human.contains("  - pick-second\n    login-1: exit 9\n    login-2: ok"));

    let discarded = repo.run_json(["discard-run", "login"]);
    assert_eq!(discarded["removed_attempt_lanes"], 3);
    assert_eq!(discarded["discarded_changes"], 4);
    assert!(!repo.path().join(".lane/runs/login.json").exists());
    assert!(
        repo.run_json(["runs"])["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(repo.run_json(["review"])["summary"]["lanes"], 0);
    assert_command_fails_with(
        &repo.run_unchecked(&["run", "login"]),
        "run \"login\" is not readable",
    );
}

#[test]
fn cli_run_detail_keeps_cleanup_available_when_base_changed() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = 'original';");

    repo.run_json([
        "try",
        "--name",
        "stale",
        "--attempts",
        "1",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path src/base.ts -Value \"export const base = 'attempt';\" -NoNewline",
    ]);
    repo.write("src/base.ts", b"export const base = 'parent';");

    let detail = repo.run_json(["run", "stale"]);
    assert!(
        detail["review_error"]
            .as_str()
            .unwrap()
            .contains("BaseChanged")
    );
    assert_eq!(detail["run"]["name"], "stale");
    assert_eq!(detail["attempts"][0]["lane"], "stale-1");
    assert_eq!(
        review_action_kinds(&detail["attempts"][0]["actions"]),
        vec!["discard"]
    );
    assert!(
        detail["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["command"] == serde_json::json!(["discard-run", "stale"]))
    );

    let human = repo.run_text(["run", "stale", "--human"]);
    assert!(human.contains("summary: 1 attempt, 0 checks, review unavailable"));
    assert!(human.contains("discard_run: lane discard-run stale"));
    assert!(human.contains("Needs decision\n  - review unavailable:"));

    let discarded = repo.run_json(["discard-run", "stale"]);
    assert_eq!(discarded["removed_attempt_lanes"], 1);
    assert_eq!(discarded["discarded_changes"], 0);
    assert!(!repo.path().join(".lane/runs/stale.json").exists());
}

#[test]
fn cli_try_and_check_spawn_all_workers_before_joining() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");
    let markers = std::env::temp_dir().join(format!("lane-parallel-{}", unique_suffix()));
    let try_markers = markers.join("try");
    let check_markers = markers.join("check");
    fs::create_dir_all(&try_markers).unwrap();
    fs::create_dir_all(&check_markers).unwrap();

    let try_marker_arg = ps_single_quoted_path(&try_markers);
    let try_script = format!(
        "$ErrorActionPreference = \"Stop\"; $dir = {try_marker_arg}; $me = $env:LANE_ID; Set-Content -LiteralPath (Join-Path $dir ($me + '.started')) -Value started -NoNewline; $other = if ($me -eq 'parallel-1') {{ 'parallel-2' }} else {{ 'parallel-1' }}; $deadline = (Get-Date).AddSeconds(5); while (-not (Test-Path -LiteralPath (Join-Path $dir ($other + '.started')))) {{ if ((Get-Date) -gt $deadline) {{ throw 'other attempt did not start' }}; Start-Sleep -Milliseconds 50 }}; Set-Content -Path ($me + '.txt') -Value done -NoNewline"
    );
    let tried = repo.run_json([
        "try",
        "--name",
        "parallel",
        "--attempts",
        "2",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        try_script.as_str(),
    ]);
    assert_eq!(tried["run"]["attempts"][0]["exec"]["exit_code"], 0);
    assert_eq!(tried["run"]["attempts"][1]["exec"]["exit_code"], 0);

    let check_marker_arg = ps_single_quoted_path(&check_markers);
    let check_script = format!(
        "$ErrorActionPreference = \"Stop\"; $dir = {check_marker_arg}; $me = $env:LANE_ID; Set-Content -LiteralPath (Join-Path $dir ($me + '.started')) -Value started -NoNewline; $other = if ($me -eq 'parallel-1') {{ 'parallel-2' }} else {{ 'parallel-1' }}; $deadline = (Get-Date).AddSeconds(5); while (-not (Test-Path -LiteralPath (Join-Path $dir ($other + '.started')))) {{ if ((Get-Date) -gt $deadline) {{ throw 'other check did not start' }}; Start-Sleep -Milliseconds 50 }}"
    );
    let checked = repo.run_json([
        "check",
        "parallel",
        "--name",
        "parallel-check",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        check_script.as_str(),
    ]);
    assert_eq!(checked["check"]["attempts"][0]["exec"]["exit_code"], 0);
    assert_eq!(checked["check"]["attempts"][1]["exec"]["exit_code"], 0);

    let _ = fs::remove_dir_all(markers);
}

#[test]
fn cli_check_merges_concurrent_check_results() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");
    repo.run_json([
        "try",
        "--name",
        "merge-checks",
        "--attempts",
        "1",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path src/attempt.ts -Value \"export const attempt = true;\" -NoNewline",
    ]);

    let markers = std::env::temp_dir().join(format!("lane-check-merge-{}", unique_suffix()));
    fs::create_dir_all(&markers).unwrap();
    let marker_arg = ps_single_quoted_path(&markers);
    let script_a = concurrent_check_script(&marker_arg, "a", "b");
    let script_b = concurrent_check_script(&marker_arg, "b", "a");
    let root_a = repo.path().to_path_buf();
    let root_b = repo.path().to_path_buf();
    let job_a = thread::spawn(move || run_named_check(&root_a, "a", &script_a));
    let job_b = thread::spawn(move || run_named_check(&root_b, "b", &script_b));
    assert_success(job_a.join().unwrap());
    assert_success(job_b.join().unwrap());

    let compare = repo.run_json(["compare", "merge-checks"]);
    let check_names = compare["run"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|check| check["name"].as_str().unwrap().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        check_names,
        ["a".to_owned(), "b".to_owned()].into_iter().collect()
    );

    let _ = fs::remove_dir_all(markers);
}

#[test]
fn cli_try_rejects_existing_attempt_lanes() {
    let repo = TempRepo::new();
    repo.run_json([
        "exec",
        "dupe-1",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "exit 0",
    ]);

    let output = repo.run_unchecked(&[
        "try",
        "--name",
        "dupe",
        "--attempts",
        "1",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"",
    ]);

    assert_command_fails_with(&output, "attempt lanes already exist: dupe-1");
}

#[test]
fn cli_try_records_failed_attempts_as_comparison_evidence() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");

    let output = repo.run([
        "try",
        "--name",
        "failure-evidence",
        "--attempts",
        "2",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Continue\"; if ($env:LANE_ID -eq \"failure-evidence-1\") { Write-Error \"attempt failed\"; exit 7 }; Set-Content -Path src/preferred.ts -Value \"export const preferred = true;\" -NoNewline",
    ]);
    assert_success(output);
    let attempted = output_json(&repo.run(["compare", "failure-evidence"]));
    assert_eq!(attempted["attempts"][0]["lane"], "failure-evidence-1");
    assert_eq!(attempted["attempts"][0]["attempt_ok"], false);
    assert_eq!(attempted["attempts"][0]["attempt_exit_code"], 7);
    assert_eq!(attempted["attempts"][1]["lane"], "failure-evidence-2");
    assert_eq!(attempted["attempts"][1]["attempt_ok"], true);
}

fn concurrent_check_script(marker_arg: &str, name: &str, other: &str) -> String {
    format!(
        "$ErrorActionPreference = \"Stop\"; $dir = {marker_arg}; $name = '{name}'; $other = '{other}'; Set-Content -LiteralPath (Join-Path $dir ($name + '.started')) -Value started -NoNewline; $deadline = (Get-Date).AddSeconds(5); while (-not (Test-Path -LiteralPath (Join-Path $dir ($other + '.started')))) {{ if ((Get-Date) -gt $deadline) {{ throw 'other check command did not start' }}; Start-Sleep -Milliseconds 50 }}"
    )
}

fn run_named_check(repo_root: &std::path::Path, name: &str, script: &str) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_lane"))
        .arg("--repo-root")
        .arg(repo_root)
        .args([
            "check",
            "merge-checks",
            "--name",
            name,
            "--",
            "pwsh",
            "-NoProfile",
            "-Command",
            script,
        ])
        .output()
        .unwrap()
}

fn run_human_command(repo: &TempRepo, command: &str) -> std::process::Output {
    let mut path_entries = vec![
        std::path::Path::new(env!("CARGO_BIN_EXE_lane"))
            .parent()
            .unwrap()
            .to_path_buf(),
    ];
    path_entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(path_entries).unwrap();
    std::process::Command::new("pwsh")
        .current_dir(repo.path())
        .env("PATH", path)
        .args(["-NoProfile", "-Command", command])
        .output()
        .unwrap()
}
