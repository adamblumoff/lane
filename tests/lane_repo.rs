use lane::{LaneError, LaneRepo};

const PATH: &str = "src/example.ts";
const BASE: &[u8] = b"export const mode = 'base';\n";
const SETTINGS_PATH: &str = "src/settings.json";
const SETTINGS_BASE: &[u8] = b"{\"mode\":\"base\"}\n";

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
fn overlay_paths_report_lane_overlays() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(
        SETTINGS_PATH,
        "agent-b",
        SETTINGS_BASE,
        9..13,
        b"safe".to_vec(),
    )
    .unwrap();

    assert_eq!(repo.overlay_paths("agent-a").unwrap(), vec![PATH]);
    assert_eq!(repo.overlay_paths("agent-b").unwrap(), vec![SETTINGS_PATH]);
    assert_eq!(
        repo.overlay_paths("missing"),
        Err(LaneError::LaneMissing("missing".to_owned()))
    );
}

#[test]
fn promote_lane_promotes_every_changed_path_for_lane() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(
        SETTINGS_PATH,
        "agent-a",
        SETTINGS_BASE,
        9..13,
        b"fast".to_vec(),
    )
    .unwrap();
    repo.write(PATH, "agent-b", BASE, 21..25, b"safe".to_vec())
        .unwrap();

    let promoted = repo
        .promote_lane(
            "agent-a",
            vec![
                (PATH.to_owned(), BASE.to_vec()),
                (SETTINGS_PATH.to_owned(), SETTINGS_BASE.to_vec()),
            ],
        )
        .unwrap();

    assert_eq!(
        promoted,
        vec![
            promoted_file(PATH, b"export const mode = 'fast';\n"),
            promoted_file(SETTINGS_PATH, b"{\"mode\":\"fast\"}\n"),
        ]
    );
    assert_eq!(
        repo.read(PATH, "agent-b", b"export const mode = 'fast';\n")
            .unwrap(),
        b"export const mode = 'safe';\n"
    );
    assert_eq!(
        repo.read(SETTINGS_PATH, "agent-b", b"{\"mode\":\"fast\"}\n")
            .unwrap(),
        b"{\"mode\":\"fast\"}\n"
    );
    assert_eq!(
        repo.overlay_paths("agent-a").unwrap(),
        vec![PATH, SETTINGS_PATH]
    );
    assert_eq!(repo.overlay_paths("agent-b").unwrap(), vec![PATH]);
    assert_eq!(
        repo.promote_lane(
            "agent-a",
            vec![
                (PATH.to_owned(), b"export const mode = 'fast';\n".to_vec()),
                (SETTINGS_PATH.to_owned(), b"{\"mode\":\"fast\"}\n".to_vec()),
            ],
        )
        .unwrap(),
        Vec::<lane::PromotedFile>::new()
    );
}

#[test]
fn promote_lane_requires_base_for_every_changed_path() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    assert_eq!(
        repo.promote_lane("agent-a", Vec::<(String, Vec<u8>)>::new()),
        Err(LaneError::BaseMissing {
            path: PATH.to_owned()
        })
    );
}

#[test]
fn failed_promote_lane_does_not_mutate_repo() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(
        SETTINGS_PATH,
        "agent-a",
        SETTINGS_BASE,
        9..13,
        b"fast".to_vec(),
    )
    .unwrap();

    assert_eq!(
        repo.promote_lane(
            "agent-a",
            vec![
                (PATH.to_owned(), BASE.to_vec()),
                (SETTINGS_PATH.to_owned(), b"{\"mode\":\"moved\"}\n".to_vec()),
            ],
        ),
        Err(LaneError::BaseChanged {
            path: SETTINGS_PATH.to_owned()
        })
    );

    assert_eq!(
        repo.read(PATH, "agent-a", BASE).unwrap(),
        b"export const mode = 'fast';\n"
    );
    assert_eq!(
        repo.read(SETTINGS_PATH, "agent-a", SETTINGS_BASE).unwrap(),
        b"{\"mode\":\"fast\"}\n"
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
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), vec![PATH]);
    assert_eq!(repo.overlay_paths("agent-b").unwrap(), vec![PATH]);
}

#[test]
fn replacing_with_base_content_clears_lane_overlay() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    repo.replace(PATH, "agent-a", BASE, BASE.to_vec()).unwrap();

    assert_eq!(repo.read(PATH, "agent-a", BASE).unwrap(), BASE);
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
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

fn promoted_file(path: &str, bytes: &[u8]) -> lane::PromotedFile {
    lane::PromotedFile {
        path: path.to_owned(),
        bytes: bytes.to_vec(),
    }
}
