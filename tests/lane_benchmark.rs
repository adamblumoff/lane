use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const VARIANTS: usize = 5;
const BASE_FILES: usize = 200;
const WINNER: usize = 2;
const DEFAULT_ROUNDS: usize = 5;

#[test]
#[ignore = "benchmark: compares git worktrees, recorded previous Lane metrics, and current Lane over multiple rounds"]
fn benchmark_git_worktrees_recorded_previous_lane_and_current_lane() {
    let temp = TempDir::new("lane-bench");
    let rounds = bench_rounds();
    let current_lane_path = PathBuf::from(env!("CARGO_BIN_EXE_lane"));
    let baseline_path = baseline_path();
    let previous_baseline = load_previous_lane_baseline(&baseline_path);

    let round_results = (0..rounds)
        .map(|round| {
            let round_root = temp.path().join(format!("round-{round:02}"));
            let git_worktree = run_worktree_flow(&round_root.join("git-worktrees"));
            let current_lane = run_lane_flow(&current_lane_path, &round_root.join("current-lane"));
            BenchmarkRound {
                round: round + 1,
                git_worktree,
                current_lane,
            }
        })
        .collect::<Vec<_>>();

    let git_average = average_worktree_metrics(&round_results);
    let current_average = average_lane_metrics(&round_results);
    let current_vs_previous = previous_baseline
        .as_ref()
        .map(|previous| lane_gain(previous, &current_average));
    let current_vs_worktree = lane_vs_worktree_gain(&git_average, &current_average);
    let previous_vs_worktree = previous_baseline
        .as_ref()
        .map(|previous| lane_vs_worktree_gain(&git_average, previous));

    let result = json!({
        "mode": "git_worktree_vs_recorded_previous_lane_vs_current_lane",
        "rounds": rounds,
        "benchmark_order": ["git_worktree", "previous_lane", "current_lane"],
        "variants": VARIANTS,
        "base_files": BASE_FILES,
        "git_worktree": {
            "source": "git worktree",
        },
        "previous_lane": {
            "source": "recorded_baseline",
            "baseline_path": &baseline_path,
            "metrics": &previous_baseline,
        },
        "current_lane": {
            "source": "current_worktree",
            "binary": &current_lane_path,
        },
        "averages": {
            "git_worktree": &git_average,
            "previous_lane": &previous_baseline,
            "current_lane": &current_average,
        },
        "average_gain": {
            "current_vs_previous_lane": current_vs_previous,
            "current_lane_vs_git_worktree": current_vs_worktree,
            "previous_lane_vs_git_worktree": previous_vs_worktree,
        },
        "round_results": &round_results,
    });
    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    for round in &round_results {
        assert!(round.git_worktree.base_stable_before_promote);
        assert!(round.current_lane.base_stable_before_promote);
        assert_eq!(round.git_worktree.repo_dirs, (VARIANTS + 1) as u64);
        assert_eq!(round.current_lane.repo_dirs, 1);
        assert!(round.git_worktree.active_disk_bytes > round.current_lane.active_disk_bytes);
    }
    write_lane_baseline(&baseline_path, &current_average);
}

#[derive(Debug, Serialize)]
struct BenchmarkRound {
    round: usize,
    git_worktree: WorktreeMetrics,
    current_lane: LaneMetrics,
}

