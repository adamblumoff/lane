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

    let changes = repo.run_json(["changes", "agent-a", "--json"]);
    assert_eq!(change_statuses(&changes), {
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

    let promoted_file = repo.run_json(["promote", "agent-a", "src/example.ts", "--json"]);
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

    let remaining = repo.run_json(["changes", "agent-a", "--json"]);
    assert_eq!(change_statuses(&remaining), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected
    });

    let promoted_lane = repo.run_json(["promote-lane", "agent-a", "--json"]);
    assert_eq!(change_statuses_from_key(&promoted_lane, "promoted"), {
        let mut expected = BTreeMap::new();
        expected.insert("src/created.ts".to_owned(), "created".to_owned());
        expected
    });
    assert_eq!(
        fs::read(repo.path().join("src/created.ts")).unwrap(),
        b"export const created = true;"
    );

    let empty = repo.run_json(["changes", "agent-a", "--json"]);
    assert!(empty["changes"].as_array().unwrap().is_empty());

    let discarded = repo.run_json(["discard", "agent-a", "--json"]);
    assert_eq!(discarded["removed"], true);
    assert_eq!(discarded["discarded_changes"], 0);
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
        repo.run_json(["changes", "agent-a", "--json"])["changes"]
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
        change_statuses(&repo.run_json(["changes", "approach-a", "--json"])),
        {
            let mut expected = BTreeMap::new();
            expected.insert("src/a.ts".to_owned(), "created".to_owned());
            expected.insert("src/feature.ts".to_owned(), "modified".to_owned());
            expected
        }
    );
    assert_eq!(
        change_statuses(&repo.run_json(["changes", "approach-b", "--json"])),
        {
            let mut expected = BTreeMap::new();
            expected.insert("src/b.ts".to_owned(), "created".to_owned());
            expected.insert("src/feature.ts".to_owned(), "modified".to_owned());
            expected
        }
    );

    repo.run_json(["promote-lane", "approach-b", "--json"]);

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

    let changes = repo.run_json(["changes", "agent-a", "--json"]);
    let ops = changes["changes"][0]["ops"].as_array().unwrap();
    assert_eq!(ops.len(), 2);
    let op_id = ops[0]["op_id"].as_str().unwrap().to_owned();

    let promoted = repo.run_json([
        "promote-ops",
        "agent-a",
        "src/math.txt",
        "--json",
        op_id.as_str(),
    ]);
    assert_eq!(string_array(&promoted["promoted_ops"]), vec![op_id]);
    assert_eq!(
        fs::read(repo.path().join("src/math.txt")).unwrap(),
        b"alpha=10\nbeta=2\ngamma=3\n"
    );

    let remaining_a = repo.run_json(["changes", "agent-a", "--json"]);
    let remaining_a_ops = remaining_a["changes"][0]["ops"].as_array().unwrap();
    assert_eq!(remaining_a_ops.len(), 1);
    assert_eq!(remaining_a_ops[0]["op_id"], "agent-a:2");
    let remaining_b = repo.run_json(["changes", "agent-b", "--json"]);
    assert_eq!(change_statuses(&remaining_b), {
        let mut expected = BTreeMap::new();
        expected.insert("src/math.txt".to_owned(), "modified".to_owned());
        expected
    });

    repo.run_json(["promote-lane", "agent-b", "--json"]);
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

    let conflicts = repo.run_json(["conflicts", "agent-a", "--json"]);
    assert_eq!(conflicts["conflicts"].as_array().unwrap().len(), 1);
    let conflict_ops = conflicts["conflicts"][0]["ops"].as_array().unwrap();
    assert_eq!(conflict_ops.len(), 1);
    assert_eq!(conflict_ops[0]["op_id"], "agent-a:2");
    assert_eq!(
        string_array(&conflict_ops[0]["conflicts_with"]),
        vec!["agent-b"]
    );

    let promoted = repo.run_json(["promote-clean", "agent-a", "--json"]);
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

    let remaining_a = repo.run_json(["changes", "agent-a", "--json"]);
    let remaining_a_ops = remaining_a["changes"][0]["ops"].as_array().unwrap();
    assert_eq!(remaining_a_ops.len(), 1);
    assert_eq!(remaining_a_ops[0]["op_id"], "agent-a:2");
    assert_eq!(
        string_array(&remaining_a_ops[0]["conflicts_with"]),
        vec!["agent-b"]
    );

    repo.run_json(["promote-lane", "agent-b", "--json"]);
    assert_eq!(
        fs::read(repo.path().join("src/vars.txt")).unwrap(),
        b"a=A\nb=X\nc=C\n"
    );
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
    let created = repo.run_json(["create", "observer", "--json"]);
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

    let existing = repo.run_json(["create", "observer", "--json"]);
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
            change_statuses(&repo.run_json(["changes", lane, "--json"])),
            {
                let mut expected = BTreeMap::new();
                expected.insert(format!("src/{design}.tsx"), "created".to_owned());
                expected.insert("src/login.tsx".to_owned(), "modified".to_owned());
                expected
            }
        );
    }

    repo.run_json(["promote-lane", "login-enterprise", "--json"]);
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
            let discarded = repo.run_json(["discard", lane, "--json"]);
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
    assert_eq!(result["timings"]["storage_write_ops"], 2);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/big.bin".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/big.bin").exists());

    repo.run_json(["promote-lane", "chunked", "--json"]);
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

    repo.run_json(["promote-lane", "nested-create", "--json"]);
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

    repo.run_json(["promote-lane", "file-to-dir", "--json"]);
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

    repo.run_json(["promote-lane", "dir-to-file", "--json"]);
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

    repo.run_json(["promote-lane", "agent-realish", "--json"]);
    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'agent-realish';"
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
        String::from_utf8_lossy(&output.stderr).contains("invalid lane storage"),
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
