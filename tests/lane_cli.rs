#![cfg(windows)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[test]
fn cli_exec_runs_command_in_virtual_mount_and_promotes_output() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"export const mode = 'base';\n");

    let exec_result = repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; if ((Resolve-Path $env:LANE_REPO_ROOT).ProviderPath -ne (Get-Location).ProviderPath) { throw \"LANE_REPO_ROOT must be the mounted view\" }; if ((Resolve-Path $env:LANE_VIEW_ROOT).ProviderPath -ne (Get-Location).ProviderPath) { throw \"LANE_VIEW_ROOT must be the mounted view\" }; if ($env:LANE_STORAGE_PATH) { throw \"LANE_STORAGE_PATH leaked\" }; if ($env:LANE_EXEC_MODE -ne \"virtual_mount\") { throw \"expected virtual_mount mode\" }; Set-Content -Path src/example.ts -Value \"export const mode = 'agent-a';\" -NoNewline; Set-Content -Path src/created.ts -Value \"export const created = true;\" -NoNewline",
    ]);
    assert_eq!(exec_result["lane"], "agent-a");
    assert_eq!(exec_result["mode"], "virtual_mount");
    assert_eq!(exec_result["exit_code"], 0);
    assert_eq!(exec_result["worker_error"], Value::Null);
    assert!(exec_result["warnings"].as_array().unwrap().is_empty());
    assert!(exec_result["timings"]["storage_lock_held_ms"].is_u64());
    assert!(
        exec_result["projected_paths"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        string_array(&exec_result["changed_paths"]),
        vec!["src/created.ts", "src/example.ts"]
    );
    assert_eq!(change_statuses(&exec_result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected.insert("src/example.ts".to_owned(), "modified".to_owned());
        expected
    });

    assert_eq!(
        fs::read(repo.path().join("src/example.ts")).unwrap(),
        b"export const mode = 'base';\n"
    );
    assert!(!repo.path().join("src/created.ts").exists());
    assert!(repo.path().join(".lane/repo.json").exists());
    assert!(!repo.path().join(".lane/repo.lane").exists());
    assert!(repo.path().join(".lane/last_exec/agent-a.json").exists());
    assert!(
        fs::read_dir(repo.path().join(".lane/blobs/sha256"))
            .unwrap()
            .next()
            .is_some()
    );
    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());

    let review = repo.run_json(["review", "agent-a"]);
    assert_eq!(review_change_statuses(&review, "agent-a"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected.insert("src/example.ts".to_owned(), "modified".to_owned());
        expected
    });

    let diff = repo.run_text(["diff", "agent-a"]);
    assert!(diff.contains("--- base/src/example.ts"));
    assert!(diff.contains("+++ agent-a/src/example.ts"));
    assert!(diff.contains("-export const mode = 'base';"));
    assert!(diff.contains("+export const mode = 'agent-a';"));
    assert!(diff.contains("+++ agent-a/src/created.ts"));

    let example_ops = review_clean_op_ids(review_path(&review, "src/example.ts"));
    let promoted_file = run_promote_ops_json(&repo, "agent-a", "src/example.ts", &example_ops);
    assert_eq!(change_statuses_from_key(&promoted_file, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/example.ts".to_owned(), "modified".to_owned());
        expected
    });
    assert_eq!(
        fs::read(repo.path().join("src/example.ts")).unwrap(),
        b"export const mode = 'agent-a';"
    );
    assert!(!repo.path().join("src/created.ts").exists());

    let remaining = repo.run_json(["review", "agent-a"]);
    assert_eq!(review_change_statuses(&remaining, "agent-a"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected
    });

    let promoted_lane = repo.run_json(["promote-clean", "agent-a"]);
    assert_eq!(change_statuses_from_key(&promoted_lane, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected
    });
    assert_eq!(
        fs::read(repo.path().join("src/created.ts")).unwrap(),
        b"export const created = true;"
    );

    let empty = repo.run_json(["review", "agent-a"]);
    assert!(empty["paths"].as_array().unwrap().is_empty());

    let discarded = repo.run_json(["discard", "agent-a"]);
    assert_eq!(discarded["removed"], true);
    assert_eq!(discarded["discarded_changes"], 0);
}

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
fn cli_exec_projects_existing_lane_file_without_worker_changes() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"export const mode = 'base';\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path src/created.ts -Value \"export const created = true;\" -NoNewline",
    ]);

    let result = repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; if (-not (Test-Path -LiteralPath src/created.ts)) { throw \"missing projected lane file\" }",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["projected_paths"]),
        vec!["src/created.ts"]
    );
    assert!(result["changed_paths"].as_array().unwrap().is_empty());
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/created.ts").exists());
}