fn run_worktree_flow(worktree_parent: &Path) -> WorktreeMetrics {
    let worktree_base = worktree_parent.join("base");
    create_fixture(&worktree_base, BASE_FILES);
    init_git_repo(&worktree_base);

    let worktree_start = Instant::now();
    let setup_start = Instant::now();
    let worktree_roots = (0..VARIANTS)
        .map(|index| {
            let root = worktree_parent.join(format!("lane-{index}"));
            run_checked(
                Command::new("git")
                    .arg("-C")
                    .arg(&worktree_base)
                    .args(["worktree", "add", "--detach"])
                    .arg(&root)
                    .arg("HEAD"),
            );
            root
        })
        .collect::<Vec<_>>();
    let setup_ms = elapsed_ms(setup_start);

    let attempt_start = Instant::now();
    let worktree_jobs = worktree_roots
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, root)| {
            thread::spawn(move || {
                let (program, args) = variant_program_args(index);
                run_checked(Command::new(program).current_dir(root).args(args));
            })
        })
        .collect::<Vec<_>>();
    for job in worktree_jobs {
        job.join().unwrap();
    }
    let attempt_ms = elapsed_ms(attempt_start);
    let base_stable_before_promote = read_login(&worktree_base) == "export const design = 'base';";

    let compare_start = Instant::now();
    for root in &worktree_roots {
        run_checked(Command::new("git").arg("-C").arg(root).args(["diff"]));
    }
    let compare_ms = elapsed_ms(compare_start);
    let active_disk_bytes = dir_size(worktree_parent);

    let promote_start = Instant::now();
    copy_variant_to_base(&worktree_roots[WINNER], &worktree_base, WINNER);
    let promote_ms = elapsed_ms(promote_start);
    assert_eq!(
        read_login(&worktree_base),
        format!("export const design = 'lane-{WINNER}';")
    );
    assert!(
        worktree_base
            .join(format!("src/marker-{WINNER}.ts"))
            .exists()
    );

    let cleanup_start = Instant::now();
    for root in &worktree_roots {
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&worktree_base)
                .args(["worktree", "remove", "--force"])
                .arg(root),
        );
    }
    let cleanup_ms = elapsed_ms(cleanup_start);
    WorktreeMetrics {
        repo_dirs: (VARIANTS + 1) as u64,
        setup_ms,
        attempt_ms,
        compare_ms,
        promote_ms,
        cleanup_ms,
        total_ms: elapsed_ms(worktree_start),
        active_disk_bytes,
        base_stable_before_promote,
    }
}

#[derive(Debug, Serialize)]
struct WorktreeMetrics {
    repo_dirs: u64,
    setup_ms: u64,
    attempt_ms: u64,
    compare_ms: u64,
    promote_ms: u64,
    cleanup_ms: u64,
    total_ms: u64,
    active_disk_bytes: u64,
    base_stable_before_promote: bool,
}

