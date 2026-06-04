use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[test]
fn cli_exec_runs_command_in_lane_view_and_promotes_output() {
    let repo = TempRepo::new();
    repo.write("src/example.ts", b"export const mode = 'base';\n");

    let exec_result = repo.run_json([
        "exec",
        "agent-a",
        "--",
        "pwsh",
        "-NoProfile",
        "-Command",
        "$ErrorActionPreference = \"Stop\"; if ((Resolve-Path $env:LANE_REPO_ROOT).Path -ne (Get-Location).ProviderPath) { throw \"LANE_REPO_ROOT must be the lane view\" }; if ($env:LANE_STORAGE_PATH) { throw \"LANE_STORAGE_PATH leaked\" }; Set-Content -Path src/example.ts -Value \"export const mode = 'agent-a';\" -NoNewline; Set-Content -Path src/created.ts -Value \"export const created = true;\" -NoNewline",
    ]);
    assert_eq!(exec_result["lane"], "agent-a");
    assert_eq!(exec_result["exit_code"], 0);
    assert_eq!(exec_result["escaped"], false);
    assert_eq!(exec_result["rolled_back"], false);
    assert!(exec_result["escaped_paths"].as_array().unwrap().is_empty());
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
    assert_eq!(exec_a["escaped"], false);
    assert_eq!(exec_b["escaped"], false);

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
        assert_eq!(output["escaped"], false);
        assert_eq!(output["rolled_back"], false);
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
    assert_eq!(result["escaped"], false);
    assert_eq!(result["rolled_back"], false);
    assert!(
        result["stderr"]
            .as_str()
            .unwrap()
            .contains("simulated failure")
    );
    assert!(result["changes"].as_array().unwrap().is_empty());
}

#[test]
fn cli_exec_rolls_back_absolute_modified_file_escape() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");
    let original_path = ps_single_quoted_path(&repo.path().join("src/login.tsx"));

    let output = run_lane_exec(
        repo.path(),
        "escape-attempt",
        &format!(
            "$ErrorActionPreference = \"Stop\"; Set-Content -LiteralPath {original_path} -Value \"export const design = 'escaped';\" -NoNewline"
        ),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-attempt");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(result["rollback_error"], Value::Null);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/login.tsx"]
    );
    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'base';"
    );
    assert!(change_statuses(&repo.run_json(["changes", "escape-attempt", "--json"])).is_empty());
}

#[test]
fn cli_exec_rolls_back_absolute_created_file_escape() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");
    let original_path = ps_single_quoted_path(&repo.path().join("src/nested/escaped.ts"));
    let original_parent = ps_single_quoted_path(&repo.path().join("src/nested"));

    let output = run_lane_exec(
        repo.path(),
        "escape-create",
        &format!(
            "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Path {original_parent} -Force | Out-Null; Set-Content -LiteralPath {original_path} -Value \"export const escaped = true;\" -NoNewline"
        ),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-create");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/nested", "src/nested/escaped.ts"]
    );
    assert!(!repo.path().join("src/nested/escaped.ts").exists());
    assert!(!repo.path().join("src/nested").exists());
    assert!(change_statuses(&repo.run_json(["changes", "escape-create", "--json"])).is_empty());
}

#[test]
fn cli_exec_rolls_back_absolute_deleted_file_escape() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");
    let original_path = ps_single_quoted_path(&repo.path().join("src/login.tsx"));

    let output = run_lane_exec(
        repo.path(),
        "escape-delete",
        &format!("$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath {original_path}"),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-delete");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/login.tsx"]
    );
    assert_eq!(
        fs::read(repo.path().join("src/login.tsx")).unwrap(),
        b"export const design = 'base';"
    );
    assert!(change_statuses(&repo.run_json(["changes", "escape-delete", "--json"])).is_empty());
}

