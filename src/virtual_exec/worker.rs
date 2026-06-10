use std::env;
use std::io::Read;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::thread;

use crate::storage::encode_path_component;

use super::git::{GitView, git_path_label};
use super::observer::ExecObserver;
use super::support::path_label;

pub(super) struct WorkerOutput {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) worker_error: Option<String>,
}

pub(super) fn run_virtual_worker(
    program: &str,
    args: &[String],
    lane: &str,
    git_view: Option<&GitView>,
    repo_root: &Path,
    mount_path: &Path,
    observer: ExecObserver,
) -> WorkerOutput {
    let mut command = virtual_command(program, args, lane, git_view, repo_root, mount_path);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    match command.spawn() {
        Ok(mut child) => {
            let streams = WorkerStreamCaptures::start(&mut child, observer);
            let (exit_code, worker_error) = match child.wait() {
                Ok(status) => (status.code(), None),
                Err(error) => (None, Some(error.to_string())),
            };
            let output = streams.finish();
            WorkerOutput {
                exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
                worker_error,
            }
        }
        Err(error) => WorkerOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            worker_error: Some(error.to_string()),
        },
    }
}

struct WorkerStreamCaptures {
    stdout: Option<thread::JoinHandle<Vec<u8>>>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
}

impl WorkerStreamCaptures {
    fn start(child: &mut std::process::Child, observer: ExecObserver) -> Self {
        Self {
            stdout: child
                .stdout
                .take()
                .map(|stream| capture_worker_stream(stream, "stdout", observer.clone())),
            stderr: child
                .stderr
                .take()
                .map(|stream| capture_worker_stream(stream, "stderr", observer)),
        }
    }

    fn finish(self) -> CapturedWorkerOutput {
        CapturedWorkerOutput {
            stdout: join_worker_stream(self.stdout),
            stderr: join_worker_stream(self.stderr),
        }
    }
}

struct CapturedWorkerOutput {
    stdout: String,
    stderr: String,
}

fn capture_worker_stream<R: Read + Send + 'static>(
    mut stream: R,
    stream_name: &'static str,
    observer: ExecObserver,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut captured = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(len) => {
                    captured.extend_from_slice(&buffer[..len]);
                    observer.mirror_child_stream_chunk(stream_name, &buffer[..len]);
                }
                Err(error) => {
                    observer.event(format_args!("failed to read worker {stream_name}: {error}"));
                    break;
                }
            }
        }
        captured
    })
}

fn join_worker_stream(handle: Option<thread::JoinHandle<Vec<u8>>>) -> String {
    let Some(handle) = handle else {
        return String::new();
    };
    match handle.join() {
        Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        Err(_) => String::new(),
    }
}

fn virtual_command<'a>(
    program: &'a str,
    args: &'a [String],
    lane: &'a str,
    git_view: Option<&'a GitView>,
    repo_root: &'a Path,
    mount_path: &'a Path,
) -> ProcessCommand {
    let mount_label = path_label(mount_path);
    let git_work_tree = git_path_label(mount_path);
    let cargo_target_dir = repo_root
        .join("target")
        .join("lane-exec")
        .join(encode_path_component(lane));
    let mut command = ProcessCommand::new(resolve_program(program));
    command
        .args(args)
        .current_dir(mount_path)
        .env("LANE_ID", lane)
        .env("LANE_REPO_ROOT", &mount_label)
        .env("LANE_VIEW_ROOT", &mount_label)
        .env("LANE_EXEC_MODE", "virtual_mount")
        .env("CARGO_TARGET_DIR", cargo_target_dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "safe.directory")
        .env("GIT_CONFIG_VALUE_0", &git_work_tree)
        .env("GIT_CONFIG_KEY_1", "core.worktree")
        .env("GIT_CONFIG_VALUE_1", &git_work_tree)
        .env("GIT_CONFIG_KEY_2", "core.bare")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env_remove("LANE_STORAGE_PATH");
    if let Some(git_view) = git_view {
        command
            .env("GIT_DIR", git_path_label(git_view.path()))
            .env("GIT_WORK_TREE", git_work_tree);
    }
    command
}

pub(super) fn command_label(program: &str, args: &[String]) -> String {
    std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .map(|part| {
            if part.contains(char::is_whitespace) {
                format!("{part:?}")
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_program(program: &str) -> PathBuf {
    let path = Path::new(program);
    if path.components().any(|component| {
        matches!(
            component,
            PathComponent::RootDir | PathComponent::Prefix(_) | PathComponent::ParentDir
        )
    }) {
        return path.to_path_buf();
    }
    if path.components().count() > 1 {
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

    let Some(paths) = env::var_os("PATH") else {
        return path.to_path_buf();
    };
    let names = if path.extension().is_some() {
        vec![program.to_owned()]
    } else {
        extensions
            .iter()
            .map(|extension| format!("{program}{extension}"))
            .collect::<Vec<_>>()
    };
    for directory in env::split_paths(&paths) {
        for name in &names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}