fn bench_rounds() -> usize {
    env::var("LANE_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|rounds| *rounds > 0)
        .unwrap_or(DEFAULT_ROUNDS)
}

fn run_lane_flow(lane_bin: &Path, lane_root: &Path) -> LaneMetrics {
    create_fixture(lane_root, BASE_FILES);
    init_git_repo(lane_root);

    let lane_start = Instant::now();
    let attempt_start = Instant::now();
    let exec_outputs = (0..VARIANTS)
        .map(|index| {
            let repo_root = lane_root.to_path_buf();
            let lane_bin = lane_bin.to_path_buf();
            thread::spawn(move || {
                let lane = format!("lane-{index}");
                let (program, args) = variant_program_args(index);
                run_checked(
                    Command::new(lane_bin)
                        .arg("--repo-root")
                        .arg(&repo_root)
                        .args(["exec", &lane, "--", &program])
                        .args(args),
                )
            })
        })
        .collect::<Vec<_>>();
    let exec_outputs = exec_outputs
        .into_iter()
        .map(|job| job.join().unwrap())
        .collect::<Vec<_>>();
    let attempt_ms = elapsed_ms(attempt_start);
    let base_stable_before_promote = read_login(lane_root) == "export const design = 'base';";

    let compare_start = Instant::now();
    for index in 0..VARIANTS {
        run_checked(
            Command::new(lane_bin)
                .arg("--repo-root")
                .arg(lane_root)
                .args(["diff", &format!("lane-{index}")]),
        );
    }
    let compare_ms = elapsed_ms(compare_start);
    let active_disk_bytes = dir_size(lane_root);

    let promote_start = Instant::now();
    run_checked(
        Command::new(lane_bin)
            .arg("--repo-root")
            .arg(lane_root)
            .args(["promote-lane", &format!("lane-{WINNER}"), "--json"]),
    );
    let promote_ms = elapsed_ms(promote_start);
    assert_eq!(
        read_login(lane_root),
        format!("export const design = 'lane-{WINNER}';")
    );
    assert!(lane_root.join(format!("src/marker-{WINNER}.ts")).exists());

    let cleanup_start = Instant::now();
    for index in 0..VARIANTS {
        if index != WINNER {
            run_checked(
                Command::new(lane_bin)
                    .arg("--repo-root")
                    .arg(lane_root)
                    .args(["discard", &format!("lane-{index}"), "--json"]),
            );
        }
    }
    let cleanup_ms = elapsed_ms(cleanup_start);
    LaneMetrics {
        mode: "deterministic_exec_capture",
        repo_dirs: 1,
        attempt_ms,
        compare_ms,
        promote_ms,
        cleanup_ms,
        total_ms: elapsed_ms(lane_start),
        active_disk_bytes,
        base_stable_before_promote,
        exec: sum_exec_timings(&exec_outputs),
    }
}

#[derive(Debug, Serialize)]
struct LaneMetrics {
    mode: &'static str,
    repo_dirs: u64,
    attempt_ms: u64,
    compare_ms: u64,
    promote_ms: u64,
    cleanup_ms: u64,
    total_ms: u64,
    active_disk_bytes: u64,
    base_stable_before_promote: bool,
    exec: ExecTimingTotals,
}

#[derive(Debug, Default, Serialize)]
struct ExecTimingTotals {
    lock_wait_ms: u64,
    lock_held_ms: u64,
    storage_lock_wait_ms: u64,
    storage_lock_held_ms: u64,
    raw_lock_wait_ms: u64,
    raw_lock_held_ms: u64,
    pre_worker_lock_ms: u64,
    worker_ms: u64,
    post_worker_lock_ms: u64,
    materialize_total_ms: u64,
    materialize_pre_worker_ms: u64,
    materialize_worker_ms: u64,
    materialize_post_worker_ms: u64,
    snapshot_ms: u64,
    project_ms: u64,
    operation_ms: u64,
    detect_ms: u64,
    capture_ms: u64,
    restore_ms: u64,
    ingest_ms: u64,
}

#[derive(Debug, Serialize)]
struct AverageWorktreeMetrics {
    repo_dirs: f64,
    setup_ms: f64,
    attempt_ms: f64,
    compare_ms: f64,
    promote_ms: f64,
    cleanup_ms: f64,
    total_ms: f64,
    active_disk_bytes: f64,
    base_stable_before_promote: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AverageLaneMetrics {
    mode: String,
    repo_dirs: f64,
    attempt_ms: f64,
    compare_ms: f64,
    promote_ms: f64,
    cleanup_ms: f64,
    total_ms: f64,
    active_disk_bytes: f64,
    base_stable_before_promote: bool,
    exec: AverageExecTimingTotals,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AverageExecTimingTotals {
    lock_wait_ms: f64,
    lock_held_ms: f64,
    #[serde(default)]
    storage_lock_wait_ms: f64,
    #[serde(default)]
    storage_lock_held_ms: f64,
    #[serde(default)]
    raw_lock_wait_ms: f64,
    #[serde(default)]
    raw_lock_held_ms: f64,
    #[serde(default)]
    pre_worker_lock_ms: f64,
    #[serde(default)]
    worker_ms: f64,
    #[serde(default)]
    post_worker_lock_ms: f64,
    materialize_total_ms: f64,
    #[serde(default)]
    materialize_pre_worker_ms: f64,
    #[serde(default)]
    materialize_worker_ms: f64,
    #[serde(default)]
    materialize_post_worker_ms: f64,
    snapshot_ms: f64,
    project_ms: f64,
    operation_ms: f64,
    detect_ms: f64,
    capture_ms: f64,
    restore_ms: f64,
    ingest_ms: f64,
}

fn sum_exec_timings(outputs: &[Vec<u8>]) -> ExecTimingTotals {
    let mut totals = ExecTimingTotals::default();
    for output in outputs {
        let value: Value = serde_json::from_slice(output).unwrap();
        let timings = &value["timings"];
        let materialize = &timings["materialize"];
        totals.lock_wait_ms += u64_field(timings, "lock_wait_ms");
        totals.lock_held_ms += u64_field(timings, "lock_held_ms");
        totals.storage_lock_wait_ms += u64_field(timings, "storage_lock_wait_ms");
        totals.storage_lock_held_ms += u64_field(timings, "storage_lock_held_ms");
        totals.raw_lock_wait_ms += u64_field(timings, "raw_lock_wait_ms");
        totals.raw_lock_held_ms += u64_field(timings, "raw_lock_held_ms");
        totals.pre_worker_lock_ms += u64_field(timings, "pre_worker_lock_ms");
        totals.worker_ms += u64_field(timings, "worker_ms");
        totals.post_worker_lock_ms += u64_field(timings, "post_worker_lock_ms");
        totals.materialize_total_ms += u64_field(materialize, "total_ms");
        totals.materialize_pre_worker_ms += u64_field(materialize, "pre_worker_ms");
        totals.materialize_worker_ms += u64_field(materialize, "worker_ms");
        totals.materialize_post_worker_ms += u64_field(materialize, "post_worker_ms");
        totals.snapshot_ms += u64_field(materialize, "snapshot_ms");
        totals.project_ms += u64_field(materialize, "project_ms");
        totals.operation_ms += u64_field(materialize, "operation_ms");
        totals.detect_ms += u64_field(materialize, "detect_ms");
        totals.capture_ms += u64_field(materialize, "capture_ms");
        totals.restore_ms += u64_field(materialize, "restore_ms");
        totals.ingest_ms += u64_field(materialize, "ingest_ms");
    }
    totals
}

fn average_worktree_metrics(rounds: &[BenchmarkRound]) -> AverageWorktreeMetrics {
    let count = rounds.len();
    AverageWorktreeMetrics {
        repo_dirs: average(
            rounds.iter().map(|round| round.git_worktree.repo_dirs),
            count,
        ),
        setup_ms: average(
            rounds.iter().map(|round| round.git_worktree.setup_ms),
            count,
        ),
        attempt_ms: average(
            rounds.iter().map(|round| round.git_worktree.attempt_ms),
            count,
        ),
        compare_ms: average(
            rounds.iter().map(|round| round.git_worktree.compare_ms),
            count,
        ),
        promote_ms: average(
            rounds.iter().map(|round| round.git_worktree.promote_ms),
            count,
        ),
        cleanup_ms: average(
            rounds.iter().map(|round| round.git_worktree.cleanup_ms),
            count,
        ),
        total_ms: average(
            rounds.iter().map(|round| round.git_worktree.total_ms),
            count,
        ),
        active_disk_bytes: average(
            rounds
                .iter()
                .map(|round| round.git_worktree.active_disk_bytes),
            count,
        ),
        base_stable_before_promote: rounds
            .iter()
            .all(|round| round.git_worktree.base_stable_before_promote),
    }
}

fn average_lane_metrics(rounds: &[BenchmarkRound]) -> AverageLaneMetrics {
    let count = rounds.len();
    AverageLaneMetrics {
        mode: "deterministic_exec_capture".to_owned(),
        repo_dirs: average(
            rounds.iter().map(|round| round.current_lane.repo_dirs),
            count,
        ),
        attempt_ms: average(
            rounds.iter().map(|round| round.current_lane.attempt_ms),
            count,
        ),
        compare_ms: average(
            rounds.iter().map(|round| round.current_lane.compare_ms),
            count,
        ),
        promote_ms: average(
            rounds.iter().map(|round| round.current_lane.promote_ms),
            count,
        ),
        cleanup_ms: average(
            rounds.iter().map(|round| round.current_lane.cleanup_ms),
            count,
        ),
        total_ms: average(
            rounds.iter().map(|round| round.current_lane.total_ms),
            count,
        ),
        active_disk_bytes: average(
            rounds
                .iter()
                .map(|round| round.current_lane.active_disk_bytes),
            count,
        ),
        base_stable_before_promote: rounds
            .iter()
            .all(|round| round.current_lane.base_stable_before_promote),
        exec: AverageExecTimingTotals {
            lock_wait_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.lock_wait_ms),
                count,
            ),
            lock_held_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.lock_held_ms),
                count,
            ),
            storage_lock_wait_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.storage_lock_wait_ms),
                count,
            ),
            storage_lock_held_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.storage_lock_held_ms),
                count,
            ),
            raw_lock_wait_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.raw_lock_wait_ms),
                count,
            ),
            raw_lock_held_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.raw_lock_held_ms),
                count,
            ),
            pre_worker_lock_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.pre_worker_lock_ms),
                count,
            ),
            worker_ms: average(
                rounds.iter().map(|round| round.current_lane.exec.worker_ms),
                count,
            ),
            post_worker_lock_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.post_worker_lock_ms),
                count,
            ),
            materialize_total_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.materialize_total_ms),
                count,
            ),
            materialize_pre_worker_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.materialize_pre_worker_ms),
                count,
            ),
            materialize_worker_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.materialize_worker_ms),
                count,
            ),
            materialize_post_worker_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.materialize_post_worker_ms),
                count,
            ),
            snapshot_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.snapshot_ms),
                count,
            ),
            project_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.project_ms),
                count,
            ),
            operation_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.operation_ms),
                count,
            ),
            detect_ms: average(
                rounds.iter().map(|round| round.current_lane.exec.detect_ms),
                count,
            ),
            capture_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.capture_ms),
                count,
            ),
            restore_ms: average(
                rounds
                    .iter()
                    .map(|round| round.current_lane.exec.restore_ms),
                count,
            ),
            ingest_ms: average(
                rounds.iter().map(|round| round.current_lane.exec.ingest_ms),
                count,
            ),
        },
    }
}