#[test]
fn cli_exec_projects_existing_lane_deletion_without_worker_changes() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"export const mode = 'base';\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath src/example.ts",
    ]);

    let result = repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; if (Test-Path -LiteralPath src/example.ts) { throw \"projected lane deletion still exists\" }",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["projected_paths"]),
        vec!["src/example.ts"]
    );
    assert!(result["changed_paths"].as_array().unwrap().is_empty());
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/example.ts".to_owned(), "deleted".to_owned());
        expected
    });
    assert_eq!(
        fs::read(repo.path().join("src/example.ts")).unwrap(),
        b"export const mode = 'base';\n"
    );
}

#[test]
fn cli_exec_deleting_lane_created_file_clears_overlay() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"export const mode = 'base';\n");

    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path src/created.ts -Value \"export const created = true;\" -NoNewline",
    ]);

    let result = repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath src/created.ts",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["projected_paths"]),
        vec!["src/created.ts"]
    );
    assert_eq!(
        string_array(&result["changed_paths"]),
        vec!["src/created.ts"]
    );
    assert!(result["changes"].as_array().unwrap().is_empty());
    assert!(!repo.path().join("src/created.ts").exists());
    assert!(
        repo.run_json(["review", "agent-a"])["paths"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

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
fn cli_parent_dogfood_flow_reviews_promotes_resolves_and_discards_worker_lanes() {
    let repo = TempRepo::new();
    repo.write(
        "src/app.ts",
        b"export const title = 'Base';\nexport const mode = 'stable';\n",
    );
    repo.write("README.md", b"# Lane\n\nBase docs.\n");

    let docs_clean = repo.run_json([
        "exec",
        "docs-clean",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('README.md', \"# Lane`n`nDogfood docs.`n\"); [IO.File]::WriteAllText('src/analytics.ts', \"export const analytics = true;`n\")",
    ]);
    assert_eq!(docs_clean["exit_code"], 0);
    assert_eq!(docs_clean["worker_error"], Value::Null);

    let title_loud = repo.run_json([
        "exec",
        "title-loud",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/app.ts', \"export const title = 'Loud';`nexport const mode = 'stable';`n\"); [IO.File]::WriteAllText('src/banner.ts', \"export const banner = 'selected';`n\")",
    ]);
    assert_eq!(title_loud["exit_code"], 0);
    assert_eq!(title_loud["worker_error"], Value::Null);

    let title_grid = repo.run_json([
        "exec",
        "title-grid",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [IO.File]::WriteAllText('src/app.ts', \"export const title = 'Grid';`nexport const mode = 'stable';`n\")",
    ]);
    assert_eq!(title_grid["exit_code"], 0);
    assert_eq!(title_grid["worker_error"], Value::Null);

    let scratch_build = repo.run_json([
        "exec",
        "scratch-build",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Path '.cache' -Force | Out-Null; [IO.File]::WriteAllText('.cache/agent-report.txt', \"noise`n\"); [IO.File]::WriteAllText('src/prototype.ts', \"export const prototype = true;`n\")",
    ]);
    assert_eq!(scratch_build["exit_code"], 0);
    assert_eq!(scratch_build["worker_error"], Value::Null);

    let failed_output = run_lane_exec(
        repo.path(),
        "failed-worker",
        "$ErrorActionPreference = \"Continue\"; New-Item -ItemType Directory -Path '.cache' -Force | Out-Null; [IO.File]::WriteAllText('.cache/failed.log', \"failed`n\"); [IO.File]::WriteAllText('src/partial.ts', \"export const partial = true;`n\"); Write-Error \"simulated dogfood failure\"; exit 9",
    );
    assert!(!failed_output.status.success());
    let failed_worker = output_json(&failed_output);
    assert_eq!(failed_worker["exit_code"], 9);
    assert_eq!(failed_worker["worker_error"], Value::Null);
    assert_eq!(
        string_array(&failed_worker["changed_paths"]),
        vec![".cache", ".cache/failed.log", "src/partial.ts"]
    );

    assert_eq!(
        fs::read(repo.path().join("src/app.ts")).unwrap(),
        b"export const title = 'Base';\nexport const mode = 'stable';\n"
    );
    assert!(!repo.path().join("src/banner.ts").exists());
    assert!(!repo.path().join("src/prototype.ts").exists());
    assert!(!repo.path().join("src/partial.ts").exists());

    let review = repo.run_json(["review"]);
    assert_eq!(review["lane"], Value::Null);
    assert_eq!(review["summary"]["lanes"], 5);
    assert_eq!(review["summary"]["changed_paths"], 8);
    assert_eq!(review["summary"]["clean_ops"], 7);
    assert_eq!(review["summary"]["conflicted_ops"], 2);
    assert_eq!(review["summary"]["conflict_groups"], 1);
    assert_eq!(review["lanes"].as_array().unwrap().len(), 5);
    assert_eq!(
        review_paths(&review),
        vec![
            ".cache/agent-report.txt",
            ".cache/failed.log",
            "README.md",
            "src/analytics.ts",
            "src/app.ts",
            "src/banner.ts",
            "src/partial.ts",
            "src/prototype.ts",
        ]
    );

    let failed_lane = review_lane(&review, "failed-worker");
    assert_eq!(failed_lane["changed_paths"], 2);
    assert_eq!(failed_lane["clean_ops"], 2);
    assert_eq!(failed_lane["conflicted_ops"], 0);
    assert_eq!(failed_lane["last_exec"]["exit_code"], 9);
    assert_eq!(failed_lane["last_exec"]["worker_error"], Value::Null);
    assert_eq!(
        string_array(&failed_lane["last_exec"]["changed_paths"]),
        vec![".cache", ".cache/failed.log", "src/partial.ts"]
    );
    assert!(
        failed_lane["last_exec"]["stderr"]["text"]
            .as_str()
            .unwrap()
            .contains("simulated dogfood failure")
    );
    assert_eq!(failed_lane["last_exec"]["stderr"]["truncated"], false);
    assert_eq!(
        review_action_kinds(&failed_lane["actions"]),
        vec!["promote_clean", "discard"]
    );
    assert_eq!(
        review_action_commands(&failed_lane["actions"]),
        vec![
            vec!["promote-clean", "failed-worker"],
            vec!["discard", "failed-worker"]
        ]
    );

    let selected_lane = review_lane(&review, "title-loud");
    assert_eq!(selected_lane["changed_paths"], 2);
    assert_eq!(selected_lane["clean_ops"], 1);
    assert_eq!(selected_lane["conflicted_ops"], 1);
    assert_eq!(selected_lane["last_exec"]["exit_code"], 0);
    assert_eq!(
        review_action_commands(&selected_lane["actions"]),
        vec![
            vec!["promote-clean", "title-loud"],
            vec!["discard", "title-loud"]
        ]
    );

    let app_review = review_path(&review, "src/app.ts");
    assert!(app_review["clean_ops"].as_array().unwrap().is_empty());
    let app_conflict = &app_review["conflicts"][0];
    assert_eq!(
        string_array(&app_conflict["lanes"]),
        vec!["title-grid", "title-loud"]
    );
    assert_eq!(
        review_op_ids(&app_conflict["ops"]),
        vec!["title-grid:1", "title-loud:1"]
    );
    assert_eq!(
        review_action_kinds(&app_conflict["actions"]),
        vec!["show_op", "resolve_op", "show_op", "resolve_op"]
    );
    assert_eq!(
        review_action_commands(&app_conflict["actions"]),
        vec![
            vec!["show-op", "title-grid", "src/app.ts", "title-grid:1"],
            vec![
                "resolve-op",
                "title-grid",
                "src/app.ts",
                "title-grid:1",
                "--with-file",
                "<replacement-file>"
            ],
            vec!["show-op", "title-loud", "src/app.ts", "title-loud:1"],
            vec![
                "resolve-op",
                "title-loud",
                "src/app.ts",
                "title-loud:1",
                "--with-file",
                "<replacement-file>"
            ],
        ]
    );
    assert_eq!(
        app_conflict["actions"][1]["required_inputs"][0]["name"],
        "with_file"
    );
    assert_eq!(
        app_conflict["actions"][1]["required_inputs"][0]["placeholder"],
        "<replacement-file>"
    );

    let failed_review = review_path(&review, "src/partial.ts");
    assert_eq!(failed_review["lanes"][0]["lane"], "failed-worker");
    assert_eq!(failed_review["lanes"][0]["status"], "created");
    assert_eq!(failed_review["lanes"][0]["clean_ops"], 1);

    let docs_promoted = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&review, "docs-clean")["actions"],
            "promote_clean",
            "docs-clean",
        ),
    );
    assert_eq!(change_statuses_from_key(&docs_promoted, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("README.md".to_owned(), "modified".to_owned());
        expected.insert("src/analytics.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(docs_promoted["conflicts"].as_array().unwrap().is_empty());

    let selected_clean = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&review, "title-loud")["actions"],
            "promote_clean",
            "title-loud",
        ),
    );
    assert_eq!(change_statuses_from_key(&selected_clean, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/banner.ts".to_owned(), "created".to_owned());
        expected
    });
    assert_eq!(change_statuses_from_key(&selected_clean, "conflicts"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/app.ts".to_owned(), "modified".to_owned());
        expected
    });

    let after_clean = repo.run_json(["review", "title-loud"]);
    assert_eq!(after_clean["summary"]["clean_ops"], 0);
    assert_eq!(after_clean["summary"]["conflicted_ops"], 1);
    assert_eq!(
        review_action_commands(&review_lane(&after_clean, "title-loud")["actions"]),
        vec![vec!["discard", "title-loud"]]
    );
    let after_clean_conflict = &review_path(&after_clean, "src/app.ts")["conflicts"][0];

    let shown = run_review_action_json(
        &repo,
        review_action(&after_clean_conflict["actions"], "show_op", "title-loud"),
    );
    let selected_op_id = shown["op"]["op_id"].as_str().unwrap();
    assert_eq!(shown["op"]["op_id"], selected_op_id);
    assert_eq!(shown["base"]["utf8"], "Base");
    assert_eq!(shown["inserted"]["utf8"], "Loud");
    assert_eq!(
        string_array(&shown["op"]["conflicts_with"]),
        vec!["title-grid"]
    );

    let resolution = repo.path().join("title-resolution.txt");
    fs::write(&resolution, b"Launch").unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(&after_clean_conflict["actions"], "resolve_op", "title-loud"),
        &resolution,
    );
    assert_eq!(resolved["replacement"]["utf8"], "Launch");
    assert!(resolved["remaining"].as_array().unwrap().is_empty());

    assert_eq!(
        fs::read(repo.path().join("src/app.ts")).unwrap(),
        b"export const title = 'Launch';\nexport const mode = 'stable';\n"
    );
    assert_eq!(
        fs::read(repo.path().join("src/banner.ts")).unwrap(),
        b"export const banner = 'selected';\n"
    );
    assert_eq!(
        fs::read(repo.path().join("src/analytics.ts")).unwrap(),
        b"export const analytics = true;\n"
    );
    assert!(!repo.path().join(".cache/agent-report.txt").exists());
    assert!(!repo.path().join(".cache/failed.log").exists());
    assert!(!repo.path().join("src/prototype.ts").exists());
    assert!(!repo.path().join("src/partial.ts").exists());

    for (lane, discarded_changes) in [
        ("docs-clean", 0),
        ("title-loud", 0),
        ("title-grid", 1),
        ("scratch-build", 2),
        ("failed-worker", 2),
    ] {
        let latest_review = repo.run_json(["review", lane]);
        let discarded = run_review_action_json(
            &repo,
            review_action(
                &review_lane(&latest_review, lane)["actions"],
                "discard",
                lane,
            ),
        );
        assert_eq!(discarded["removed"], true);
        assert_eq!(discarded["discarded_changes"], discarded_changes);
    }

    let final_review = repo.run_json(["review"]);
    assert_eq!(final_review["summary"]["lanes"], 0);
    assert_eq!(final_review["summary"]["changed_paths"], 0);
    assert!(final_review["lanes"].as_array().unwrap().is_empty());
    assert!(final_review["paths"].as_array().unwrap().is_empty());
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

#[test]
fn cli_exec_releases_storage_lock_while_worker_runs() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");
    let marker = std::env::temp_dir().join(format!(
        "lane-worker-started-{}-{}.txt",
        std::process::id(),
        unique_suffix()
    ));
    let marker_arg = ps_single_quoted_path(&marker);

    let root = repo.path().to_path_buf();
    let job = thread::spawn(move || {
        run_lane_exec(
            &root,
            "slow-worker",
            &format!(
                "$ErrorActionPreference = \"Stop\"; Set-Content -LiteralPath {marker_arg} -Value started -NoNewline; Start-Sleep -Milliseconds 1500; Set-Content -Path src/slow.ts -Value \"export const slow = true;\" -NoNewline"
            ),
        )
    });

    wait_for_path(&marker);
    let storage_command_start = Instant::now();
    let created = repo.run_json(["create", "observer"]);
    assert_eq!(created["created"], true);
    assert!(
        storage_command_start.elapsed() < Duration::from_millis(1000),
        "storage command waited for the virtual worker"
    );

    let output = assert_success(job.join().unwrap());
    let result = output_json(&output);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert!(result["timings"]["storage_lock_held_ms"].as_u64().unwrap() < 1000);

    let existing = repo.run_json(["create", "observer"]);
    assert_eq!(existing["created"], false);
    let _ = fs::remove_file(marker);
}

#[test]
fn cli_parent_can_orchestrate_five_parallel_lane_execs_directly() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");

    let variants = [
        ("login-minimal", "minimal"),
        ("login-enterprise", "enterprise"),
        ("login-playful", "playful"),
        ("login-split", "split"),
        ("login-focused", "focused"),
    ];
    let jobs = variants
        .iter()
        .map(|(lane, design)| {
            let root = repo.path().to_path_buf();
            let lane = (*lane).to_owned();
            let design = (*design).to_owned();
            thread::spawn(move || {
                let script = format!(
                    "$ErrorActionPreference = \"Stop\"; Start-Sleep -Milliseconds 250; Set-Content -Path src/login.tsx -Value \"export const design = '{}';\" -NoNewline; Set-Content -Path src/{}.tsx -Value \"export const marker = '{}';\" -NoNewline",
                    design, design, design
                );
                run_lane_exec(&root, &lane, &script)
            })
        })
        .collect::<Vec<_>>();

    let exec_outputs = jobs
        .into_iter()
        .map(|job| output_json(&assert_success(job.join().unwrap())))
        .collect::<Vec<_>>();
    assert_eq!(exec_outputs.len(), 5);
    for output in &exec_outputs {
        assert_eq!(output["exit_code"], 0);
        assert_eq!(output["worker_error"], Value::Null);
        assert_eq!(output["changes"].as_array().unwrap().len(), 2);
    }

    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'base';"
    );

    for (lane, design) in variants {
        assert_eq!(
            review_change_statuses(&repo.run_json(["review", lane]), lane),
            {
                let mut expected = BTreeMap::new();
                expected.insert(format!("src/{design}.tsx"), "created".to_owned());
                expected.insert("src/login.tsx".to_owned(), "modified".to_owned());
                expected
            }
        );
    }

    let enterprise_review = repo.run_json(["review", "login-enterprise"]);
    let promoted_clean = run_review_action_json(
        &repo,
        review_action(
            &review_lane(&enterprise_review, "login-enterprise")["actions"],
            "promote_clean",
            "login-enterprise",
        ),
    );
    assert_eq!(change_statuses_from_key(&promoted_clean, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/enterprise.tsx".to_owned(), "created".to_owned());
        expected
    });

    let conflict_review = repo.run_json(["review"]);
    let conflict_actions =
        &review_path(&conflict_review, "src/login.tsx")["conflicts"][0]["actions"];
    let shown = run_review_action_json(
        &repo,
        review_action(conflict_actions, "show_op", "login-enterprise"),
    );
    let resolution = repo.path().join("login-enterprise-resolution.txt");
    fs::write(
        &resolution,
        shown["inserted"]["utf8"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let resolved = run_review_action_with_replacement_json(
        &repo,
        review_action(conflict_actions, "resolve_op", "login-enterprise"),
        &resolution,
    );
    assert!(resolved["remaining"].as_array().unwrap().is_empty());

    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'enterprise';"
    );
    assert_eq!(
        fs::read(repo.path().join("src/enterprise.tsx")).unwrap(),
        b"export const marker = 'enterprise';"
    );
    assert!(!repo.path().join("src/minimal.tsx").exists());
    assert!(!repo.path().join("src/playful.tsx").exists());
    assert!(!repo.path().join("src/split.tsx").exists());
    assert!(!repo.path().join("src/focused.tsx").exists());

    for (lane, _) in variants {
        if lane != "login-enterprise" {
            let discarded = repo.run_json(["discard", lane]);
            assert_eq!(discarded["removed"], true);
        }
    }
}

