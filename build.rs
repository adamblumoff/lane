use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    winfsp_build::build();

    let Some((library_name, dll_name, machine)) = winfsp_target() else {
        return;
    };
    let Ok(install_dir) = winfsp_install_dir() else {
        return;
    };

    let system_lib_dir = install_dir.join("lib");
    if system_lib_dir.join(format!("{library_name}.lib")).exists() {
        println!("cargo:rustc-link-search={}", system_lib_dir.display());
        return;
    }

    let dll_path = install_dir.join("bin").join(dll_name);
    println!("cargo:rerun-if-changed={}", dll_path.display());
    match generate_import_lib(&dll_path, library_name, machine) {
        Ok(import_lib_dir) => {
            println!("cargo:rustc-link-search={}", import_lib_dir.display());
        }
        Err(error) => {
            println!(
                "cargo:warning=failed to generate WinFsp import library from {}: {error}",
                dll_path.display()
            );
        }
    }
}

fn winfsp_target() -> Option<(&'static str, &'static str, &'static str)> {
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => Some(("winfsp-a64", "winfsp-a64.dll", "arm64")),
        Ok("x86_64") => Some(("winfsp-x64", "winfsp-x64.dll", "i386:x86-64")),
        Ok("x86") => Some(("winfsp-x86", "winfsp-x86.dll", "i386")),
        _ => None,
    }
}

fn winfsp_install_dir() -> io::Result<PathBuf> {
    env::var_os("ProgramFiles(x86)")
        .map(PathBuf::from)
        .map(|path| path.join("WinFsp"))
        .filter(|path| path.exists())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "WinFsp install dir not found"))
}

fn generate_import_lib(dll_path: &Path, library_name: &str, machine: &str) -> io::Result<PathBuf> {
    if !dll_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "WinFsp DLL not found",
        ));
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "OUT_DIR environment variable missing",
        )
    })?);
    let import_lib_dir = out_dir.join("winfsp-import-lib");
    fs::create_dir_all(&import_lib_dir)?;

    let readobj = llvm_tool("llvm-readobj");
    let dlltool = llvm_tool("llvm-dlltool");
    let exports_output = Command::new(readobj)
        .arg("--coff-exports")
        .arg(dll_path)
        .output()?;
    if !exports_output.status.success() {
        return Err(io::Error::other("llvm-readobj failed"));
    }

    let exports = String::from_utf8_lossy(&exports_output.stdout)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("Name: "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if exports.is_empty() {
        return Err(io::Error::other("WinFsp DLL exported no symbols"));
    }

    let def_path = import_lib_dir.join(format!("{library_name}.def"));
    let mut def = format!("LIBRARY {library_name}.dll\nEXPORTS\n");
    for export in exports {
        def.push_str(&export);
        def.push('\n');
    }
    fs::write(&def_path, def)?;

    let lib_path = import_lib_dir.join(format!("{library_name}.lib"));
    let status = Command::new(dlltool)
        .args(["-m", machine, "-d"])
        .arg(&def_path)
        .arg("-l")
        .arg(&lib_path)
        .status()?;
    if !status.success() {
        return Err(io::Error::other("llvm-dlltool failed"));
    }
    Ok(import_lib_dir)
}

fn llvm_tool(name: &str) -> PathBuf {
    let program_files = env::var_os("ProgramFiles").map(PathBuf::from);
    program_files
        .map(|root| root.join("LLVM").join("bin").join(format!("{name}.exe")))
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}
