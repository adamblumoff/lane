#![allow(dead_code)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

pub const BASE_FILES: usize = 200;
pub const DEFAULT_DETERMINISTIC_ROUNDS: usize = 5;
pub const DEFAULT_REAL_AGENT_ROUNDS: usize = 5;
pub const WINNER: usize = 2;
pub const VARIANTS: [Variant; 5] = [
    Variant {
        name: "alpha",
        value: "alpha-bench",
    },
    Variant {
        name: "beta",
        value: "beta-bench",
    },
    Variant {
        name: "gamma",
        value: "gamma-bench",
    },
    Variant {
        name: "delta",
        value: "delta-bench",
    },
    Variant {
        name: "epsilon",
        value: "epsilon-bench",
    },
];

#[derive(Clone, Copy)]
pub struct Variant {
    pub name: &'static str,
    pub value: &'static str,
}

#[derive(Clone)]
pub enum WorkerKind {
    Scripted,
    Codex { program: PathBuf },
}

#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    pub mode: &'static str,
    pub rounds: usize,
    pub variants: usize,
    pub base_files: usize,
    pub winner: &'static str,
    pub agent: Option<AgentInfo>,
    pub summaries: BenchmarkSummaries,
    pub gain: GainSummary,
    pub round_results: Vec<BenchmarkRound>,
}

