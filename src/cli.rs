mod commands;
mod error;
mod human_review;
mod orchestrate;
mod output;
mod preview;
mod repo;
mod review;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

pub use error::CliError;
use error::CliResult;

#[derive(Parser, Debug)]
#[command(name = "lane")]
#[command(about = "Run agents in isolated lanes without copying the repo")]
struct Cli {
    #[arg(long, global = true, value_name = "PATH", default_value = ".")]
    repo_root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    #[command(about = "Run isolated work in one lane or multiple attempt lanes")]
    Run {
        #[arg(value_name = "NAME", help = "Lane id, or run id when using --attempts")]
        name: String,
        #[arg(long, value_name = "N", help = "Create N attempt lanes for this run")]
        attempts: Option<usize>,
        #[arg(
            long,
            help = "Stream worker output to stderr while preserving JSON stdout"
        )]
        observe: bool,
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
    #[command(about = "Run a verification command across every attempt in a run")]
    Check {
        #[arg(value_name = "RUN", help = "Run id created by lane run --attempts")]
        run: String,
        #[arg(long, value_name = "NAME", help = "Name for this check result")]
        name: Option<String>,
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "COMMAND"
        )]
        command: Vec<String>,
    },
    #[command(about = "Review runs, lanes, diffs, or one lane operation")]
    Review {
        #[arg(long, help = "Print human-readable review text instead of JSON")]
        human: bool,
        #[arg(long, help = "List stored runs")]
        history: bool,
        #[arg(long, help = "Show lane text diff; requires a lane target")]
        diff: bool,
        #[arg(value_name = "RUN_OR_LANE", help = "Run id or lane id")]
        target: Option<String>,
        #[arg(
            value_name = "PATH_OR_OP",
            help = "For op detail: <path> <op-id>; with --diff: optional paths"
        )]
        detail: Vec<String>,
    },
    #[command(about = "Accept clean lane work, selected operations, or replacement bytes")]
    Accept {
        #[arg(
            value_name = "LANE_OR_PATH",
            help = "Lane id, or path when using repeated --op selections"
        )]
        target: String,
        #[arg(value_name = "PATH", help = "Path for lane-scoped operation accepts")]
        path: Option<String>,
        #[arg(value_name = "OP_ID", help = "Operation ids for lane-scoped accepts")]
        ops: Vec<String>,
        #[arg(
            long = "op",
            value_name = "LANE:OP",
            help = "Lane-qualified operation for multi-lane replacement accepts"
        )]
        selected_ops: Vec<String>,
        #[arg(
            long = "with-file",
            value_name = "PATH",
            help = "Replacement byte source"
        )]
        with_file: Option<PathBuf>,
    },
    #[command(about = "Discard one lane or one stored run")]
    Discard { target: String },
    #[command(about = "Validate lane storage and report repairable state")]
    Doctor {
        #[arg(long)]
        cleanup: bool,
    },
}

