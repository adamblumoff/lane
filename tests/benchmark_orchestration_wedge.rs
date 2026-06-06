#![cfg(windows)]

mod bench_support;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Instant;

use bench_support::{
    BASE_FILES, FsMetrics, TempDir, WorktreeGuard, average, bench_rounds, create_fixture,
    elapsed_ms, fs_metrics, init_git_repo, median, ratio, run_checked,
};
use serde::Serialize;
use serde_json::Value;

const DEFAULT_WEDGE_ATTEMPTS: usize = 20;
const DEFAULT_WEDGE_CYCLES: usize = 3;

#[test]
#[ignore = "benchmark: 20-way repeated orchestration wedge with selected file promotion"]
fn benchmark_orchestration_wedge_lane_vs_worktrees() {
    let attempts = bench_rounds("LANE_WEDGE_ATTEMPTS", DEFAULT_WEDGE_ATTEMPTS);
    let cycles = bench_rounds("LANE_WEDGE_CYCLES", DEFAULT_WEDGE_CYCLES);
    assert!(
        attempts >= 12,
        "wedge benchmark expects at least 12 attempts"
    );

    let lane_bin = PathBuf::from(env!("CARGO_BIN_EXE_lane"));
    let temp = TempDir::new("lane-wedge-bench");
    let worktree = run_worktree_wedge(&temp.path().join("git-worktrees"), attempts, cycles);
    let lane = run_lane_wedge(&lane_bin, &temp.path().join("lane"), attempts, cycles);
    let gain = WedgeGain::new(&worktree, &lane);
    let report = WedgeReport {
        mode: "orchestration_wedge_script",
        attempts_per_cycle: attempts,
        cycles,
        base_files: BASE_FILES,
        promoted_paths_per_cycle: 3,
        gain,
        summaries: WedgeSummaries {
            git_worktree: worktree,
            lane,
        },
    };

    report.assert_valid();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

#[derive(Debug, Serialize)]
struct WedgeReport {
    mode: &'static str,
    attempts_per_cycle: usize,
    cycles: usize,
    base_files: usize,
    promoted_paths_per_cycle: usize,
    gain: WedgeGain,
    summaries: WedgeSummaries,
}

impl WedgeReport {
    fn assert_valid(&self) {
        self.summaries.git_worktree.assert_valid(
            (self.attempts_per_cycle + 1) as u64,
            self.attempts_per_cycle * self.cycles,
            self.promoted_paths_per_cycle * self.cycles,
            self.attempts_per_cycle,
        );
        self.summaries.lane.assert_valid(
            1,
            self.attempts_per_cycle * self.cycles,
            self.promoted_paths_per_cycle * self.cycles,
            self.attempts_per_cycle,
        );
    }
}

#[derive(Debug, Serialize)]
struct WedgeSummaries {
    git_worktree: WedgeFlowMetrics,
    lane: WedgeFlowMetrics,
}

#[derive(Debug, Serialize)]
struct WedgeGain {
    total_ms_delta: i64,
    total_ms_speedup: Value,
    compare_ms_speedup: Value,
    reset_ms_speedup: Value,
    peak_bytes_delta: i64,
    peak_bytes_ratio: Value,
    peak_files_delta: i64,
    peak_files_ratio: Value,
    peak_dirs_delta: i64,
    peak_dirs_ratio: Value,
}

impl WedgeGain {
    fn new(worktree: &WedgeFlowMetrics, lane: &WedgeFlowMetrics) -> Self {
        Self {
            total_ms_delta: worktree.total_ms as i64 - lane.total_ms as i64,
            total_ms_speedup: ratio(worktree.total_ms, lane.total_ms),
            compare_ms_speedup: ratio(worktree.compare_ms, lane.compare_ms),
            reset_ms_speedup: ratio(worktree.reset_ms, lane.reset_ms),
            peak_bytes_delta: worktree.peak_fs.total_bytes as i64 - lane.peak_fs.total_bytes as i64,
            peak_bytes_ratio: ratio(worktree.peak_fs.total_bytes, lane.peak_fs.total_bytes),
            peak_files_delta: worktree.peak_fs.file_count as i64 - lane.peak_fs.file_count as i64,
            peak_files_ratio: ratio(worktree.peak_fs.file_count, lane.peak_fs.file_count),
            peak_dirs_delta: worktree.peak_fs.dir_count as i64 - lane.peak_fs.dir_count as i64,
            peak_dirs_ratio: ratio(worktree.peak_fs.dir_count, lane.peak_fs.dir_count),
        }
    }
}

#[derive(Debug, Serialize)]
struct WedgeFlowMetrics {
    repo_dirs: u64,
    worker_attempts: usize,
    compared_paths: usize,
    promoted_paths: usize,
    promoted_bytes: u64,
    setup_ms: u64,
    worker_ms: u64,
    compare_ms: u64,
    promote_ms: u64,
    checkpoint_ms: u64,
    reset_ms: u64,
    cleanup_ms: u64,
    total_ms: u64,
    peak_fs: FsMetrics,
    end_fs: FsMetrics,
    cycle_totals: WedgeCycleSummary,
    final_shared: String,
}

impl WedgeFlowMetrics {
    fn assert_valid(
        &self,
        repo_dirs: u64,
        worker_attempts: usize,
        promoted_paths: usize,
        attempts_per_cycle: usize,
    ) {
        assert_eq!(self.repo_dirs, repo_dirs);
        assert_eq!(self.worker_attempts, worker_attempts);
        assert_eq!(self.promoted_paths, promoted_paths);
        assert_eq!(self.compared_paths, worker_attempts * 3);
        assert_eq!(
            self.final_shared,
            expected_shared(self.cycle_totals.count - 1, attempts_per_cycle)
        );
        assert!(self.peak_fs.total_bytes >= self.end_fs.total_bytes);
    }
}

#[derive(Debug, Serialize)]
struct WedgeCycleSummary {
    count: usize,
    median_total_ms: u64,
    average_total_ms: f64,
    median_worker_ms: u64,
    median_compare_ms: u64,
    median_promote_ms: u64,
    median_checkpoint_ms: u64,
    median_reset_ms: u64,
    median_active_bytes: u64,
    median_active_files: u64,
    median_active_dirs: u64,
}

#[derive(Debug)]
struct WedgeCycleMetrics {
    total_ms: u64,
    worker_ms: u64,
    compare_ms: u64,
    promote_ms: u64,
    checkpoint_ms: u64,
    reset_ms: u64,
    active_fs: FsMetrics,
}

fn run_worktree_wedge(root: &Path, attempts: usize, cycles: usize) -> WedgeFlowMetrics {
    let base = root.join("base");
    create_fixture(&base);
    init_git_repo(&base);
    let mut guard = WorktreeGuard::new(base.clone());

    let total_start = Instant::now();
    let setup_start = Instant::now();
    for attempt in 0..attempts {
        let worktree = root.join(lane_name(attempt));
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&base)
                .args(["worktree", "add", "--detach"])
                .arg(&worktree)
                .arg("HEAD"),
        );
        guard.roots.push(worktree);
    }
    let setup_ms = elapsed_ms(setup_start);

    let mut cycle_metrics = Vec::new();
    let mut compared_paths = 0;
    let mut promoted_bytes = 0;
    let mut peak_fs = FsMetrics::default();
    for cycle in 0..cycles {
        let cycle_start = Instant::now();
        let worker_start = Instant::now();
        let failures = guard
            .roots
            .iter()
            .cloned()
            .enumerate()
            .map(|(attempt, worktree)| {
                thread::spawn(move || run_wedge_script(&worktree, cycle, attempt))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|job| job.join().unwrap())
            .collect::<Vec<_>>();
        assert!(failures.is_empty(), "worker failures: {failures:#?}");
        let worker_ms = elapsed_ms(worker_start);

        let compare_start = Instant::now();
        for worktree in &guard.roots {
            compared_paths += git_changed_path_count(worktree);
        }
        let compare_ms = elapsed_ms(compare_start);
        let active_fs = fs_metrics(root);
        peak_fs = max_fs(peak_fs, active_fs.clone());

        let promote_start = Instant::now();
        promoted_bytes += promote_worktree_selection(&guard.roots, &base, cycle, attempts);
        let promote_ms = elapsed_ms(promote_start);

        let checkpoint_start = Instant::now();
        commit_cycle(&base, cycle);
        let checkpoint_ms = elapsed_ms(checkpoint_start);

        let reset_start = Instant::now();
        let base_head = String::from_utf8(run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&base)
                .args(["rev-parse", "HEAD"]),
        ))
        .unwrap();
        let base_head = base_head.trim();
        for worktree in &guard.roots {
            run_checked(
                Command::new("git")
                    .arg("-C")
                    .arg(worktree)
                    .args(["reset", "--hard", "-q", base_head]),
            );
            run_checked(
                Command::new("git")
                    .arg("-C")
                    .arg(worktree)
                    .args(["clean", "-fdq"]),
            );
        }
        let reset_ms = elapsed_ms(reset_start);

        cycle_metrics.push(WedgeCycleMetrics {
            total_ms: elapsed_ms(cycle_start),
            worker_ms,
            compare_ms,
            promote_ms,
            checkpoint_ms,
            reset_ms,
            active_fs,
        });
    }

    let cleanup_start = Instant::now();
    guard.cleanup_checked();
    let cleanup_ms = elapsed_ms(cleanup_start);
    let end_fs = fs_metrics(root);

    WedgeFlowMetrics {
        repo_dirs: (attempts + 1) as u64,
        worker_attempts: attempts * cycles,
        compared_paths,
        promoted_paths: cycles * 3,
        promoted_bytes,
        setup_ms,
        worker_ms: cycle_metrics.iter().map(|cycle| cycle.worker_ms).sum(),
        compare_ms: cycle_metrics.iter().map(|cycle| cycle.compare_ms).sum(),
        promote_ms: cycle_metrics.iter().map(|cycle| cycle.promote_ms).sum(),
        checkpoint_ms: cycle_metrics.iter().map(|cycle| cycle.checkpoint_ms).sum(),
        reset_ms: cycle_metrics.iter().map(|cycle| cycle.reset_ms).sum(),
        cleanup_ms,
        total_ms: elapsed_ms(total_start),
        peak_fs,
        end_fs,
        cycle_totals: summarize_wedge_cycles(&cycle_metrics),
        final_shared: read_trimmed(base.join("src/shared.txt")),
    }
}

