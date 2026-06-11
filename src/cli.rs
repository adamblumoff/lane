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
    #[command(about = "Run a command in a lane through a virtual mounted lane view")]
    Exec {
        lane: String,
        #[arg(long)]
        observe: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = "Review lane work across every lane or one lane")]
    Review {
        #[arg(long)]
        human: bool,
        lane: Option<String>,
    },
    #[command(about = "Run N isolated attempts for the same command")]
    Try {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 5)]
        attempts: usize,
        #[arg(long)]
        observe: bool,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = "Run a verification command across every attempt in a run")]
    Check {
        run: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    #[command(about = "Compare attempts, checks, and lane review state for a run")]
    Compare {
        run: String,
        #[arg(long)]
        human: bool,
    },
    #[command(about = "Show one lane operation with base and inserted byte previews")]
    ShowOp {
        lane: String,
        path: String,
        op_id: String,
    },
    #[command(about = "Resolve and promote one lane operation from replacement bytes")]
    ResolveOp {
        lane: String,
        path: String,
        op_id: String,
        #[arg(long = "with-file", value_name = "PATH")]
        with_file: PathBuf,
    },
    #[command(about = "Show a text diff for a lane")]
    Diff { lane: String, paths: Vec<String> },
    #[command(about = "Promote selected lane operations into the normal repo")]
    PromoteOps {
        lane: String,
        path: String,
        #[arg(required = true)]
        ops: Vec<String>,
    },
    #[command(about = "Promote every non-conflicting operation in a lane")]
    PromoteClean { lane: String },
    #[command(about = "Remove a lane and its private changes")]
    Discard { lane: String },
    #[command(about = "Validate lane storage and report repairable state")]
    Doctor,
}

pub fn run() -> CliResult<ExitCode> {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> CliResult<ExitCode> {
    let repo_root = repo::repo_root(cli.repo_root)?;
    match cli.command {
        Command::Exec {
            lane,
            observe,
            command,
        } => commands::exec(&repo_root, &lane, observe, &command),
        Command::Review { human, lane } => {
            commands::review(&repo_root, lane.as_deref(), human).map(|()| ExitCode::SUCCESS)
        }
        Command::Try {
            name,
            attempts,
            observe,
            command,
        } => orchestrate::try_run(&repo_root, &name, attempts, observe, &command),
        Command::Check { run, name, command } => {
            orchestrate::check(&repo_root, &run, name.as_deref(), &command)
        }
        Command::Compare { run, human } => {
            orchestrate::compare(&repo_root, &run, human).map(|()| ExitCode::SUCCESS)
        }
        Command::ShowOp { lane, path, op_id } => {
            commands::show_op(&repo_root, &lane, &path, &op_id).map(|()| ExitCode::SUCCESS)
        }
        Command::ResolveOp {
            lane,
            path,
            op_id,
            with_file,
        } => commands::resolve_op(&repo_root, &lane, &path, &op_id, &with_file)
            .map(|()| ExitCode::SUCCESS),
        Command::Diff { lane, paths } => {
            commands::diff(&repo_root, &lane, paths).map(|()| ExitCode::SUCCESS)
        }
        Command::PromoteOps { lane, path, ops } => {
            commands::promote_ops(&repo_root, &lane, &path, &ops).map(|()| ExitCode::SUCCESS)
        }
        Command::PromoteClean { lane } => {
            commands::promote_clean(&repo_root, &lane).map(|()| ExitCode::SUCCESS)
        }
        Command::Discard { lane } => {
            commands::discard(&repo_root, &lane).map(|()| ExitCode::SUCCESS)
        }
        Command::Doctor => commands::doctor(&repo_root),
    }
}
