use std::path::Path;

pub(crate) fn path_label(path: impl AsRef<Path>) -> String {
    display_path(path.as_ref())
}

#[cfg(windows)]
fn display_path(path: &Path) -> String {
    let label = path.display().to_string();
    if let Some(path) = label.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else if let Some(path) = label.strip_prefix(r"\\?\") {
        path.to_owned()
    } else {
        label
    }
}

#[cfg(not(windows))]
fn display_path(path: &Path) -> String {
    path.display().to_string()
}