fn run_lane_wedge(
    lane_bin: &Path,
    root: &Path,
    attempts: usize,
    cycles: usize,
) -> WedgeFlowMetrics {
    create_fixture(root);
    init_git_repo(root);

    let total_start = Instant::now();
    let mut cycle_metrics = Vec::new();
    let mut compared_paths = 0;
    let mut promoted_bytes = 0;
    let mut peak_fs = FsMetrics::default();
    for cycle in 0..cycles {
        let cycle_start = Instant::now();
        let worker_start = Instant::now();
        let failures = (0..attempts)
            .map(|attempt| {
                let lane_bin = lane_bin.to_path_buf();
                let root = root.to_path_buf();
                thread::spawn(move || run_lane_wedge_script(&lane_bin, &root, cycle, attempt))
            })
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|job| job.join().unwrap())
            .collect::<Vec<_>>();
        assert!(failures.is_empty(), "worker failures: {failures:#?}");
        let worker_ms = elapsed_ms(worker_start);

        let compare_start = Instant::now();
        for attempt in 0..attempts {
            compared_paths += lane_changed_path_count(lane_bin, root, attempt);
        }
        let compare_ms = elapsed_ms(compare_start);
        let active_fs = fs_metrics(root);
        peak_fs = max_fs(peak_fs, active_fs.clone());

        let promote_start = Instant::now();
        promoted_bytes += promote_lane_selection(lane_bin, root, cycle, attempts);
        let promote_ms = elapsed_ms(promote_start);

        let checkpoint_start = Instant::now();
        commit_cycle(root, cycle);
        let checkpoint_ms = elapsed_ms(checkpoint_start);

        let reset_start = Instant::now();
        for attempt in 0..attempts {
            run_checked(Command::new(lane_bin).arg("--repo-root").arg(root).args([
                "discard",
                &lane_name(attempt),
                "--json",
            ]));
        }
        let reset_ms = elapsed_ms(reset_start);

        cycle_metrics.push(WedgeCycleMetrics {
            total_ms: elapsed_ms(cycle_start),
            worker_ms,
            compare_ms,
            promote_ms,
            checkpoint_ms,
            reset_ms,
            active_fs,
        });
    }

    WedgeFlowMetrics {
        repo_dirs: 1,
        worker_attempts: attempts * cycles,
        compared_paths,
        promoted_paths: cycles * 3,
        promoted_bytes,
        setup_ms: 0,
        worker_ms: cycle_metrics.iter().map(|cycle| cycle.worker_ms).sum(),
        compare_ms: cycle_metrics.iter().map(|cycle| cycle.compare_ms).sum(),
        promote_ms: cycle_metrics.iter().map(|cycle| cycle.promote_ms).sum(),
        checkpoint_ms: cycle_metrics.iter().map(|cycle| cycle.checkpoint_ms).sum(),
        reset_ms: cycle_metrics.iter().map(|cycle| cycle.reset_ms).sum(),
        cleanup_ms: 0,
        total_ms: elapsed_ms(total_start),
        peak_fs,
        end_fs: fs_metrics(root),
        cycle_totals: summarize_wedge_cycles(&cycle_metrics),
        final_shared: read_trimmed(root.join("src/shared.txt")),
    }
}