#[test]
fn cli_exec_returns_json_for_child_failure() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");

    let output = run_lane_exec(
        repo.path(),
        "failing-lane",
        "$ErrorActionPreference = \"Continue\"; Write-Error \"simulated failure\"; exit 7",
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "failing-lane");
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["worker_error"], Value::Null);
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("simulated failure")
    );
    assert!(result["changes"].as_array().unwrap().is_empty());
}

#[test]
fn cli_exec_buffers_chunked_writes_until_worker_finishes() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");

    let result = repo.run_json([
        "exec",
        "chunked",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; $stream = [IO.File]::Open('src/big.bin', 'Create', 'Write', 'None'); try { $chunk = New-Object byte[] 4096; for ($i = 0; $i -lt 256; $i++) { $stream.Write($chunk, 0, $chunk.Length) } } finally { $stream.Close() }",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(string_array(&result["changed_paths"]), vec!["src/big.bin"]);
    assert_eq!(result["timings"]["storage_write_ops"], 3);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/big.bin".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/big.bin").exists());

    repo.run_json(["promote-clean", "chunked"]);
    assert_eq!(
        fs::metadata(repo.path().join("src/big.bin")).unwrap().len(),
        1 << 20
    );
}

#[test]
fn cli_exec_creates_nested_file_in_new_directory() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");

    let result = repo.run_json([
        "exec",
        "nested-create",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Path src/nested -Force | Out-Null; Set-Content -Path src/nested/created.ts -Value \"export const created = true;\" -NoNewline",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["changed_paths"]),
        vec!["src/nested", "src/nested/created.ts"]
    );
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/nested/created.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/nested").exists());

    repo.run_json(["promote-clean", "nested-create"]);
    assert_eq!(
        fs::read(repo.path().join("src/nested/created.ts")).unwrap(),
        b"export const created = true;"
    );
}