pub fn run() -> CliResult<ExitCode> {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> CliResult<ExitCode> {
    let repo_root = repo::repo_root(cli.repo_root)?;
    match cli.command {
        Command::Run {
            name,
            attempts,
            observe,
            command,
        } => match attempts {
            Some(attempts) => {
                orchestrate::run_attempts(&repo_root, &name, attempts, observe, &command)
            }
            None => commands::run_one(&repo_root, &name, observe, &command),
        },
        Command::Check { run, name, command } => {
            orchestrate::check(&repo_root, &run, name.as_deref(), &command)
        }
        Command::Review {
            human,
            history,
            diff,
            target,
            detail,
        } => review_target(&repo_root, human, history, diff, target, detail)
            .map(|()| ExitCode::SUCCESS),
        Command::Accept {
            target,
            path,
            ops,
            selected_ops,
            with_file,
        } => accept_target(&repo_root, &target, path, ops, selected_ops, with_file)
            .map(|()| ExitCode::SUCCESS),
        Command::Discard { target } => {
            discard_target(&repo_root, &target).map(|()| ExitCode::SUCCESS)
        }
        Command::Doctor { cleanup } => {
            if cleanup {
                commands::cleanup_storage(&repo_root).map(|()| ExitCode::SUCCESS)
            } else {
                commands::doctor(&repo_root)
            }
        }
    }
}

fn review_target(
    repo_root: &std::path::Path,
    human: bool,
    history: bool,
    diff: bool,
    target: Option<String>,
    detail: Vec<String>,
) -> CliResult<()> {
    if history {
        if target.is_some() || !detail.is_empty() || diff {
            return Err(CliError::message(
                "review --history cannot be combined with a target, detail args, or --diff",
            ));
        }
        return orchestrate::review_history(repo_root);
    }

    let Some(target) = target else {
        if diff {
            return Err(CliError::message("review --diff requires a lane target"));
        }
        if !detail.is_empty() {
            return Err(CliError::message("review detail requires a target"));
        }
        return commands::review(repo_root, None, human);
    };

    if diff {
        return commands::review_diff(repo_root, &target, detail);
    }

    match detail.as_slice() {
        [] => {
            let is_run = orchestrate::run_exists(repo_root, &target);
            let is_lane = commands::lane_exists(repo_root, &target)?;
            match (is_run, is_lane) {
                (true, true) => Err(CliError::message(format!(
                    "review target {target:?} is both a run and a lane"
                ))),
                (true, false) => orchestrate::review_run(repo_root, &target, human),
                (false, true) => commands::review(repo_root, Some(&target), human),
                (false, false) => Err(CliError::message(format!(
                    "review target {target:?} is neither a run nor a lane"
                ))),
            }
        }
        [path, op_id] => commands::review_op_detail(repo_root, &target, path, op_id),
        _ => Err(CliError::message(
            "review detail accepts either <target> or <lane> <path> <op-id>; use --diff for path diffs",
        )),
    }
}

fn accept_target(
    repo_root: &std::path::Path,
    target: &str,
    path: Option<String>,
    ops: Vec<String>,
    selected_ops: Vec<String>,
    with_file: Option<PathBuf>,
) -> CliResult<()> {
    if !selected_ops.is_empty() {
        if path.is_some() || !ops.is_empty() {
            return Err(CliError::message(
                "accept with --op uses the form: accept <path> --op <lane:op>... --with-file <path>",
            ));
        }
        let Some(with_file) = with_file else {
            return Err(CliError::message("accept with --op requires --with-file"));
        };
        return commands::accept_replacement_ops(repo_root, target, &selected_ops, &with_file);
    }

    let Some(path) = path else {
        if with_file.is_some() {
            return Err(CliError::message(
                "accept <lane> cannot use --with-file without <path> <op-id>",
            ));
        }
        if !ops.is_empty() {
            return Err(CliError::message("accept operation args require a path"));
        }
        return commands::accept_clean(repo_root, target);
    };

    if ops.is_empty() {
        return Err(CliError::message(
            "accept <lane> <path> requires at least one operation id",
        ));
    }

    if let Some(with_file) = with_file {
        if ops.len() != 1 {
            return Err(CliError::message(
                "accept replacement for multiple operations requires --op <lane:op> selections",
            ));
        }
        commands::accept_replacement_op(repo_root, target, &path, &ops[0], &with_file)
    } else {
        commands::accept_ops(repo_root, target, &path, &ops)
    }
}

fn discard_target(repo_root: &std::path::Path, target: &str) -> CliResult<()> {
    let is_run = orchestrate::run_exists(repo_root, target);
    let is_lane = commands::lane_exists(repo_root, target)?;
    match (is_run, is_lane) {
        (true, true) => Err(CliError::message(format!(
            "discard target {target:?} is both a run and a lane"
        ))),
        (true, false) => orchestrate::discard(repo_root, target),
        (false, true) => commands::discard(repo_root, target),
        (false, false) => Err(CliError::message(format!(
            "discard target {target:?} is neither a run nor a lane"
        ))),
    }
}