fn lane_gain(previous: &AverageLaneMetrics, current: &AverageLaneMetrics) -> Value {
    json!({
        "attempt_ms_speedup": ratio_f64(previous.attempt_ms, current.attempt_ms),
        "total_ms_speedup": ratio_f64(previous.total_ms, current.total_ms),
        "lock_held_ms_speedup": ratio_f64(previous.exec.lock_held_ms, current.exec.lock_held_ms),
        "materialize_ms_speedup": ratio_f64(previous.exec.materialize_total_ms, current.exec.materialize_total_ms),
        "snapshot_ms_speedup": ratio_f64(previous.exec.snapshot_ms, current.exec.snapshot_ms),
        "detect_ms_speedup": ratio_f64(previous.exec.detect_ms, current.exec.detect_ms),
        "restore_ms_speedup": ratio_f64(previous.exec.restore_ms, current.exec.restore_ms),
        "attempt_ms_improvement": improvement_f64(previous.attempt_ms, current.attempt_ms),
        "total_ms_improvement": improvement_f64(previous.total_ms, current.total_ms),
        "lock_held_ms_improvement": improvement_f64(previous.exec.lock_held_ms, current.exec.lock_held_ms),
        "materialize_ms_improvement": improvement_f64(previous.exec.materialize_total_ms, current.exec.materialize_total_ms),
        "active_disk_bytes_delta": signed_delta_f64(previous.active_disk_bytes, current.active_disk_bytes),
    })
}

