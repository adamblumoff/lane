use std::process::ExitCode;

fn main() -> ExitCode {
    match lane::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("lane: {error}");
            ExitCode::FAILURE
        }
    }
}
