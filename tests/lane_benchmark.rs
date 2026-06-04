use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const VARIANTS: usize = 5;
const BASE_FILES: usize = 200;
const WINNER: usize = 2;

#[test]
#[ignore = "benchmark: compares Lane deterministic capture with git worktree baseline"]
fn benchmark_lane_exec_against_git_worktrees() {
    let temp = TempDir::new("lane-bench");

    let lane_root = temp.path().join("lane");
    create_fixture(&lane_root, BASE_FILES);
    init_git_repo(&lane_root);

    let lane_start = Instant::now();
    let lane_attempt_start = Instant::now();
    let lane_jobs = (0..VARIANTS)
        .map(|index| {
            let repo_root = lane_root.clone();
            thread::spawn(move || {
                let lane = format!("lane-{index}");
                let (program, args) = variant_program_args(index);
                run_checked(
                    Command::new(env!("CARGO_BIN_EXE_lane"))
                        .arg("--repo-root")
                        .arg(&repo_root)
                        .args(["exec", &lane, "--", &program])
                        .args(args),
                )
            })
        })
        .collect::<Vec<_>>();
    for job in lane_jobs {
        job.join().unwrap();
    }
    let lane_attempt_ms = elapsed_ms(lane_attempt_start);
    let lane_base_stable_before_promote = read_login(&lane_root) == "export const design = 'base';";

    let lane_compare_start = Instant::now();
    for index in 0..VARIANTS {
        run_checked(
            Command::new(env!("CARGO_BIN_EXE_lane"))
                .arg("--repo-root")
                .arg(&lane_root)
                .args(["diff", &format!("lane-{index}")]),
        );
    }
    let lane_compare_ms = elapsed_ms(lane_compare_start);
    let lane_active_disk_bytes = dir_size(&lane_root);

    let lane_promote_start = Instant::now();
    run_checked(
        Command::new(env!("CARGO_BIN_EXE_lane"))
            .arg("--repo-root")
            .arg(&lane_root)
            .args(["promote-lane", &format!("lane-{WINNER}"), "--json"]),
    );
    let lane_promote_ms = elapsed_ms(lane_promote_start);
    assert_eq!(
        read_login(&lane_root),
        format!("export const design = 'lane-{WINNER}';")
    );
    assert!(lane_root.join(format!("src/marker-{WINNER}.ts")).exists());

    let lane_cleanup_start = Instant::now();
    for index in 0..VARIANTS {
        if index != WINNER {
            run_checked(
                Command::new(env!("CARGO_BIN_EXE_lane"))
                    .arg("--repo-root")
                    .arg(&lane_root)
                    .args(["discard", &format!("lane-{index}"), "--json"]),
            );
        }
    }
    let lane_cleanup_ms = elapsed_ms(lane_cleanup_start);
    let lane_total_ms = elapsed_ms(lane_start);

    let worktree_parent = temp.path().join("worktrees");
    let worktree_base = worktree_parent.join("base");
    create_fixture(&worktree_base, BASE_FILES);
    init_git_repo(&worktree_base);

    let worktree_start = Instant::now();
    let worktree_setup_start = Instant::now();
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
    let worktree_setup_ms = elapsed_ms(worktree_setup_start);

    let worktree_attempt_start = Instant::now();
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
    let worktree_attempt_ms = elapsed_ms(worktree_attempt_start);
    let worktree_base_stable_before_promote =
        read_login(&worktree_base) == "export const design = 'base';";

    let worktree_compare_start = Instant::now();
    for root in &worktree_roots {
        run_checked(Command::new("git").arg("-C").arg(root).args(["diff"]));
    }
    let worktree_compare_ms = elapsed_ms(worktree_compare_start);
    let worktree_active_disk_bytes = dir_size(&worktree_parent);

    let worktree_promote_start = Instant::now();
    copy_variant_to_base(&worktree_roots[WINNER], &worktree_base, WINNER);
    let worktree_promote_ms = elapsed_ms(worktree_promote_start);
    assert_eq!(
        read_login(&worktree_base),
        format!("export const design = 'lane-{WINNER}';")
    );
    assert!(
        worktree_base
            .join(format!("src/marker-{WINNER}.ts"))
            .exists()
    );

    let worktree_cleanup_start = Instant::now();
    for root in &worktree_roots {
        run_checked(
            Command::new("git")
                .arg("-C")
                .arg(&worktree_base)
                .args(["worktree", "remove", "--force"])
                .arg(root),
        );
    }
    let worktree_cleanup_ms = elapsed_ms(worktree_cleanup_start);
    let worktree_total_ms = elapsed_ms(worktree_start);

    let lane_repo_dirs = 1_u64;
    let worktree_repo_dirs = (VARIANTS + 1) as u64;
    let result = json!({
        "variants": VARIANTS,
        "base_files": BASE_FILES,
        "lane": {
            "mode": "deterministic_exec_capture",
            "repo_dirs": lane_repo_dirs,
            "attempt_ms": lane_attempt_ms,
            "compare_ms": lane_compare_ms,
            "promote_ms": lane_promote_ms,
            "cleanup_ms": lane_cleanup_ms,
            "total_ms": lane_total_ms,
            "active_disk_bytes": lane_active_disk_bytes,
            "base_stable_before_promote": lane_base_stable_before_promote,
        },
        "git_worktree": {
            "repo_dirs": worktree_repo_dirs,
            "setup_ms": worktree_setup_ms,
            "attempt_ms": worktree_attempt_ms,
            "compare_ms": worktree_compare_ms,
            "promote_ms": worktree_promote_ms,
            "cleanup_ms": worktree_cleanup_ms,
            "total_ms": worktree_total_ms,
            "active_disk_bytes": worktree_active_disk_bytes,
            "base_stable_before_promote": worktree_base_stable_before_promote,
        },
        "gain": {
            "repo_dirs_removed": worktree_repo_dirs - lane_repo_dirs,
            "wall_time_speedup": ratio(worktree_total_ms, lane_total_ms),
            "disk_bytes_reduction": worktree_active_disk_bytes.saturating_sub(lane_active_disk_bytes),
            "disk_bytes_ratio": ratio(worktree_active_disk_bytes, lane_active_disk_bytes),
        }
    });
    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    assert!(lane_base_stable_before_promote);
    assert!(worktree_base_stable_before_promote);
    assert_eq!(lane_repo_dirs, 1);
    assert_eq!(worktree_repo_dirs, 6);
    assert!(worktree_active_disk_bytes > lane_active_disk_bytes);
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

fn ratio(numerator: u64, denominator: u64) -> Value {
    if denominator == 0 {
        Value::Null
    } else {
        json!((numerator as f64 / denominator as f64 * 100.0).round() / 100.0)
    }
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