#[test]
fn cli_exec_replaces_file_with_directory_tree() {
    let repo = TempRepo::new();
    repo.write("src/swap", b"base file");

    let result = repo.run_json([
        "exec",
        "file-to-dir",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath src/swap; New-Item -ItemType Directory -Path src/swap -Force | Out-Null; Set-Content -Path src/swap/nested.txt -Value nested -NoNewline",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["changed_paths"]),
        vec!["src/swap", "src/swap/nested.txt"]
    );
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/swap".to_owned(), "deleted".to_owned());
        expected.insert("src/swap/nested.txt".to_owned(), "created".to_owned());
        expected
    });
    assert_eq!(
        fs::read(repo.path().join("src/swap")).unwrap(),
        b"base file"
    );

    repo.run_json(["promote-clean", "file-to-dir"]);
    assert!(repo.path().join("src/swap").is_dir());
    assert_eq!(
        fs::read(repo.path().join("src/swap/nested.txt")).unwrap(),
        b"nested"
    );
}

#[test]
fn cli_exec_replaces_directory_tree_with_file() {
    let repo = TempRepo::new();
    repo.write("src/swap/original.txt", b"original");

    let result = repo.run_json([
        "exec",
        "dir-to-file",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -Recurse -LiteralPath src/swap; Set-Content -Path src/swap -Value \"now a file\" -NoNewline",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["changed_paths"]),
        vec!["src/swap", "src/swap/original.txt"]
    );
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/swap".to_owned(), "created".to_owned());
        expected.insert("src/swap/original.txt".to_owned(), "deleted".to_owned());
        expected
    });
    assert!(repo.path().join("src/swap").is_dir());
    assert_eq!(
        fs::read(repo.path().join("src/swap/original.txt")).unwrap(),
        b"original"
    );

    repo.run_json(["promote-clean", "dir-to-file"]);
    assert!(repo.path().join("src/swap").is_file());
    assert_eq!(
        fs::read(repo.path().join("src/swap")).unwrap(),
        b"now a file"
    );
    assert!(!repo.path().join("src/swap/original.txt").exists());
}