fn lane_vs_worktree_gain(worktree: &AverageWorktreeMetrics, lane: &AverageLaneMetrics) -> Value {
    json!({
        "repo_dirs_removed": improvement_f64(worktree.repo_dirs, lane.repo_dirs),
        "attempt_ms_speedup": ratio_f64(worktree.attempt_ms, lane.attempt_ms),
        "total_ms_speedup": ratio_f64(worktree.total_ms, lane.total_ms),
        "active_disk_bytes_ratio": ratio_f64(worktree.active_disk_bytes, lane.active_disk_bytes),
        "active_disk_bytes_reduction": improvement_f64(worktree.active_disk_bytes, lane.active_disk_bytes),
    })
}

fn average(values: impl Iterator<Item = u64>, count: usize) -> f64 {
    round_two(values.sum::<u64>() as f64 / count as f64)
}

fn baseline_path() -> PathBuf {
    env::var("LANE_BENCH_BASELINE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("lane-bench-baseline.json")
        })
}

fn load_previous_lane_baseline(path: &Path) -> Option<AverageLaneMetrics> {
    match fs::read(path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).unwrap()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("failed to read {}: {error}", path.display()),
    }
}

fn write_lane_baseline(path: &Path, metrics: &AverageLaneMetrics) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let bytes = serde_json::to_vec_pretty(metrics).unwrap();
    fs::write(path, bytes).unwrap();
}

