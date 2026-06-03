#[cfg(windows)]
fn main() {
    use std::env;
    use std::path::PathBuf;

    use lane::winfsp_mount::{MountOptions, mount_foreground};
    use winfsp::FspError;

    let mut args = env::args().skip(1);
    let lane = args.next().unwrap_or_else(|| usage());
    let mount_path = args.next().map(PathBuf::from).unwrap_or_else(|| usage());
    let repo_root = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().expect("current dir"));

    if let Err(error) = mount_foreground(MountOptions {
        repo_root,
        lane,
        mount_path,
    }) {
        eprintln!("lane-mount: {}", format_mount_error(&error));
        std::process::exit(1);
    }

    fn format_mount_error(error: &FspError) -> String {
        match error {
            FspError::HRESULT(code) => {
                format!(
                    "HRESULT 0x{:08X}; NTSTATUS 0x{:08X}",
                    *code as u32,
                    error.to_ntstatus() as u32
                )
            }
            FspError::WIN32(code) => {
                format!(
                    "WIN32 0x{code:08X}; NTSTATUS 0x{:08X}",
                    error.to_ntstatus() as u32
                )
            }
            FspError::NTSTATUS(code) => format!("NTSTATUS 0x{:08X}", *code as u32),
            FspError::IO(kind) => {
                format!("IO {kind:?}; NTSTATUS 0x{:08X}", error.to_ntstatus() as u32)
            }
            _ => format!("{error}; NTSTATUS 0x{:08X}", error.to_ntstatus() as u32),
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("lane-mount requires Windows and WinFsp");
    std::process::exit(1);
}

fn usage() -> ! {
    eprintln!("usage: lane-mount <lane> <mount-path> [repo-root]");
    std::process::exit(2);
}
