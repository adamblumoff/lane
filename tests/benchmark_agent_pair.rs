#![cfg(windows)]

mod bench_support;

use std::env;
use std::path::PathBuf;
use std::process::Command;

use bench_support::{
    AgentInfo, DEFAULT_DETERMINISTIC_ROUNDS, DEFAULT_REAL_AGENT_ROUNDS, WorkerKind, assert_report,
    bench_rounds, path_label, resolve_program, run_checked, run_paired_benchmark,
};
use serde_json::json;

#[test]
#[ignore = "benchmark: paired deterministic scripted edits through git worktrees and Lane"]
fn benchmark_deterministic_lane_vs_worktrees() {
    let lane_bin = PathBuf::from(env!("CARGO_BIN_EXE_lane"));
    let report = run_paired_benchmark(
        "deterministic_script",
        bench_rounds("LANE_BENCH_ROUNDS", DEFAULT_DETERMINISTIC_ROUNDS),
        &lane_bin,
        WorkerKind::Scripted,
    );

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert_report(&report);
}

#[test]
#[ignore = "benchmark: launches real codex agents; set LANE_REAL_AGENT_BENCH=1 to run"]
fn benchmark_real_codex_agents_lane_vs_worktrees() {
    if env::var("LANE_REAL_AGENT_BENCH").as_deref() != Ok("1") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "mode": "real_codex_agents",
                "skipped": true,
                "reason": "set LANE_REAL_AGENT_BENCH=1 to launch real codex workers"
            }))
            .unwrap()
        );
        return;
    }

    let lane_bin = PathBuf::from(env!("CARGO_BIN_EXE_lane"));
    let codex_bin = resolve_program("codex");
    let codex_version = String::from_utf8(run_checked(Command::new(&codex_bin).arg("--version")))
        .unwrap()
        .trim()
        .to_owned();
    let mut report = run_paired_benchmark(
        "real_codex_agents",
        bench_rounds("LANE_REAL_BENCH_ROUNDS", DEFAULT_REAL_AGENT_ROUNDS),
        &lane_bin,
        WorkerKind::Codex {
            program: codex_bin.clone(),
        },
    );
    report.agent = Some(AgentInfo {
        program: path_label(&codex_bin),
        version: codex_version,
    });

    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert_report(&report);
}
