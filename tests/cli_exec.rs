#![cfg(windows)]

mod common;

use common::*;

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
    assert_exec_contract(&exec_result);
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
fn cli_exec_parent_relative_escape_stays_inside_virtual_lane_view() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;\n");
    let escaped_name = format!("escaped-by-parent-{}.txt", unique_suffix());
    let parent_candidate = repo.path().parent().unwrap().join(&escaped_name);
    assert!(!parent_candidate.exists());

    let result = output_json(&repo.run_vec(vec![
        "exec".to_owned(),
        "escape-check".to_owned(),
        "--".to_owned(),
        "pwsh".to_owned(),
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        format!(
            "$ErrorActionPreference = \"Stop\"; Set-Content -LiteralPath ..\\{escaped_name} -Value virtualized -NoNewline; if (-not (Test-Path -LiteralPath .\\{escaped_name})) {{ throw \"parent-relative write was not projected at the mount root\" }}"
        ),
    ]));

    assert_exec_contract(&result);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(
        string_array(&result["changed_paths"]),
        vec![escaped_name.clone()]
    );
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert(escaped_name.clone(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join(&escaped_name).exists());
    assert!(!parent_candidate.exists());

    repo.run_json(["promote-clean", "escape-check"]);
    assert_eq!(
        fs::read(repo.path().join(&escaped_name)).unwrap(),
        b"virtualized"
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
fn cli_exec_observe_streams_worker_output_to_stderr_and_preserves_json_stdout() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");

    let output = repo.run([
        "exec",
        "observed",
        "--observe",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; [Console]::Out.WriteLine('child out'); [Console]::Error.WriteLine('child err'); Set-Content -Path src/observed.ts -Value \"export const observed = true;\" -NoNewline",
    ]);

    assert!(output.status.success());
    let result = output_json(&output);
    assert_exec_contract(&result);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["stdout"], "child out\r\n");
    assert_eq!(result["stderr"], "child err\r\n");
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/observed.ts".to_owned(), "created".to_owned());
        expected
    });

    let observed = String::from_utf8(output.stderr).unwrap();
    assert!(observed.contains("[lane exec observed +"));
    assert!(observed.contains("starting worker: pwsh"));
    assert!(observed.contains("[lane exec observed stdout] child out"));
    assert!(observed.contains("[lane exec observed stderr] child err"));
    assert!(observed.contains("storage done"));
}

