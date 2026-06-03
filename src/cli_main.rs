use std::process::ExitCode;

fn main() -> ExitCode {
    match lane::cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("lane: {error}");
            ExitCode::FAILURE
        }
    }
}
