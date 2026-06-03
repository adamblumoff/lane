fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let dll = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "winfsp-x64.dll",
        Ok("x86") => "winfsp-x86.dll",
        Ok("aarch64") => "winfsp-a64.dll",
        _ => return,
    };
    println!("cargo:rustc-link-lib=dylib=delayimp");
    println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
}
