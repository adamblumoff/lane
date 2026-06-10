mod fs;
mod git;
mod mount;
mod nodes;
mod observer;
mod state;
mod support;
mod types;
mod worker;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::LaneExecState;

use git::prepare_git_view;
use mount::start_mount;
use observer::ExecObserver;
use support::{elapsed_ms, path_label};
pub(crate) use types::{VirtualExecError, VirtualExecOptions, VirtualLaneRun};
use types::{VirtualExecOutput, VirtualExecTimings, VirtualExecWarning, VirtualFsMetrics};
use worker::{command_label, run_virtual_worker};

const STORAGE_PATH: &str = ".lane";

pub(crate) fn run_virtual_lane(
    repo_root: &Path,
    lane: &str,
    command: &[String],
    options: VirtualExecOptions,
) -> Result<VirtualLaneRun, VirtualExecError> {
    let total_start = Instant::now();
    let (program, args) = command
        .split_first()
        .ok_or_else(|| VirtualExecError::message("missing command for lane exec"))?;
    let storage_path = repo_root.join(STORAGE_PATH);
    let metrics = Arc::new(VirtualFsMetrics::default());
    let observer = ExecObserver::new(lane, options.observe);

    let setup_start = Instant::now();
    observer.event("preparing git view");
    let git_view = prepare_git_view(repo_root)?;
    winfsp_wrs::init().map_err(|error| VirtualExecError::message(error.to_string()))?;
    let mount_start = Instant::now();
    observer.event("mounting virtual lane view");
    let (mount_point, mount, state) = start_mount(repo_root, &storage_path, lane, metrics.clone())?;
    let mount_ms = elapsed_ms(mount_start);
    let pre_worker_lock_ms = elapsed_ms(setup_start);
    observer.event(format_args!(
        "mounted in {mount_ms}ms at {}",
        mount_point.workspace_path.display()
    ));

    let worker_start = Instant::now();
    observer.event(format_args!(
        "starting worker: {}",
        command_label(program, args)
    ));
    let worker = run_virtual_worker(
        program,
        args,
        lane,
        git_view.as_ref(),
        repo_root,
        &mount_point.workspace_path,
        observer.clone(),
    );
    let worker_ms = elapsed_ms(worker_start);
    observer.event(format_args!(
        "worker finished in {worker_ms}ms with exit {:?}",
        worker.exit_code
    ));

    let stop_start = Instant::now();
    observer.event("unmounting virtual lane view");
    mount.stop();
    let unmount_ms = elapsed_ms(stop_start);

    let collect_start = Instant::now();
    let projected_paths = state.projected_paths()?;
    let changed_paths = state.worker_changed_paths()?;
    observer.event(format_args!(
        "persisting {} changed path entries",
        changed_paths.len()
    ));
    state.flush()?;
    let changes = state.collect_changes()?;
    let exec_state = LaneExecState::new(
        worker.exit_code,
        worker.worker_error.clone(),
        &worker.stdout,
        &worker.stderr,
        changed_paths.clone(),
    );
    let warnings = last_exec_warnings(state.record_last_exec(exec_state));
    let post_worker_lock_ms = elapsed_ms(collect_start);
    let snapshot = metrics.snapshot();
    observer.event(format_args!(
        "storage done in {post_worker_lock_ms}ms; lock wait {}ms, lock held {}ms, writes {}",
        snapshot.storage_lock_wait_ms, snapshot.storage_lock_held_ms, snapshot.storage_write_ops
    ));
    let failed = worker.exit_code != Some(0) || worker.worker_error.is_some();

    let output = VirtualExecOutput {
        lane: lane.to_owned(),
        repo_root: path_label(repo_root),
        storage_path: path_label(&storage_path),
        workspace_root: path_label(&mount_point.workspace_path),
        mount_path: path_label(&mount_point.workspace_path),
        mode: "virtual_mount",
        projected_paths,
        exit_code: worker.exit_code,
        stdout: worker.stdout,
        stderr: worker.stderr,
        worker_error: worker.worker_error,
        changed_paths,
        timings: VirtualExecTimings {
            total_ms: elapsed_ms(total_start),
            lock_wait_ms: snapshot.storage_lock_wait_ms,
            lock_held_ms: snapshot.storage_lock_held_ms,
            storage_lock_wait_ms: snapshot.storage_lock_wait_ms,
            storage_lock_held_ms: snapshot.storage_lock_held_ms,
            pre_worker_lock_ms,
            worker_ms,
            post_worker_lock_ms,
            mount_ms,
            unmount_ms,
            storage_write_ops: snapshot.storage_write_ops,
        },
        changes,
        warnings,
    };

    Ok(VirtualLaneRun { output, failed })
}

fn last_exec_warnings(result: Result<(), VirtualExecError>) -> Vec<VirtualExecWarning> {
    match result {
        Ok(()) => Vec::new(),
        Err(error) => vec![VirtualExecWarning {
            kind: "last_exec_not_recorded",
            message: format!("failed to record advisory last_exec metadata: {error}"),
        }],
    }
}
