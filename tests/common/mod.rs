use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) use serde_json::Value;
pub(crate) use std::collections::BTreeMap;
pub(crate) use std::fs;
pub(crate) use std::thread;
pub(crate) use std::time::{Duration, Instant};

#[allow(dead_code)]
static NEXT_UNIQUE_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[allow(dead_code)]
pub(crate) struct TempRepo {
    root: PathBuf,
}

#[allow(dead_code)]
impl TempRepo {
    pub(crate) fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "lane-cli-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    pub(crate) fn run_json<const N: usize>(&self, args: [&str; N]) -> Value {
        serde_json::from_slice(&self.run(args).stdout).unwrap()
    }

    pub(crate) fn run_json_with_env<const N: usize, const M: usize>(
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

    pub(crate) fn run_text<const N: usize>(&self, args: [&str; N]) -> String {
        String::from_utf8(self.run(args).stdout).unwrap()
    }

    pub(crate) fn run<const N: usize>(&self, args: [&str; N]) -> Output {
        self.run_vec(args.into_iter().map(str::to_owned).collect())
    }

    pub(crate) fn run_vec(&self, args: Vec<String>) -> Output {
        self.run_vec_with_env(args, [])
    }

    pub(crate) fn run_unchecked(&self, args: &[&str]) -> Output {
        self.run_vec_unchecked(args.iter().map(|arg| (*arg).to_owned()).collect())
    }

    pub(crate) fn run_vec_unchecked(&self, args: Vec<String>) -> Output {
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

    pub(crate) fn init_git_repo(&self) {
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

#[allow(dead_code)]
impl Drop for TempRepo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[allow(dead_code)]
pub(crate) fn repo_with_agent_exec() -> TempRepo {
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

#[allow(dead_code)]
pub(crate) fn first_blob_path(repo: &TempRepo) -> PathBuf {
    fs::read_dir(repo.path().join(".lane/blobs/sha256"))
        .unwrap()
        .next()
        .expect("test expected one blob file")
        .unwrap()
        .path()
}

#[allow(dead_code)]
pub(crate) fn run_lane_exec(repo_root: &Path, lane: &str, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lane"))
        .arg("--repo-root")
        .arg(repo_root)
        .args(["exec", lane, "--", "pwsh", "-NoProfile", "-Command", script])
        .output()
        .unwrap()
}

#[allow(dead_code)]
pub(crate) fn ps_single_quoted_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

#[allow(dead_code)]
pub(crate) fn wait_for_path(path: &Path) {
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

#[allow(dead_code)]
pub(crate) fn assert_success(output: Output) -> Output {
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

#[allow(dead_code)]
pub(crate) fn output_json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[allow(dead_code)]
pub(crate) fn assert_command_fails_with(output: &Output, message: &str) {
    assert!(
        !output.status.success(),
        "lane command unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "failing lane command should not emit JSON stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "expected stderr to contain {message:?}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[allow(dead_code)]
pub(crate) fn assert_exec_contract(output: &Value) {
    assert!(
        output["lane"].is_string(),
        "lane must be a string: {output}"
    );
    assert!(
        output["repo_root"].is_string(),
        "repo_root must be a string: {output}"
    );
    assert!(
        output["storage_path"].is_string(),
        "storage_path must be a string: {output}"
    );
    assert!(
        output["workspace_root"].is_string(),
        "workspace_root must be a string: {output}"
    );
    assert!(
        output["mount_path"].is_string(),
        "mount_path must be a string: {output}"
    );
    assert_eq!(output["workspace_root"], output["mount_path"]);
    assert_eq!(output["mode"], "virtual_mount");
    assert!(
        output["projected_paths"].is_array(),
        "projected_paths must be an array: {output}"
    );
    assert!(
        output["exit_code"].is_i64() || output["exit_code"].is_null(),
        "exit_code must be an integer or null: {output}"
    );
    assert!(
        output["stdout"].is_string(),
        "stdout must be a string: {output}"
    );
    assert!(
        output["stderr"].is_string(),
        "stderr must be a string: {output}"
    );
    assert!(
        output["worker_error"].is_string() || output["worker_error"].is_null(),
        "worker_error must be a string or null: {output}"
    );
    assert!(
        output["changed_paths"].is_array(),
        "changed_paths must be an array: {output}"
    );
    assert!(
        output["changes"].is_array(),
        "changes must be an array: {output}"
    );
    assert!(
        output["warnings"].is_array(),
        "warnings must be an array: {output}"
    );

    let timings = &output["timings"];
    assert!(timings.is_object(), "timings must be an object: {output}");
    for key in [
        "total_ms",
        "lock_wait_ms",
        "lock_held_ms",
        "storage_lock_wait_ms",
        "storage_lock_held_ms",
        "pre_worker_lock_ms",
        "worker_ms",
        "post_worker_lock_ms",
        "mount_ms",
        "unmount_ms",
        "storage_write_ops",
    ] {
        assert!(
            timings[key].is_u64(),
            "timing {key} must be an unsigned integer: {output}"
        );
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
pub(crate) fn string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

#[allow(dead_code)]
pub(crate) fn review_op_ids(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["op"]["op_id"].as_str().unwrap().to_owned())
        .collect()
}

#[allow(dead_code)]
pub(crate) fn review_paths(review: &Value) -> Vec<&str> {
    review["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path["path"].as_str().unwrap())
        .collect()
}

#[allow(dead_code)]
pub(crate) fn review_path<'a>(review: &'a Value, path: &str) -> &'a Value {
    review["paths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap_or_else(|| panic!("missing review path {path}"))
}

#[allow(dead_code)]
pub(crate) fn review_change_statuses(review: &Value, lane: &str) -> BTreeMap<String, String> {
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

#[allow(dead_code)]
pub(crate) fn review_clean_op_ids(path: &Value) -> Vec<String> {
    path["clean_ops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| op["op"]["op_id"].as_str().unwrap().to_owned())
        .collect()
}

#[allow(dead_code)]
pub(crate) fn review_lane<'a>(review: &'a Value, lane: &str) -> &'a Value {
    review["lanes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["lane"] == lane)
        .unwrap_or_else(|| panic!("missing review lane {lane}"))
}

#[allow(dead_code)]
pub(crate) fn review_action<'a>(actions: &'a Value, kind: &str, lane: &str) -> &'a Value {
    actions
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["kind"] == kind && action["lane"] == lane)
        .unwrap_or_else(|| panic!("missing review action {kind} for lane {lane}"))
}

#[allow(dead_code)]
pub(crate) fn run_review_action_json(repo: &TempRepo, action: &Value) -> Value {
    assert!(
        action["required_inputs"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "action requires inputs: {action}"
    );
    output_json(&repo.run_vec(review_action_command(action)))
}

#[allow(dead_code)]
pub(crate) fn run_promote_ops_json(
    repo: &TempRepo,
    lane: &str,
    path: &str,
    ops: &[String],
) -> Value {
    let mut command = vec!["promote-ops".to_owned(), lane.to_owned(), path.to_owned()];
    command.extend(ops.iter().cloned());
    output_json(&repo.run_vec(command))
}

#[allow(dead_code)]
pub(crate) fn run_review_action_with_replacement_json(
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

#[allow(dead_code)]
pub(crate) fn review_action_command(action: &Value) -> Vec<String> {
    string_array(&action["command"])
}

#[allow(dead_code)]
pub(crate) fn review_action_kinds(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[allow(dead_code)]
pub(crate) fn review_action_commands(value: &Value) -> Vec<Vec<String>> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|action| string_array(&action["command"]))
        .collect()
}

#[allow(dead_code)]
pub(crate) fn change_statuses(value: &Value) -> BTreeMap<String, String> {
    change_statuses_from_key(value, "changes")
}

#[allow(dead_code)]
pub(crate) fn change_statuses_from_key(value: &Value, key: &str) -> BTreeMap<String, String> {
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

#[allow(dead_code)]
pub(crate) fn unique_suffix() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let sequence = NEXT_UNIQUE_SUFFIX.fetch_add(1, Ordering::Relaxed);
    format!("{timestamp}-{sequence}")
}