#[test]
fn cli_exec_parallel_repeated_blob_writes_do_not_fail() {
    let repo = TempRepo::new();
    repo.write("src/base.ts", b"export const base = true;");
    let root = repo.path().to_path_buf();

    let jobs = ["a", "b", "c"]
        .into_iter()
        .map(|name| {
            let root = root.clone();
            let lane = format!("parallel-{name}");
            let byte = name.as_bytes()[0];
            thread::spawn(move || {
                let script = format!(
                    "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Force -Path stress | Out-Null; $bytes = New-Object byte[] 4096; for ($j = 0; $j -lt $bytes.Length; $j++) {{ $bytes[$j] = {byte} }}; for ($i = 0; $i -lt 80; $i++) {{ [IO.File]::WriteAllBytes(('stress/{name}-{{0:D3}}.bin' -f $i), $bytes) }}"
                );
                run_lane_exec(&root, &lane, &script)
            })
        })
        .collect::<Vec<_>>();

    for job in jobs {
        let output = assert_success(job.join().unwrap());
        let result = output_json(&output);
        assert_exec_contract(&result);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["worker_error"], Value::Null);
        assert_eq!(result["changes"].as_array().unwrap().len(), 80);
    }

    let review = repo.run_json(["review"]);
    assert_eq!(review["summary"]["changed_paths"], 240);
    assert_eq!(review["summary"]["clean_ops"], 240);
    assert_eq!(review["summary"]["conflicted_ops"], 0);
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
    assert_exec_contract(&result);
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
fn cli_exec_recursive_directory_delete_hides_descendants_immediately() {
    let repo = TempRepo::new();
    repo.write("src/tree/nested/original.txt", b"original");

    let result = repo.run_json([
        "exec",
        "delete-tree",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -Recurse -LiteralPath src/tree; if (Test-Path -LiteralPath src/tree/nested) { throw \"deleted nested directory stayed visible\" }; if (Test-Path -LiteralPath src/tree/nested/original.txt) { throw \"deleted nested file stayed visible\" }",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert(
            "src/tree/nested/original.txt".to_owned(),
            "deleted".to_owned(),
        );
        expected
    });
    assert!(repo.path().join("src/tree/nested/original.txt").exists());

    repo.run_json(["promote-clean", "delete-tree"]);
    assert!(!repo.path().join("src/tree/nested/original.txt").exists());
}

#[test]
fn cli_exec_recursive_directory_delete_allows_recreated_subtree_in_same_session() {
    let repo = TempRepo::new();
    repo.write("src/tree/nested/original.txt", b"original");
    repo.write("src/tree/other.txt", b"other");

    let result = repo.run_json([
        "exec",
        "recreate-tree",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; Remove-Item -Recurse -LiteralPath src/tree; if (Test-Path -LiteralPath src/tree/nested/original.txt) { throw \"deleted nested file stayed visible\" }; New-Item -ItemType Directory -Force -Path src/tree/reborn | Out-Null; Set-Content -LiteralPath src/tree/reborn/fresh.txt -Value fresh -NoNewline; if (Test-Path -LiteralPath src/tree/other.txt) { throw \"deleted sibling file stayed visible\" }; if (-not (Test-Path -LiteralPath src/tree/reborn/fresh.txt)) { throw \"recreated file was hidden by parent tombstone\" }",
    ]);

    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert(
            "src/tree/nested/original.txt".to_owned(),
            "deleted".to_owned(),
        );
        expected.insert("src/tree/other.txt".to_owned(), "deleted".to_owned());
        expected.insert("src/tree/reborn/fresh.txt".to_owned(), "created".to_owned());
        expected
    });

    repo.run_json(["promote-clean", "recreate-tree"]);
    assert!(!repo.path().join("src/tree/nested/original.txt").exists());
    assert!(!repo.path().join("src/tree/other.txt").exists());
    assert_eq!(
        fs::read(repo.path().join("src/tree/reborn/fresh.txt")).unwrap(),
        b"fresh"
    );
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
        r#"
$ErrorActionPreference = "Stop"
if ((git rev-parse --show-toplevel).TrimEnd('/') -ne (Get-Location).ProviderPath.TrimEnd('\')) {
    throw "git root must be the mounted lane view"
}
if ((git rev-parse --is-inside-work-tree).Trim() -ne "true") {
    throw "git must see the mounted lane view as a work tree"
}
if ($env:GIT_OPTIONAL_LOCKS -ne "0") {
    throw "git optional locks must be disabled in lane views"
}
pwsh -NoProfile -Command '$tmp = Join-Path (Get-Location) "src/login.tsx.tmp"; $target = Join-Path (Get-Location) "src/login.tsx"; Set-Content -LiteralPath $tmp -Value "export const design = ''agent-realish'';" -NoNewline; [IO.File]::Move($tmp, $target, $true)'
$status = git status --short
if (-not ($status -match "M src/login.tsx")) {
    throw "git status did not see mounted lane changes"
}
"#,
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
fn cli_exec_keeps_lane_and_git_metadata_private_for_agent_processes() {
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
        "$ErrorActionPreference = \"Stop\"; git status --short | Out-Null; $rootNames = @(Get-ChildItem -Force -Name .); foreach ($metadataName in @('.lane', '.git')) { if ($rootNames -contains $metadataName) { throw \"metadata entry unexpectedly visible: $metadataName\" } }; foreach ($path in @('.lane/agent-owned.json', '.LANE/agent-owned.json', '.git/index.lock', '.GIT/index.lock')) { $wrote = $false; try { Set-Content -LiteralPath $path -Value nope -NoNewline -ErrorAction Stop; $wrote = $true } catch { } if ($wrote) { throw \"metadata write unexpectedly succeeded: $path\" } }; Set-Content -Path src/agent.ts -Value \"export const agent = true;\" -NoNewline",
    ]);

    assert_exec_contract(&result);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["worker_error"], Value::Null);
    assert_eq!(string_array(&result["changed_paths"]), vec!["src/agent.ts"]);
    assert_eq!(change_statuses(&result), {
        let mut expected = BTreeMap::new();
        expected.insert("src/agent.ts".to_owned(), "created".to_owned());
        expected
    });
    assert!(!repo.path().join("src/agent.ts").exists());
    assert!(!repo.path().join(".lane/agent-owned.json").exists());
    assert!(!repo.path().join(".git/index.lock").exists());
    let doctor = repo.run_json(["doctor"]);
    assert_eq!(doctor["healthy"], true);
    assert!(doctor["report"]["errors"].as_array().unwrap().is_empty());
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
