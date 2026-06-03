use lane::LaneRepo;
use lane::vfs::{DirEntry, DirEntryKind, LaneFs, MemoryWorktree};

const BASE: &[u8] = b"export const mode = 'base';\n";

#[test]
fn subagents_get_isolated_normal_file_views() {
    let mut fs = seeded_fs();

    fs.write_file(
        "agent-a",
        "src/example.ts",
        b"export const mode = 'fast';\n",
    )
    .unwrap();
    fs.write_file(
        "agent-b",
        "src/example.ts",
        b"export const mode = 'safe';\n",
    )
    .unwrap();

    assert_eq!(
        fs.read_file("base", "src/example.ts").unwrap(),
        Some(BASE.to_vec())
    );
    assert_eq!(
        fs.read_file("agent-a", "src/example.ts").unwrap(),
        Some(b"export const mode = 'fast';\n".to_vec())
    );
    assert_eq!(
        fs.read_file("agent-b", "src/example.ts").unwrap(),
        Some(b"export const mode = 'safe';\n".to_vec())
    );
    assert_eq!(fs.worktree().file("src/example.ts"), Some(BASE));
}

#[test]
fn lane_view_tracks_create_delete_rename_and_directories() {
    let mut fs = seeded_fs();

    fs.write_file("agent-a", "src/generated.ts", b"generated\n")
        .unwrap();
    fs.delete_file("agent-a", "src/example.ts").unwrap();
    fs.rename_file("agent-a", "README.md", "docs/README.md")
        .unwrap();

    assert_eq!(fs.read_file("agent-a", "src/example.ts").unwrap(), None);
    assert_eq!(
        fs.read_file("agent-a", "src/generated.ts").unwrap(),
        Some(b"generated\n".to_vec())
    );
    assert_eq!(fs.read_file("agent-a", "README.md").unwrap(), None);
    assert_eq!(
        fs.read_file("agent-a", "docs/README.md").unwrap(),
        Some(b"# Lane\n".to_vec())
    );
    assert_eq!(
        fs.list_dir("agent-a", "").unwrap(),
        vec![dir("docs"), file("package.json"), dir("src"), dir("tests"),]
    );

    assert_eq!(
        fs.read_file("agent-b", "src/example.ts").unwrap(),
        Some(BASE.to_vec())
    );
    assert_eq!(
        fs.read_file("agent-b", "README.md").unwrap(),
        Some(b"# Lane\n".to_vec())
    );
    assert_eq!(fs.read_file("agent-b", "src/generated.ts").unwrap(), None);
    assert_eq!(fs.read_file("agent-b", "docs/README.md").unwrap(), None);
}

#[test]
fn orchestrator_promotes_selected_files_across_lanes() {
    let mut fs = seeded_fs();

    fs.write_file("agent-a", "src/parser.ts", b"broad parser rewrite\n")
        .unwrap();
    fs.write_file("agent-a", "tests/parser.test.ts", b"broad parser tests\n")
        .unwrap();
    fs.write_file("agent-b", "src/parser.ts", b"minimal parser fix\n")
        .unwrap();

    fs.promote_file("agent-b", "src/parser.ts").unwrap();
    fs.promote_file("agent-a", "tests/parser.test.ts").unwrap();

    assert_eq!(
        fs.worktree().file("src/parser.ts"),
        Some(b"minimal parser fix\n".as_slice())
    );
    assert_eq!(
        fs.worktree().file("tests/parser.test.ts"),
        Some(b"broad parser tests\n".as_slice())
    );
    assert_eq!(
        fs.read_file("agent-a", "src/parser.ts").unwrap(),
        Some(b"broad parser rewrite\n".to_vec())
    );
    assert_eq!(
        fs.read_file("agent-b", "tests/parser.test.ts").unwrap(),
        Some(b"broad parser tests\n".to_vec())
    );
}

#[test]
fn temp_file_save_flow_stays_lane_local() {
    let mut fs = seeded_fs();

    fs.write_file(
        "agent-a",
        "src/example.ts.tmp",
        b"saved through temp file\n",
    )
    .unwrap();
    fs.rename_file("agent-a", "src/example.ts.tmp", "src/example.ts")
        .unwrap();

    assert_eq!(
        fs.read_file("agent-a", "src/example.ts").unwrap(),
        Some(b"saved through temp file\n".to_vec())
    );
    assert_eq!(
        fs.read_file("agent-b", "src/example.ts").unwrap(),
        Some(BASE.to_vec())
    );
    assert_eq!(fs.read_file("agent-a", "src/example.ts.tmp").unwrap(), None);
    assert_eq!(fs.worktree().file("src/example.ts"), Some(BASE));
}

fn seeded_fs() -> LaneFs<MemoryWorktree> {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    repo.create_lane("agent-b").unwrap();
    LaneFs::new(
        repo,
        MemoryWorktree::new([
            ("README.md", b"# Lane\n".to_vec()),
            ("package.json", b"{}\n".to_vec()),
            ("src/example.ts", BASE.to_vec()),
            ("src/parser.ts", b"base parser\n".to_vec()),
            ("tests/parser.test.ts", b"base parser tests\n".to_vec()),
        ]),
    )
}

fn dir(name: &str) -> DirEntry {
    DirEntry {
        name: name.to_owned(),
        kind: DirEntryKind::Directory,
    }
}

fn file(name: &str) -> DirEntry {
    DirEntry {
        name: name.to_owned(),
        kind: DirEntryKind::File,
    }
}