#[test]
fn cli_exec_rolls_back_absolute_created_empty_dir_escape() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");
    let original_dir = ps_single_quoted_path(&repo.path().join("src/empty-real-dir"));

    let output = run_lane_exec(
        repo.path(),
        "escape-empty-dir-create",
        &format!(
            "$ErrorActionPreference = \"Stop\"; New-Item -ItemType Directory -Path {original_dir} -Force | Out-Null"
        ),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-empty-dir-create");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/empty-real-dir"]
    );
    assert!(!repo.path().join("src/empty-real-dir").exists());
}

#[test]
fn cli_exec_rolls_back_absolute_deleted_empty_dir_escape() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");
    fs::create_dir_all(repo.path().join("src/empty-real-dir")).unwrap();
    let original_dir = ps_single_quoted_path(&repo.path().join("src/empty-real-dir"));

    let output = run_lane_exec(
        repo.path(),
        "escape-empty-dir-delete",
        &format!("$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath {original_dir}"),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-empty-dir-delete");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/empty-real-dir"]
    );
    assert!(repo.path().join("src/empty-real-dir").is_dir());
}

#[test]
fn cli_exec_rolls_back_absolute_file_replaced_by_dir_escape() {
    let repo = TempRepo::new();
    repo.write("src/swap", b"base file");
    let original_path = ps_single_quoted_path(&repo.path().join("src/swap"));
    let nested_path = ps_single_quoted_path(&repo.path().join("src/swap/nested.txt"));

    let output = run_lane_exec(
        repo.path(),
        "escape-file-to-dir",
        &format!(
            "$ErrorActionPreference = \"Stop\"; Remove-Item -LiteralPath {original_path}; New-Item -ItemType Directory -Path {original_path} -Force | Out-Null; Set-Content -LiteralPath {nested_path} -Value nested -NoNewline"
        ),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-file-to-dir");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/swap", "src/swap/nested.txt"]
    );
    assert_eq!(
        fs::read(repo.path().join("src/swap")).unwrap(),
        b"base file"
    );
    assert!(repo.path().join("src/swap").is_file());
}

#[test]
fn cli_exec_rolls_back_absolute_dir_replaced_by_file_escape() {
    let repo = TempRepo::new();
    repo.write("src/login.tsx", b"export const design = 'base';");
    fs::create_dir_all(repo.path().join("src/swap")).unwrap();
    repo.write("src/swap/original.txt", b"original");
    let original_path = ps_single_quoted_path(&repo.path().join("src/swap"));

    let output = run_lane_exec(
        repo.path(),
        "escape-dir-to-file",
        &format!(
            "$ErrorActionPreference = \"Stop\"; Remove-Item -Recurse -LiteralPath {original_path}; Set-Content -LiteralPath {original_path} -Value \"now a file\" -NoNewline"
        ),
    );

    assert!(!output.status.success());
    assert!(output.stderr.is_empty());
    let result = output_json(&output);
    assert_eq!(result["lane"], "escape-dir-to-file");
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["escaped"], true);
    assert_eq!(result["rolled_back"], true);
    assert_eq!(
        string_array(&result["escaped_paths"]),
        vec!["src/swap", "src/swap/original.txt"]
    );
    assert!(repo.path().join("src/swap").is_dir());
    assert_eq!(
        fs::read(repo.path().join("src/swap/original.txt")).unwrap(),
        b"original"
    );
}

struct TempRepo {
    root: PathBuf,
}

impl TempRepo {
    fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("lane-cli-test-{}-{suffix}", std::process::id()));
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

    fn run_text<const N: usize>(&self, args: [&str; N]) -> String {
        String::from_utf8(self.run(args).stdout).unwrap()
    }

    fn run<const N: usize>(&self, args: [&str; N]) -> Output {
        self.run_vec(args.into_iter().map(str::to_owned).collect())
    }

    fn run_vec(&self, args: Vec<String>) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_lane"))
            .arg("--repo-root")
            .arg(&self.root)
            .args(args)
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