fn create_fixture(root: &Path, extra_files: usize) {
    fs::create_dir_all(root.join("src/lib")).unwrap();
    fs::write(root.join("src/login.tsx"), "export const design = 'base';").unwrap();
    let payload = "x".repeat(4096);
    for index in 0..extra_files {
        fs::write(
            root.join(format!("src/lib/file-{index:03}.ts")),
            format!("export const file{index} = '{payload}';"),
        )
        .unwrap();
    }
}

fn init_git_repo(root: &Path) {
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
    run_checked(Command::new("git").arg("-C").arg(root).args(["add", "."]));
    run_checked(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-q", "-m", "base"]),
    );
}

#[cfg(windows)]
fn variant_program_args(index: usize) -> (String, Vec<String>) {
    (
        "cmd".to_owned(),
        vec![
            "/D".to_owned(),
            "/C".to_owned(),
            format!(
                "> src\\login.tsx echo export const design = 'lane-{index}'; && > src\\marker-{index}.ts echo export const marker = {index};"
            ),
        ],
    )
}

#[cfg(not(windows))]
fn variant_program_args(index: usize) -> (String, Vec<String>) {
    (
        "sh".to_owned(),
        vec![
            "-c".to_owned(),
            format!(
                "printf \"export const design = 'lane-{index}';\\n\" > src/login.tsx && printf \"export const marker = {index};\\n\" > src/marker-{index}.ts"
            ),
        ],
    )
}

fn copy_variant_to_base(worktree: &Path, base: &Path, index: usize) {
    fs::copy(worktree.join("src/login.tsx"), base.join("src/login.tsx")).unwrap();
    fs::copy(
        worktree.join(format!("src/marker-{index}.ts")),
        base.join(format!("src/marker-{index}.ts")),
    )
    .unwrap();
}

fn read_login(root: &Path) -> String {
    fs::read_to_string(root.join("src/login.tsx"))
        .unwrap()
        .trim_end()
        .to_owned()
}

fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    for entry in entries {
        let entry = entry.unwrap();
        let metadata = entry.metadata().unwrap();
        if metadata.is_dir() {
            total += dir_size(&entry.path());
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    total
}

fn ratio_f64(numerator: f64, denominator: f64) -> Value {
    if denominator == 0.0 {
        Value::Null
    } else {
        json!(round_two(numerator / denominator))
    }
}

fn improvement_f64(previous: f64, current: f64) -> f64 {
    round_two(previous - current)
}

fn signed_delta_f64(previous: f64, current: f64) -> f64 {
    round_two(current - previous)
}

fn round_two(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn u64_field(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
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

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("{prefix}-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