fn run_wedge_script(root: &Path, cycle: usize, attempt: usize) -> Option<String> {
    let output = Command::new("cmd")
        .args(["/D", "/C", &wedge_cmd(cycle, attempt)])
        .current_dir(root)
        .output()
        .unwrap();
    if output.status.success() {
        None
    } else {
        Some(format!(
            "{}: {}",
            lane_name(attempt),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn run_lane_wedge_script(
    lane_bin: &Path,
    root: &Path,
    cycle: usize,
    attempt: usize,
) -> Option<String> {
    let output = Command::new(lane_bin)
        .arg("--repo-root")
        .arg(root)
        .args(["exec", &lane_name(attempt), "--", "cmd", "/D", "/C"])
        .arg(wedge_cmd(cycle, attempt))
        .output()
        .unwrap();
    let value = serde_json::from_slice::<Value>(&output.stdout).ok();
    let worker_exit = value
        .as_ref()
        .and_then(|value| value["exit_code"].as_i64())
        .map(|code| code as i32);
    if output.status.success() && worker_exit == Some(0) {
        None
    } else {
        Some(format!(
            "{}: process={:?} worker={:?} stdout={} stderr={}",
            lane_name(attempt),
            output.status.code(),
            worker_exit,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn wedge_cmd(cycle: usize, attempt: usize) -> String {
    format!(
        "mkdir {} {} 2>nul & > src\\shared.txt echo {} & > {} echo {} & > {} echo {}",
        win_parent(&widget_path(cycle, attempt)),
        win_parent(&note_path(cycle, attempt)),
        shared_value(cycle, attempt),
        win_path(&widget_path(cycle, attempt)),
        widget_value(cycle, attempt),
        win_path(&note_path(cycle, attempt)),
        note_value(cycle, attempt)
    )
}

fn promote_worktree_selection(
    worktrees: &[PathBuf],
    base: &Path,
    cycle: usize,
    attempts: usize,
) -> u64 {
    selected_paths(cycle, attempts)
        .into_iter()
        .map(|(attempt, path)| {
            let source = worktrees[attempt].join(&path);
            let target = base.join(&path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::copy(source, &target).unwrap();
            fs::metadata(target).unwrap().len()
        })
        .sum()
}

fn promote_lane_selection(lane_bin: &Path, root: &Path, cycle: usize, attempts: usize) -> u64 {
    selected_paths(cycle, attempts)
        .into_iter()
        .map(|(attempt, path)| {
            run_checked(Command::new(lane_bin).arg("--repo-root").arg(root).args([
                "promote",
                &lane_name(attempt),
                &path,
                "--json",
            ]));
            fs::metadata(root.join(path)).unwrap().len()
        })
        .sum()
}

fn selected_paths(cycle: usize, attempts: usize) -> Vec<(usize, String)> {
    let shared = winner_attempt(cycle, attempts);
    let widget = (shared + 5) % attempts;
    let note = (shared + 11) % attempts;
    vec![
        (shared, "src/shared.txt".to_owned()),
        (widget, widget_path(cycle, widget)),
        (note, note_path(cycle, note)),
    ]
}

fn git_changed_path_count(root: &Path) -> usize {
    let stdout = run_checked(Command::new("git").arg("-C").arg(root).args([
        "status",
        "--short",
        "--untracked-files=all",
    ]));
    String::from_utf8(stdout)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
}

fn lane_changed_path_count(lane_bin: &Path, root: &Path, attempt: usize) -> usize {
    let stdout = run_checked(Command::new(lane_bin).arg("--repo-root").arg(root).args([
        "changes",
        &lane_name(attempt),
        "--json",
    ]));
    let value: Value = serde_json::from_slice(&stdout).unwrap();
    value["changes"].as_array().unwrap().len()
}

fn commit_cycle(root: &Path, cycle: usize) {
    run_checked(Command::new("git").arg("-C").arg(root).args(["add", "."]));
    run_checked(Command::new("git").arg("-C").arg(root).args([
        "commit",
        "-q",
        "-m",
        &format!("wedge cycle {cycle}"),
    ]));
}

fn summarize_wedge_cycles(cycles: &[WedgeCycleMetrics]) -> WedgeCycleSummary {
    WedgeCycleSummary {
        count: cycles.len(),
        median_total_ms: median(cycles.iter().map(|cycle| cycle.total_ms).collect()),
        average_total_ms: average(cycles.iter().map(|cycle| cycle.total_ms).collect()),
        median_worker_ms: median(cycles.iter().map(|cycle| cycle.worker_ms).collect()),
        median_compare_ms: median(cycles.iter().map(|cycle| cycle.compare_ms).collect()),
        median_promote_ms: median(cycles.iter().map(|cycle| cycle.promote_ms).collect()),
        median_checkpoint_ms: median(cycles.iter().map(|cycle| cycle.checkpoint_ms).collect()),
        median_reset_ms: median(cycles.iter().map(|cycle| cycle.reset_ms).collect()),
        median_active_bytes: median(
            cycles
                .iter()
                .map(|cycle| cycle.active_fs.total_bytes)
                .collect(),
        ),
        median_active_files: median(
            cycles
                .iter()
                .map(|cycle| cycle.active_fs.file_count)
                .collect(),
        ),
        median_active_dirs: median(
            cycles
                .iter()
                .map(|cycle| cycle.active_fs.dir_count)
                .collect(),
        ),
    }
}

fn max_fs(left: FsMetrics, right: FsMetrics) -> FsMetrics {
    if right.total_bytes > left.total_bytes {
        right
    } else {
        left
    }
}

fn expected_shared(cycle: usize, attempts: usize) -> String {
    shared_value(cycle, winner_attempt(cycle, attempts))
}

fn winner_attempt(cycle: usize, attempts: usize) -> usize {
    (cycle * 7 + 2) % attempts
}

fn shared_value(cycle: usize, attempt: usize) -> String {
    format!("cycle-{cycle}-attempt-{attempt:02}")
}

fn widget_value(cycle: usize, attempt: usize) -> String {
    format!("widget-{cycle}-{attempt:02}")
}

fn note_value(cycle: usize, attempt: usize) -> String {
    format!("note-{cycle}-{attempt:02}")
}

fn lane_name(attempt: usize) -> String {
    format!("attempt-{attempt:02}")
}

fn widget_path(cycle: usize, attempt: usize) -> String {
    format!("src/widgets/cycle-{cycle}/widget-{attempt:02}.txt")
}

fn note_path(cycle: usize, attempt: usize) -> String {
    format!("src/notes/cycle-{cycle}/note-{attempt:02}.txt")
}

fn win_path(path: &str) -> String {
    path.replace('/', "\\")
}

fn win_parent(path: &str) -> String {
    let parent = path.rsplit_once('/').unwrap().0;
    win_path(parent)
}

fn read_trimmed(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .unwrap()
        .trim_start_matches('\u{feff}')
        .trim_end()
        .to_owned()
}
