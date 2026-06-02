use lane::{LaneError, LaneRepo};

const PATH: &str = "src/example.ts";
const BASE: &[u8] = b"export const mode = 'base';\n";

#[test]
fn lanes_project_normal_file_bytes_without_changing_base() {
    let mut repo = seeded_repo();

    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(PATH, "agent-b", BASE, 21..25, b"safe".to_vec())
        .unwrap();

    assert_eq!(repo.read(PATH, "base", BASE).unwrap(), BASE);
    assert_eq!(
        repo.read(PATH, "agent-a", BASE).unwrap(),
        b"export const mode = 'fast';\n"
    );
    assert_eq!(
        repo.read(PATH, "agent-b", BASE).unwrap(),
        b"export const mode = 'safe';\n"
    );
    assert_eq!(
        repo.read("src/untouched.ts", "agent-a", b"untouched")
            .unwrap(),
        b"untouched"
    );
    assert_eq!(
        repo.read(PATH, "missing", BASE),
        Err(LaneError::LaneMissing("missing".to_owned()))
    );
}

#[test]
fn promote_returns_new_base_and_preserves_other_lane_projections() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(PATH, "agent-b", BASE, 21..25, b"safe".to_vec())
        .unwrap();

    let promoted = repo.promote(PATH, "agent-a", BASE).unwrap();

    assert_eq!(promoted, b"export const mode = 'fast';\n");
    assert_eq!(repo.read(PATH, "base", &promoted).unwrap(), promoted);
    assert_eq!(repo.read(PATH, "agent-a", &promoted).unwrap(), promoted);
    assert_eq!(
        repo.read(PATH, "agent-b", &promoted).unwrap(),
        b"export const mode = 'safe';\n"
    );
}

#[test]
fn untouched_lanes_follow_promoted_base() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    let promoted = repo.promote(PATH, "agent-a", BASE).unwrap();

    assert_eq!(promoted, b"export const mode = 'fast';\n");
    assert_eq!(repo.read(PATH, "agent-b", &promoted).unwrap(), promoted);
}

#[test]
fn promoted_lanes_do_not_follow_later_base_changes() {
    let mut repo = seeded_repo();
    repo.create_lane("badabing").unwrap();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(
        PATH,
        "badabing",
        BASE,
        BASE.len() as u64..BASE.len() as u64,
        b"badabing\n".to_vec(),
    )
    .unwrap();

    let badabing = repo.promote(PATH, "badabing", BASE).unwrap();
    assert_eq!(badabing, b"export const mode = 'base';\nbadabing\n");

    let promoted = repo.promote(PATH, "agent-a", &badabing).unwrap();

    assert_eq!(promoted, b"export const mode = 'fast';\n");
    assert_eq!(repo.read(PATH, "agent-a", &promoted).unwrap(), promoted);
    assert_eq!(repo.read(PATH, "badabing", &promoted).unwrap(), badabing);
}

#[test]
fn projection_rejects_overlays_when_the_normal_file_changed_outside_lane() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    assert_eq!(
        repo.read(PATH, "agent-a", b"export const mode = 'drift';\n"),
        Err(LaneError::BaseChanged {
            path: PATH.to_owned()
        })
    );
}

#[test]
fn failed_first_write_does_not_pin_path_to_old_base() {
    let mut repo = seeded_repo();
    let len = BASE.len() as u64;

    assert_eq!(
        repo.write(PATH, "agent-a", BASE, len + 1..len + 2, b"fast".to_vec()),
        Err(LaneError::RangeOutOfBounds {
            start: len + 1,
            end: len + 2,
            len,
        })
    );

    let changed = b"export const mode = 'changed';\n";
    assert_eq!(repo.read(PATH, "agent-a", changed).unwrap(), changed);
}

#[test]
fn repo_state_round_trips() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    let decoded = LaneRepo::from_bytes(&repo.to_bytes()).unwrap();

    assert_eq!(decoded.read(PATH, "base", BASE).unwrap(), BASE);
    assert_eq!(
        decoded.read(PATH, "agent-a", BASE).unwrap(),
        b"export const mode = 'fast';\n"
    );
}

fn seeded_repo() -> LaneRepo {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    repo.create_lane("agent-b").unwrap();
    repo
}