#[test]
fn cli_exec_runs_agent_like_process_with_git_view_and_atomic_save() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';\n");
    repo.init_git_repo();

    let result = repo.run_json([
        "exec",
        "agent-realish",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; if ((git rev-parse --show-toplevel).TrimEnd('/') -ne (Get-Location).ProviderPath.TrimEnd('\\')) { throw \"git root must be the mounted lane view\" }; if ($env:GIT_OPTIONAL_LOCKS -ne \"0\") { throw \"git optional locks must be disabled in lane views\" }; pwsh -NoProfile -Command '$tmp = Join-Path (Get-Location) \"src/login.tsx.tmp\"; $target = Join-Path (Get-Location) \"src/login.tsx\"; Set-Content -LiteralPath $tmp -Value \"export const design = ''agent-realish'';\" -NoNewline; [IO.File]::Move($tmp, $target, $true)'; $diff = git diff -- src/login.tsx; if (-not ($diff -match \"agent-realish\")) { throw \"git diff did not see mounted lane changes\" }",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(result["mode"], "virtual_mount");
    assert_eq!(
        string_array(&result["changed_paths"]),
        vec!["src/login.tsx", "src/login.tsx.tmp"]
    );
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/login.tsx".to_owned(), "modified".to_owned());
        expected
    });
    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'base';\n"
    );
    assert!(!repo.path().join("src/login.tsx.tmp").exists());

    repo.run_json(["promote-clean", "agent-realish"]);
    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'agent-realish';"
    );
}

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
    assert!(
        doctor["report"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("is unreadable"))
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

#[test]
fn cli_exec_keeps_git_metadata_read_only_for_agent_processes() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;\n");
    repo.init_git_repo();

    let result = repo.run_json([
        "exec",
        "agent-git",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; git status --short | Out-Null; Set-Content -Path src/agent.ts -Value \"export const agent = true;\" -NoNewline",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(string_array(&result["changed_paths"]), vec!["src/agent.ts"]);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/agent.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/agent.ts").exists());
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

#[test]
fn cli_exec_resolves_agent_command_shims_from_path() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;\n");
    repo.write(
        "bin/fake-agent.cmd",
        b"@echo off\r\necho export const shim = true;> src\\shim.ts\r\n",
    );
    let path = format!(
        "{};{}",
        repo.path().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let result = repo.run_json_with_env(
        ["exec", "shim-agent", "--", "fake-agent"],
        [("PATH", path.as_str())],
    );

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(string_array(&result["changed_paths"]), vec!["src/shim.ts"]);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/shim.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/shim.ts").exists());
}

struct TempRepo {
    root: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "lane-cli-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn run_json<const N: usize>(&self, args: [&str; N]) -> Value {
        serde_json::from_slice(&self.run(args).stdout).unwrap()
    }

    fn run_json_with_env<const N: usize, const M: usize>(
        &self,
        args: [&str; N],
        envs: [(&str, &str); M],
    ) -> Value {
        serde_json::from_slice(
            &self
                .run_vec_with_env(args.into_iter().map(str::to_owned).collect(), envs)
                .stdout,
        )
        .unwrap()
    }

    fn run_text<const N: usize>(&self, args: [&str; N]) -> String {
        String::from_utf8(self.run(args).stdout).unwrap()
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Output {
        self.run_vec(args.into_iter().map(str::to_owned).collect())
    }

    fn run_vec(&self, args: Vec<String>) -> Output {
        self.run_vec_with_env(args, [])
    }

    fn run_unchecked(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_lane"))
            .arg("--repo-root")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap()
    }

    fn run_vec_with_env<const N: usize>(
        &self,
        args: Vec<String>,
        envs: [(&str, &str); N],
    ) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_lane"))
            .arg("--repo-root")
            .arg(&self.root)
            .args(args)
            .envs(envs)
            .output()
            .unwrap();
        if !output.status.success() {
            panic!(
                "lane command failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        output
    }

    fn init_git_repo(&self) {
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["init", "-q"]),
        );
        run_checked(Command::new("git").arg("-C").arg(&self.root).args([
            "config",
            "user.email",
            "lane@example.invalid",
        ]));
        run_checked(Command::new("git").arg("-C").arg(&self.root).args([
            "config",
            "user.name",
            "Lane Test",
        ]));
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["add", "."]),
        );
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(["commit", "-q", "-m", "base"]),
        );
    }
}

impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn repo_with_agent_exec() -> TempRepo {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;\n");
    repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Set-Content -Path src/agent.ts -Value \"export const agent = true;\" -NoNewline",
    ]);
    repo
}

fn first_blob_path(repo: &TempRepo) -> PathBuf {
    fs::read_dir(repo.path().join(".lane/blobs/sha256"))
        .unwrap()
        .next()
        .expect("test expected one blob file")
        .unwrap()
        .path()
}

fn run_lane_exec(repo_root: &Path, lane: &str, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lane"))
        .arg("--repo-root")
        .arg(repo_root)
        .args(["exec", lane, "--", "pwsh", "-NoProfile", "-Command", script])
        .output()
        .unwrap()
}

fn ps_single_quoted_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

fn wait_for_path(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn assert_success(output: Output) -> Output {
    if !output.status.success() {
        panic!(
            "lane command failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

fn output_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

fn run_checked(command: &mut Command) -> Vec<u8> {
    let output = command.output().unwrap();
    if !output.status.success() {
        panic!(
            "command failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output.stdout
}

fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

fn review_op_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["op"]["op_id"].as_str().unwrap().to_owned())
        .collect()
}

fn review_paths(review: &Value) -> Vec<&str> {
    review["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path["path"].as_str().unwrap())
        .collect()
}

fn review_path<'a>(review: &'a Value, path: &str) -> &'a Value {
    review["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap_or_else(|| panic!("missing review path {path}"))
}

fn review_change_statuses(review: &Value, lane: &str) -> BTreeMap<String, String> {
    review["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|path| {
            let lane_entry = path["lanes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["lane"] == lane)?;
            Some((
                path["path"].as_str().unwrap().to_owned(),
                lane_entry["status"].as_str().unwrap().to_owned(),
            ))
        })
        .collect()
}

fn review_clean_op_ids(path: &Value) -> Vec<String> {
    path["clean_ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"]["op_id"].as_str().unwrap().to_owned())
        .collect()
}

fn review_lane<'a>(review: &'a Value, lane: &str) -> &'a Value {
    review["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["lane"] == lane)
        .unwrap_or_else(|| panic!("missing review lane {lane}"))
}

fn review_action<'a>(actions: &'a Value, kind: &str, lane: &str) -> &'a Value {
    actions
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["kind"] == kind && action["lane"] == lane)
        .unwrap_or_else(|| panic!("missing review action {kind} for lane {lane}"))
}

