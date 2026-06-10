use std::path::Path;
use std::time::Instant;

pub(super) fn path_label(path: impl AsRef<Path>) -> String {
    let label = path.as_ref().display().to_string();
    if let Some(path) = label.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = label.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        label
    }
}

pub(super) fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}