#[derive(Debug, Serialize)]
pub struct AgentInfo {
    pub program: String,
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkSummaries {
    pub git_worktree: FlowSummary,
    pub lane: FlowSummary,
}

#[derive(Debug, Serialize)]
pub struct GainSummary {
    pub lane_total_ms_win_count: usize,
    pub lane_worker_ms_win_count: usize,
    pub median_total_ms_delta: i64,
    pub median_worker_ms_delta: i64,
    pub median_total_ms_speedup: Value,
    pub median_worker_ms_speedup: Value,
    pub median_active_bytes_delta: i64,
    pub median_active_files_delta: i64,
    pub median_active_dirs_delta: i64,
    pub median_active_bytes_ratio: Value,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkRound {
    pub round: usize,
    pub order: Vec<&'static str>,
    pub git_worktree: FlowMetrics,
    pub lane: FlowMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct FlowMetrics {
    pub repo_dirs: u64,
    pub setup_ms: u64,
    pub worker_ms: u64,
    pub compare_ms: u64,
    pub promote_ms: u64,
    pub cleanup_ms: u64,
    pub total_ms: u64,
    pub active_fs: FsMetrics,
    pub base_stable_before_promote: bool,
    pub promoted_shared: String,
    pub only_winner_marker_promoted: bool,
    pub worker_failures: Vec<WorkerFailure>,
    pub lane_exec: Option<LaneExecTotals>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkerFailure {
    pub variant: String,
    pub process_exit: Option<i32>,
    pub worker_exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LaneExecTotals {
    pub total_ms: u64,
    pub worker_ms: u64,
    pub storage_lock_wait_ms: u64,
    pub storage_lock_held_ms: u64,
    pub mount_ms: u64,
    pub unmount_ms: u64,
    pub storage_write_ops: u64,
}

#[derive(Debug, Serialize)]
pub struct FlowSummary {
    pub median_total_ms: u64,
    pub average_total_ms: f64,
    pub min_total_ms: u64,
    pub max_total_ms: u64,
    pub median_worker_ms: u64,
    pub average_worker_ms: f64,
    pub median_setup_ms: u64,
    pub median_compare_ms: u64,
    pub median_promote_ms: u64,
    pub median_cleanup_ms: u64,
    pub active_fs: FsSummary,
    pub all_workers_succeeded: bool,
    pub all_base_stable_before_promote: bool,
    pub all_only_winner_marker_promoted: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FsMetrics {
    pub total_bytes: u64,
    pub content_bytes: u64,
    pub git_bytes: u64,
    pub lane_bytes: u64,
    pub file_count: u64,
    pub dir_count: u64,
}

#[derive(Debug, Serialize)]
pub struct FsSummary {
    pub median_total_bytes: u64,
    pub average_total_bytes: f64,
    pub median_content_bytes: u64,
    pub median_git_bytes: u64,
    pub median_lane_bytes: u64,
    pub median_file_count: u64,
    pub median_dir_count: u64,
}

pub fn run_paired_benchmark(
    mode: &'static str,
    rounds: usize,
    lane_bin: &Path,
    worker: WorkerKind,
) -> BenchmarkReport {
    let temp = TempDir::new("lane-bench");
    let round_results = (0..rounds)
        .map(|round| {
            let round_root = temp.path().join(format!("round-{:02}", round + 1));
            let order = if round % 2 == 0 {
                vec!["git_worktree", "lane"]
            } else {
                vec!["lane", "git_worktree"]
            };

            let mut git_worktree = None;
            let mut lane = None;
            for flow in &order {
                match *flow {
                    "git_worktree" => {
                        git_worktree = Some(run_worktree_flow(
                            &round_root.join("git-worktrees"),
                            worker.clone(),
                        ));
                    }
                    "lane" => {
                        lane = Some(run_lane_flow(
                            lane_bin,
                            &round_root.join("lane"),
                            worker.clone(),
                        ));
                    }
                    _ => unreachable!(),
                }
            }

            BenchmarkRound {
                round: round + 1,
                order,
                git_worktree: git_worktree.unwrap(),
                lane: lane.unwrap(),
            }
        })
        .collect::<Vec<_>>();

    let git_summary = summarize(round_results.iter().map(|round| &round.git_worktree));
    let lane_summary = summarize(round_results.iter().map(|round| &round.lane));
    let gain = GainSummary {
        lane_total_ms_win_count: round_results
            .iter()
            .filter(|round| round.lane.total_ms < round.git_worktree.total_ms)
            .count(),
        lane_worker_ms_win_count: round_results
            .iter()
            .filter(|round| round.lane.worker_ms < round.git_worktree.worker_ms)
            .count(),
        median_total_ms_delta: git_summary.median_total_ms as i64
            - lane_summary.median_total_ms as i64,
        median_worker_ms_delta: git_summary.median_worker_ms as i64
            - lane_summary.median_worker_ms as i64,
        median_total_ms_speedup: ratio(git_summary.median_total_ms, lane_summary.median_total_ms),
        median_worker_ms_speedup: ratio(
            git_summary.median_worker_ms,
            lane_summary.median_worker_ms,
        ),
        median_active_bytes_delta: git_summary.active_fs.median_total_bytes as i64
            - lane_summary.active_fs.median_total_bytes as i64,
        median_active_files_delta: git_summary.active_fs.median_file_count as i64
            - lane_summary.active_fs.median_file_count as i64,
        median_active_dirs_delta: git_summary.active_fs.median_dir_count as i64
            - lane_summary.active_fs.median_dir_count as i64,
        median_active_bytes_ratio: ratio(
            git_summary.active_fs.median_total_bytes,
            lane_summary.active_fs.median_total_bytes,
        ),
    };

    BenchmarkReport {
        mode,
        rounds,
        variants: VARIANTS.len(),
        base_files: BASE_FILES,
        winner: VARIANTS[WINNER].name,
        agent: None,
        summaries: BenchmarkSummaries {
            git_worktree: git_summary,
            lane: lane_summary,
        },
        gain,
        round_results,
    }
}

fn run_worktree_flow(worktree_parent: &Path, worker: WorkerKind) -> FlowMetrics {
    let worktree_base = worktree_parent.join("base");
    create_fixture(&worktree_base);
    init_git_repo(&worktree_base);
    let mut guard = WorktreeGuard::new(worktree_base.clone());

    let flow_start = Instant::now();
    let setup_start = Instant::now();
    for variant in VARIANTS {
        let root = worktree_parent.join(format!("worktree-{}", variant.name));
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&worktree_base)
                .args(["worktree", "add", "--detach"])
                .arg(&root)
                .arg("HEAD"),
        );
        guard.roots.push(root);
    }
    let setup_ms = elapsed_ms(setup_start);

    let worker_start = Instant::now();
    let workers = guard
        .roots
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, root)| {
            let worker = worker.clone();
            thread::spawn(move || run_worker_command(index, &root, worker))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|job| job.join().unwrap())
        .collect::<Vec<_>>();
    let worker_ms = elapsed_ms(worker_start);
    let base_stable_before_promote = read_shared(&worktree_base) == "base";

    let compare_start = Instant::now();
    for root in &guard.roots {
        run_checked(Command::new("git").arg("-C").arg(root).args([
            "status",
            "--short",
            "--untracked-files=all",
        ]));
    }
    let compare_ms = elapsed_ms(compare_start);
    let active_fs = fs_metrics(worktree_parent);

    let promote_start = Instant::now();
    copy_winner_to_base(&guard.roots[WINNER], &worktree_base);
    let promote_ms = elapsed_ms(promote_start);
    let promoted_shared = read_shared(&worktree_base);
    let only_winner_marker_promoted = only_winner_marker_exists(&worktree_base);

    let cleanup_start = Instant::now();
    guard.cleanup_checked();
    let cleanup_ms = elapsed_ms(cleanup_start);

    FlowMetrics {
        repo_dirs: (VARIANTS.len() + 1) as u64,
        setup_ms,
        worker_ms,
        compare_ms,
        promote_ms,
        cleanup_ms,
        total_ms: elapsed_ms(flow_start),
        active_fs,
        base_stable_before_promote,
        promoted_shared,
        only_winner_marker_promoted,
        worker_failures: workers.into_iter().flatten().collect(),
        lane_exec: None,
    }
}

fn run_lane_flow(lane_bin: &Path, lane_root: &Path, worker: WorkerKind) -> FlowMetrics {
    create_fixture(lane_root);
    init_git_repo(lane_root);

    let flow_start = Instant::now();
    let worker_start = Instant::now();
    let worker_outputs = VARIANTS
        .iter()
        .enumerate()
        .map(|(index, variant)| {
            let lane_bin = lane_bin.to_path_buf();
            let repo_root = lane_root.to_path_buf();
            let lane = variant.name.to_owned();
            let worker = worker.clone();
            thread::spawn(move || run_lane_worker(&lane_bin, &repo_root, &lane, index, worker))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|job| job.join().unwrap())
        .collect::<Vec<_>>();
    let worker_ms = elapsed_ms(worker_start);
    let base_stable_before_promote = read_shared(lane_root) == "base";

    let compare_start = Instant::now();
    for variant in VARIANTS {
        run_checked(
            Command::new(lane_bin)
                .arg("--repo-root")
                .arg(lane_root)
                .args(["changes", variant.name]),
        );
    }
    let compare_ms = elapsed_ms(compare_start);
    let active_fs = fs_metrics(lane_root);

    let promote_start = Instant::now();
    run_checked(
        Command::new(lane_bin)
            .arg("--repo-root")
            .arg(lane_root)
            .args(["promote-lane", VARIANTS[WINNER].name]),
    );
    let promote_ms = elapsed_ms(promote_start);
    let promoted_shared = read_shared(lane_root);
    let only_winner_marker_promoted = only_winner_marker_exists(lane_root);

    let cleanup_start = Instant::now();
    for variant in VARIANTS {
        run_checked(
            Command::new(lane_bin)
                .arg("--repo-root")
                .arg(lane_root)
                .args(["discard", variant.name]),
        );
    }
    let cleanup_ms = elapsed_ms(cleanup_start);

    FlowMetrics {
        repo_dirs: 1,
        setup_ms: 0,
        worker_ms,
        compare_ms,
        promote_ms,
        cleanup_ms,
        total_ms: elapsed_ms(flow_start),
        active_fs,
        base_stable_before_promote,
        promoted_shared,
        only_winner_marker_promoted,
        worker_failures: worker_outputs
            .iter()
            .filter_map(|output| output.failure.clone())
            .collect(),
        lane_exec: Some(sum_lane_exec_totals(&worker_outputs)),
    }
}

pub fn run_worker_command(index: usize, root: &Path, worker: WorkerKind) -> Option<WorkerFailure> {
    let (program, args) = worker_command(index, worker);
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    worker_failure(index, &output, None)
}

pub fn run_lane_worker(
    lane_bin: &Path,
    repo_root: &Path,
    lane: &str,
    index: usize,
    worker: WorkerKind,
) -> LaneWorkerOutput {
    let (program, args) = worker_command(index, worker);
    let output = Command::new(lane_bin)
        .arg("--repo-root")
        .arg(repo_root)
        .arg("exec")
        .arg(lane)
        .arg("--")
        .arg(&program)
        .args(args)
        .output()
        .unwrap();
    let value = serde_json::from_slice::<Value>(&output.stdout).ok();
    let worker_exit = value
        .as_ref()
        .and_then(|value| value["exit_code"].as_i64())
        .map(|code| code as i32);

    LaneWorkerOutput {
        failure: worker_failure(index, &output, worker_exit),
        json: value,
    }
}

#[cfg(windows)]
pub fn worker_command(index: usize, worker: WorkerKind) -> (PathBuf, Vec<String>) {
    match worker {
        WorkerKind::Scripted => scripted_worker_command(index),
        WorkerKind::Codex { program } => codex_worker_command(index, program),
    }
}

#[cfg(windows)]
pub fn scripted_worker_command(index: usize) -> (PathBuf, Vec<String>) {
    let variant = VARIANTS[index];
    (
        PathBuf::from("cmd"),
        vec![
            "/D".to_owned(),
            "/C".to_owned(),
            format!(
                "> src\\shared.txt echo {} && > src\\{}.txt echo {}",
                variant.value, variant.name, variant.value
            ),
        ],
    )
}

pub fn codex_worker_command(index: usize, program: PathBuf) -> (PathBuf, Vec<String>) {
    let variant = VARIANTS[index];
    let prompt = format!(
        "You are in a temporary benchmark repository. Make exactly these two file changes and nothing else: overwrite src/shared.txt with exactly this text: {}; create src/{}.txt with exactly this text: {}; do not commit; do not edit any other file; use direct PowerShell file writes; stop as soon as those files are written.",
        variant.value, variant.name, variant.value
    );
    (
        program,
        vec![
            "exec".to_owned(),
            "--dangerously-bypass-approvals-and-sandbox".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "--ephemeral".to_owned(),
            "--ignore-rules".to_owned(),
            "--color".to_owned(),
            "never".to_owned(),
            prompt,
        ],
    )
}

pub struct LaneWorkerOutput {
    pub failure: Option<WorkerFailure>,
    pub json: Option<Value>,
}

pub fn worker_failure(
    index: usize,
    output: &Output,
    worker_exit: Option<i32>,
) -> Option<WorkerFailure> {
    let process_exit = output.status.code();
    let worker_exit = worker_exit.or(process_exit);
    if output.status.success() && worker_exit == Some(0) {
        return None;
    }

    Some(WorkerFailure {
        variant: VARIANTS[index].name.to_owned(),
        process_exit,
        worker_exit,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

pub fn sum_lane_exec_totals(outputs: &[LaneWorkerOutput]) -> LaneExecTotals {
    let mut totals = LaneExecTotals::default();
    for output in outputs {
        let Some(value) = &output.json else {
            continue;
        };
        let timings = &value["timings"];
        totals.total_ms += u64_field(timings, "total_ms");
        totals.worker_ms += u64_field(timings, "worker_ms");
        totals.storage_lock_wait_ms += u64_field(timings, "storage_lock_wait_ms");
        totals.storage_lock_held_ms += u64_field(timings, "storage_lock_held_ms");
        totals.mount_ms += u64_field(timings, "mount_ms");
        totals.unmount_ms += u64_field(timings, "unmount_ms");
        totals.storage_write_ops += u64_field(timings, "storage_write_ops");
    }
    totals
}

pub fn assert_report(report: &BenchmarkReport) {
    assert_eq!(report.variants, 5);
    for round in &report.round_results {
        assert_flow(&round.git_worktree, (VARIANTS.len() + 1) as u64);
        assert_flow(&round.lane, 1);
        assert_ne!(round.order[0], round.order[1]);
    }
}

pub fn assert_flow(flow: &FlowMetrics, expected_repo_dirs: u64) {
    assert_eq!(flow.repo_dirs, expected_repo_dirs);
    assert!(flow.base_stable_before_promote);
    assert_eq!(flow.promoted_shared, VARIANTS[WINNER].value);
    assert!(flow.only_winner_marker_promoted);
    assert!(
        flow.worker_failures.is_empty(),
        "worker failure: {:#?}",
        flow.worker_failures
    );
}

pub fn summarize<'a>(flows: impl Iterator<Item = &'a FlowMetrics>) -> FlowSummary {
    let flows = flows.cloned().collect::<Vec<_>>();
    FlowSummary {
        median_total_ms: median(flows.iter().map(|flow| flow.total_ms).collect()),
        average_total_ms: average(flows.iter().map(|flow| flow.total_ms).collect()),
        min_total_ms: flows.iter().map(|flow| flow.total_ms).min().unwrap_or(0),
        max_total_ms: flows.iter().map(|flow| flow.total_ms).max().unwrap_or(0),
        median_worker_ms: median(flows.iter().map(|flow| flow.worker_ms).collect()),
        average_worker_ms: average(flows.iter().map(|flow| flow.worker_ms).collect()),
        median_setup_ms: median(flows.iter().map(|flow| flow.setup_ms).collect()),
        median_compare_ms: median(flows.iter().map(|flow| flow.compare_ms).collect()),
        median_promote_ms: median(flows.iter().map(|flow| flow.promote_ms).collect()),
        median_cleanup_ms: median(flows.iter().map(|flow| flow.cleanup_ms).collect()),
        active_fs: summarize_fs(flows.iter().map(|flow| flow.active_fs.clone()).collect()),
        all_workers_succeeded: flows.iter().all(|flow| flow.worker_failures.is_empty()),
        all_base_stable_before_promote: flows.iter().all(|flow| flow.base_stable_before_promote),
        all_only_winner_marker_promoted: flows.iter().all(|flow| flow.only_winner_marker_promoted),
    }
}

pub fn create_fixture(root: &Path) {
    fs::create_dir_all(root.join("src/lib")).unwrap();
    fs::write(root.join("src/shared.txt"), "base\n").unwrap();
    let payload = "x".repeat(4096);
    for index in 0..BASE_FILES {
        fs::write(
            root.join(format!("src/lib/file-{index:03}.ts")),
            format!("export const file{index} = '{payload}';"),
        )
        .unwrap();
    }
}

pub fn init_git_repo(root: &Path) {
    run_checked(Command::new("git").arg("-C").arg(root).args(["init", "-q"]));
    run_checked(Command::new("git").arg("-C").arg(root).args([
        "config",
        "user.email",
        "lane@example.invalid",
    ]));
    run_checked(Command::new("git").arg("-C").arg(root).args([
        "config",
        "user.name",
        "Lane Benchmark",
    ]));
    run_checked(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "core.autocrlf", "false"]),
    );
    run_checked(Command::new("git").arg("-C").arg(root).args(["add", "."]));
    run_checked(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-q", "-m", "base"]),
    );
}

pub fn copy_winner_to_base(worktree: &Path, base: &Path) {
    fs::copy(worktree.join("src/shared.txt"), base.join("src/shared.txt")).unwrap();
    fs::copy(
        worktree.join(format!("src/{}.txt", VARIANTS[WINNER].name)),
        base.join(format!("src/{}.txt", VARIANTS[WINNER].name)),
    )
    .unwrap();
}

pub fn only_winner_marker_exists(root: &Path) -> bool {
    VARIANTS.iter().enumerate().all(|(index, variant)| {
        root.join(format!("src/{}.txt", variant.name)).exists() == (index == WINNER)
    })
}

pub fn read_shared(root: &Path) -> String {
    fs::read_to_string(root.join("src/shared.txt"))
        .unwrap()
        .trim_start_matches('\u{feff}')
        .trim_end()
        .to_owned()
}

pub fn run_checked(command: &mut Command) -> Vec<u8> {
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

pub fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        return path.to_path_buf();
    }

    let extensions = env::var_os("PATHEXT")
        .map(|value| {
            env::split_paths(&value)
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| {
            [".COM", ".EXE", ".BAT", ".CMD"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        });
    let names = if path.extension().is_some() {
        vec![program.to_owned()]
    } else {
        extensions
            .iter()
            .map(|extension| format!("{program}{extension}"))
            .collect::<Vec<_>>()
    };

    env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| env::split_paths(&paths).collect::<Vec<_>>())
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| path.to_path_buf())
}

pub fn bench_rounds(var: &str, default: usize) -> usize {
    env::var(var)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|rounds| *rounds > 0)
        .unwrap_or(default)
}

pub fn median(mut values: Vec<u64>) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

pub fn average(values: Vec<u64>) -> f64 {
    round_two(values.iter().sum::<u64>() as f64 / values.len() as f64)
}

pub fn ratio(numerator: u64, denominator: u64) -> Value {
    if denominator == 0 {
        Value::Null
    } else {
        json!(round_two(numerator as f64 / denominator as f64))
    }
}

pub fn round_two(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub fn u64_field(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

pub fn fs_metrics(root: &Path) -> FsMetrics {
    let mut metrics = FsMetrics::default();
    collect_fs_metrics(root, MetricCategory::Content, &mut metrics);
    metrics
}

fn collect_fs_metrics(root: &Path, category: MetricCategory, metrics: &mut FsMetrics) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        let child_category = match entry.file_name().to_string_lossy().as_ref() {
            ".git" => MetricCategory::Git,
            ".lane" => MetricCategory::Lane,
            _ => category,
        };
        let metadata = entry.metadata().unwrap();
        if metadata.is_dir() {
            metrics.dir_count += 1;
            collect_fs_metrics(&path, child_category, metrics);
        } else if metadata.is_file() {
            metrics.file_count += 1;
            metrics.total_bytes += metadata.len();
            match child_category {
                MetricCategory::Content => metrics.content_bytes += metadata.len(),
                MetricCategory::Git => metrics.git_bytes += metadata.len(),
                MetricCategory::Lane => metrics.lane_bytes += metadata.len(),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum MetricCategory {
    Content,
    Git,
    Lane,
}

pub fn summarize_fs(values: Vec<FsMetrics>) -> FsSummary {
    FsSummary {
        median_total_bytes: median(values.iter().map(|metrics| metrics.total_bytes).collect()),
        average_total_bytes: average(values.iter().map(|metrics| metrics.total_bytes).collect()),
        median_content_bytes: median(values.iter().map(|metrics| metrics.content_bytes).collect()),
        median_git_bytes: median(values.iter().map(|metrics| metrics.git_bytes).collect()),
        median_lane_bytes: median(values.iter().map(|metrics| metrics.lane_bytes).collect()),
        median_file_count: median(values.iter().map(|metrics| metrics.file_count).collect()),
        median_dir_count: median(values.iter().map(|metrics| metrics.dir_count).collect()),
    }
}

pub fn path_label(path: &Path) -> String {
    path.display().to_string()
}

pub fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

pub struct WorktreeGuard {
    pub base: PathBuf,
    pub roots: Vec<PathBuf>,
}

impl WorktreeGuard {
    pub fn new(base: PathBuf) -> Self {
        Self {
            base,
            roots: Vec::new(),
        }
    }

    pub fn cleanup_checked(&mut self) {
        for root in self.roots.drain(..) {
            run_checked(
                Command::new("git")
                    .arg("-C")
                    .arg(&self.base)
                    .args(["worktree", "remove", "--force"])
                    .arg(root),
            );
        }
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&self.base)
                .args(["worktree", "prune"]),
        );
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        for root in self.roots.drain(..) {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&self.base)
                .args(["worktree", "remove", "--force"])
                .arg(root)
                .output();
        }
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.base)
            .args(["worktree", "prune"])
            .output();
    }
}

pub struct TempDir {
    root: PathBuf,
}

impl TempDir {
    pub fn new(prefix: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("{prefix}-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    pub fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