fn run_review_action_json(repo: &TempRepo, action: &Value) -> Value {
    assert!(
        action["required_inputs"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "action requires inputs: {action}"
    );
    output_json(&repo.run_vec(review_action_command(action)))
}

fn run_promote_ops_json(repo: &TempRepo, lane: &str, path: &str, ops: &[String]) -> Value {
    let mut command = vec!["promote-ops".to_owned(), lane.to_owned(), path.to_owned()];
    command.extend(ops.iter().cloned());
    output_json(&repo.run_vec(command))
}

fn run_review_action_with_replacement_json(
    repo: &TempRepo,
    action: &Value,
    replacement: &Path,
) -> Value {
    let mut command = review_action_command(action);
    let replacement = replacement.display().to_string();
    for arg in &mut command {
        if arg == "<replacement-file>" {
            *arg = replacement.clone();
        }
    }
    assert!(
        !command.iter().any(|arg| arg == "<replacement-file>"),
        "replacement-file placeholder was not filled"
    );
    output_json(&repo.run_vec(command))
}

fn review_action_command(action: &Value) -> Vec<String> {
    string_array(&action["command"])
}

fn review_action_kinds(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["kind"].as_str().unwrap().to_owned())
        .collect()
}

fn review_action_commands(value: &Value) -> Vec<Vec<String>> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|action| string_array(&action["command"]))
        .collect()
}

fn change_statuses(value: &Value) -> BTreeMap<String, String> {
    change_statuses_from_key(value, "changes")
}

fn change_statuses_from_key(value: &Value, key: &str) -> BTreeMap<String, String> {
    value[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            (
                change["path"].as_str().unwrap().to_owned(),
                change["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
