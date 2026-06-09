use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::LaneRepo;
use crate::vfs::{FileWorktree, LaneFs, LaneFsError};

#[test]
fn lane_fs_normalizes_current_dir_paths_and_rejects_unsafe_paths() {
    let temp = TempDir::new();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/example.ts"), b"base").unwrap();

    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    let mut fs_view = LaneFs::new(repo, FileWorktree::new(temp.path()));

    assert_eq!(
        fs_view
            .read_file("agent-a", "./src/example.ts")
            .unwrap()
            .unwrap(),
        b"base"
    );

    for (path, message) in [
        ("", "missing path"),
        (".lane/repo.json", "cannot project lane state files"),
        ("../outside.ts", "path must stay inside the repo"),
    ] {
        assert_bad_path(fs_view.read_file("agent-a", path), message);
    }
    let absolute = temp.path().join("src/example.ts");
    assert_bad_path(
        fs_view.read_file("agent-a", absolute.to_str().unwrap()),
        "path must be repo-relative",
    );
    assert_bad_path(
        fs_view.write_file("agent-a", ".lane/agent.json", b"nope"),
        "cannot project lane state files",
    );
}

#[test]
fn promotion_rolls_back_worktree_when_later_file_write_fails() {
    let temp = TempDir::new();
    fs::create_dir_all(temp.path().join("src/swap")).unwrap();
    fs::create_dir_all(temp.path().join("src/swap/empty-dir/nested")).unwrap();
    fs::write(temp.path().join("src/swap/original.txt"), b"original").unwrap();
    fs::write(temp.path().join("zz-blocked"), b"still a file").unwrap();

    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    let mut fs_view = LaneFs::new(repo, FileWorktree::new(temp.path()));
    fs_view.write_file("agent-a", "a.txt", b"created").unwrap();
    fs_view
        .write_file("agent-a", "src/swap", b"now a file")
        .unwrap();
    fs_view
        .write_file("agent-a", "zz-blocked/nested.txt", b"cannot write")
        .unwrap();

    let selections = ["a.txt", "src/swap", "zz-blocked/nested.txt"]
        .into_iter()
        .map(|path| (path.to_owned(), op_ids(&fs_view, path)))
        .collect::<Vec<_>>();

    let error = fs_view
        .promote_ops_files("agent-a", &selections)
        .unwrap_err();
    assert!(matches!(error, LaneFsError::Io(_)));

    assert!(!temp.path().join("a.txt").exists());
    assert!(temp.path().join("src/swap").is_dir());
    assert!(temp.path().join("src/swap/empty-dir/nested").is_dir());
    assert_eq!(
        fs::read(temp.path().join("src/swap/original.txt")).unwrap(),
        b"original"
    );
    assert_eq!(
        fs::read(temp.path().join("zz-blocked")).unwrap(),
        b"still a file"
    );
    assert_eq!(
        fs_view.changed_paths("agent-a").unwrap(),
        vec!["a.txt", "src/swap", "zz-blocked/nested.txt"]
    );
}

fn assert_bad_path<T: std::fmt::Debug>(result: Result<T, LaneFsError>, message: &str) {
    match result {
        Err(LaneFsError::BadPath(error)) => assert!(
            error.contains(message),
            "expected bad path error to contain {message:?}, got {error:?}"
        ),
        other => panic!("expected bad path error containing {message:?}, got {other:?}"),
    }
}

fn op_ids(fs: &LaneFs, path: &str) -> Vec<String> {
    fs.change_for_path("agent-a", path)
        .unwrap()
        .unwrap_or_else(|| panic!("missing change for {path}"))
        .ops
        .into_iter()
        .map(|op| op.op_id)
        .collect()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "lane-vfs-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
