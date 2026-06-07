use super::{LaneError, LaneRepo, PromotedFile};
use sha2::{Digest, Sha256};
use std::ops::Range;

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
                (PATH.to_owned(), Some(BASE.to_vec())),
                (SETTINGS_PATH.to_owned(), Some(SETTINGS_BASE.to_vec())),
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
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
    assert_eq!(repo.overlay_paths("agent-b").unwrap(), vec![PATH]);
    assert_eq!(
        repo.promote_lane(
            "agent-a",
            vec![
                (
                    PATH.to_owned(),
                    Some(b"export const mode = 'fast';\n".to_vec()),
                ),
                (
                    SETTINGS_PATH.to_owned(),
                    Some(b"{\"mode\":\"fast\"}\n".to_vec()),
                ),
            ],
        )
        .unwrap(),
        Vec::<PromotedFile>::new()
    );
}

#[test]
fn promote_lane_requires_base_for_every_changed_path() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    assert_eq!(
        repo.promote_lane("agent-a", Vec::<(String, Option<Vec<u8>>)>::new()),
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
                (PATH.to_owned(), Some(BASE.to_vec())),
                (
                    SETTINGS_PATH.to_owned(),
                    Some(b"{\"mode\":\"moved\"}\n".to_vec()),
                ),
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
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
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
fn non_overlapping_promoted_lanes_follow_later_base_changes() {
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

    assert_eq!(promoted, b"export const mode = 'fast';\nbadabing\n");
    assert_eq!(repo.read(PATH, "agent-a", &promoted).unwrap(), promoted);
    assert_eq!(repo.read(PATH, "badabing", &promoted).unwrap(), promoted);
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
fn created_and_deleted_paths_are_lane_local() {
    let mut repo = seeded_repo();
    repo.replace_path(
        "src/new.ts",
        "agent-a",
        None,
        Some(b"export const created = true;\n".to_vec()),
    )
    .unwrap();
    repo.delete_path(PATH, "agent-a", Some(BASE)).unwrap();

    assert_eq!(repo.read_path("src/new.ts", "base", None).unwrap(), None);
    assert_eq!(
        repo.read_path("src/new.ts", "agent-a", None).unwrap(),
        Some(b"export const created = true;\n".to_vec())
    );
    assert_eq!(repo.read_path("src/new.ts", "agent-b", None).unwrap(), None);
    assert_eq!(
        repo.read_path(PATH, "base", Some(BASE)).unwrap(),
        Some(BASE.to_vec())
    );
    assert_eq!(repo.read_path(PATH, "agent-a", Some(BASE)).unwrap(), None);
    assert_eq!(
        repo.read_path(PATH, "agent-b", Some(BASE)).unwrap(),
        Some(BASE.to_vec())
    );
}

#[test]
fn created_and_deleted_paths_round_trip_through_storage() {
    let mut repo = seeded_repo();
    repo.replace_path("src/new.ts", "agent-a", None, Some(b"new\n".to_vec()))
        .unwrap();
    repo.delete_path(PATH, "agent-b", Some(BASE)).unwrap();

    let decoded = LaneRepo::from_bytes(&repo.to_bytes()).unwrap();

    assert_eq!(
        decoded.read_path("src/new.ts", "agent-a", None).unwrap(),
        Some(b"new\n".to_vec())
    );
    assert_eq!(
        decoded.read_path(PATH, "agent-b", Some(BASE)).unwrap(),
        None
    );
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

#[test]
fn repo_state_serializes_v5_sha256_base_fingerprint() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    let encoded = repo.to_bytes();
    let mut expected = [0; 32];
    expected.copy_from_slice(&Sha256::digest(BASE));

    assert!(encoded.starts_with(b"LANEREPO\0\0\0\x05"));
    assert!(
        encoded
            .windows(expected.len())
            .any(|window| window == expected.as_slice())
    );
}

#[test]
fn snapshot_replacement_is_stored_as_byte_ops() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\ngamma=3\n";
    let edited = b"alpha=10\nbeta=2\ngamma=30\n";

    repo.replace("src/math.txt", "agent-a", base, edited.to_vec())
        .unwrap();

    let ops = repo
        .change_ops("src/math.txt", "agent-a", Some(base))
        .unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].base_start, 7);
    assert_eq!(ops[0].base_end, 7);
    assert_eq!(ops[0].inserted_len, 1);
    assert_eq!(ops[1].base_start, 22);
    assert_eq!(ops[1].base_end, 22);
    assert_eq!(ops[1].inserted_len, 1);
    assert_eq!(repo.read("src/math.txt", "agent-a", base).unwrap(), edited);
}

#[test]
fn non_overlapping_same_file_ops_compose_after_promotion() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\n";
    repo.write("src/math.txt", "agent-a", base, 6..7, b"10".to_vec())
        .unwrap();
    repo.write("src/math.txt", "agent-b", base, 13..14, b"20".to_vec())
        .unwrap();

    let promoted = repo.promote("src/math.txt", "agent-a", base).unwrap();

    assert_eq!(promoted, b"alpha=10\nbeta=2\n");
    assert_eq!(
        repo.read("src/math.txt", "agent-b", &promoted).unwrap(),
        b"alpha=10\nbeta=20\n"
    );
    assert_eq!(
        repo.change_ops("src/math.txt", "agent-b", Some(&promoted))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn selected_ops_promote_without_promoting_the_whole_lane_file() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\ngamma=3\n";
    let edited = b"alpha=10\nbeta=2\ngamma=30\n";
    repo.replace("src/math.txt", "agent-a", base, edited.to_vec())
        .unwrap();
    repo.write("src/math.txt", "agent-b", base, 13..14, b"20".to_vec())
        .unwrap();

    let ops = repo
        .change_ops("src/math.txt", "agent-a", Some(base))
        .unwrap();
    assert_eq!(ops.len(), 2);
    let promoted = repo
        .promote_ops("src/math.txt", "agent-a", base, &[ops[0].op_id.clone()])
        .unwrap();

    assert_eq!(promoted, b"alpha=10\nbeta=2\ngamma=3\n");
    assert_eq!(
        repo.read("src/math.txt", "agent-a", &promoted).unwrap(),
        edited
    );
    assert_eq!(
        repo.read("src/math.txt", "agent-b", &promoted).unwrap(),
        b"alpha=10\nbeta=20\ngamma=3\n"
    );
    assert_eq!(
        repo.change_ops("src/math.txt", "agent-a", Some(&promoted))
            .unwrap()
            .len(),
        1
    );
    let remaining_a_ops = repo
        .change_ops("src/math.txt", "agent-a", Some(&promoted))
        .unwrap();
    assert_eq!(remaining_a_ops[0].op_id, "agent-a:2");
    assert_eq!(remaining_a_ops[0].base_start, 23);
    assert_eq!(
        remaining_a_ops[0].order_key,
        "00000000000000000023:j:agent-a:00000000000000000002"
    );
    let remaining_b_ops = repo
        .change_ops("src/math.txt", "agent-b", Some(&promoted))
        .unwrap();
    assert_eq!(remaining_b_ops[0].op_id, "agent-b:1");
    assert_eq!(remaining_b_ops[0].base_start, 15);
}

#[test]
fn missing_selected_op_does_not_mutate_repo() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\n";
    repo.write("src/math.txt", "agent-a", base, 6..7, b"10".to_vec())
        .unwrap();

    assert_eq!(
        repo.promote_ops("src/math.txt", "agent-a", base, &["agent-a:999".to_owned()],),
        Err(LaneError::OperationMissing {
            path: "src/math.txt".to_owned(),
            op_id: "agent-a:999".to_owned()
        })
    );
    assert_eq!(
        repo.read("src/math.txt", "agent-a", base).unwrap(),
        b"alpha=10\nbeta=2\n"
    );
}

#[test]
fn resolve_op_promotes_replacement_bytes_and_preserves_other_lane_alternative() {
    let mut repo = seeded_repo();
    let base = b"a=1\nb=2\nc=3\n";
    repo.replace("src/vars.txt", "agent-a", base, b"a=A\nb=B\nc=C\n".to_vec())
        .unwrap();
    repo.replace("src/vars.txt", "agent-b", base, b"a=1\nb=X\nc=3\n".to_vec())
        .unwrap();

    let agent_a_ops = repo
        .change_ops("src/vars.txt", "agent-a", Some(base))
        .unwrap();
    assert_eq!(agent_a_ops.len(), 3);
    let clean_op_ids = vec![agent_a_ops[0].op_id.clone(), agent_a_ops[2].op_id.clone()];
    let promoted_clean = repo
        .promote_ops("src/vars.txt", "agent-a", base, &clean_op_ids)
        .unwrap();
    assert_eq!(promoted_clean, b"a=A\nb=2\nc=C\n");

    let detail = repo
        .op_detail(
            "src/vars.txt",
            "agent-a",
            Some(&promoted_clean),
            "agent-a:2",
        )
        .unwrap();
    assert_eq!(detail.base, b"2");
    assert_eq!(detail.inserted, b"B");
    assert_eq!(detail.summary.conflicts_with, vec!["agent-b".to_owned()]);

    let resolved = repo
        .resolve_op_path(
            "src/vars.txt",
            "agent-a",
            Some(&promoted_clean),
            "agent-a:2",
            b"Y".to_vec(),
        )
        .unwrap()
        .unwrap();

    assert_eq!(resolved, b"a=A\nb=Y\nc=C\n");
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
    assert_eq!(
        repo.read("src/vars.txt", "agent-b", &resolved).unwrap(),
        b"a=A\nb=X\nc=C\n"
    );
}

#[test]
fn overlapping_same_file_ops_remain_alternatives_after_promotion() {
    let mut repo = seeded_repo();
    let base = b"mode=base\n";
    repo.write("src/mode.txt", "agent-a", base, 5..9, b"fast".to_vec())
        .unwrap();
    repo.write("src/mode.txt", "agent-b", base, 5..9, b"safe".to_vec())
        .unwrap();

    let before = repo
        .change_ops("src/mode.txt", "agent-a", Some(base))
        .unwrap();
    assert_eq!(before[0].conflicts_with, vec!["agent-b".to_owned()]);

    let promoted = repo.promote("src/mode.txt", "agent-a", base).unwrap();

    assert_eq!(promoted, b"mode=fast\n");
    assert_eq!(
        repo.read("src/mode.txt", "agent-b", &promoted).unwrap(),
        b"mode=safe\n"
    );
    assert!(
        !repo
            .change_ops("src/mode.txt", "agent-b", Some(&promoted))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn same_position_inserts_have_deterministic_order_without_conflict() {
    let mut repo = seeded_repo();
    let base = b"tail\n";
    repo.write(
        "src/imports.txt",
        "agent-a",
        base,
        0..0,
        b"use a;\n".to_vec(),
    )
    .unwrap();
    repo.write(
        "src/imports.txt",
        "agent-b",
        base,
        0..0,
        b"use b;\n".to_vec(),
    )
    .unwrap();

    assert!(
        repo.change_ops("src/imports.txt", "agent-a", Some(base))
            .unwrap()[0]
            .conflicts_with
            .is_empty()
    );

    let promoted = repo.promote("src/imports.txt", "agent-a", base).unwrap();

    assert_eq!(promoted, b"use a;\ntail\n");
    assert_eq!(
        repo.read("src/imports.txt", "agent-b", &promoted).unwrap(),
        b"use a;\nuse b;\ntail\n"
    );
}

#[test]
fn same_position_inserts_into_empty_file_are_not_create_conflicts() {
    let mut repo = seeded_repo();
    let base = b"";
    repo.write("src/empty.txt", "agent-a", base, 0..0, b"a".to_vec())
        .unwrap();
    repo.write("src/empty.txt", "agent-b", base, 0..0, b"b".to_vec())
        .unwrap();

    assert!(
        repo.change_ops("src/empty.txt", "agent-a", Some(base))
            .unwrap()[0]
            .conflicts_with
            .is_empty()
    );

    let promoted = repo.promote("src/empty.txt", "agent-a", base).unwrap();

    assert_eq!(promoted, b"a");
    assert_eq!(
        repo.read("src/empty.txt", "agent-b", &promoted).unwrap(),
        b"ab"
    );
}

fn seeded_repo() -> LaneRepo {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    repo.create_lane("agent-b").unwrap();
    repo
}

fn promoted_file(path: &str, bytes: &[u8]) -> PromotedFile {
    PromotedFile {
        path: path.to_owned(),
        bytes: Some(bytes.to_vec()),
    }
}

trait RepoTestExt {
    fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError>;
    fn write(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
        replacement: Vec<u8>,
    ) -> Result<(), LaneError>;
    fn replace(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        content: Vec<u8>,
    ) -> Result<(), LaneError>;
    fn promote(&mut self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError>;
    fn promote_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        op_ids: &[String],
    ) -> Result<Vec<u8>, LaneError>;
}

impl RepoTestExt for LaneRepo {
    fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.read_path(path, lane, Some(base))?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
    }

    fn write(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        range: Range<u64>,
        replacement: Vec<u8>,
    ) -> Result<(), LaneError> {
        let mut current = self.read(path, lane, base)?;
        let start = usize::try_from(range.start).expect("test range start fits usize");
        let end = usize::try_from(range.end).expect("test range end fits usize");
        current.splice(start..end, replacement);
        self.replace_path(path, lane, Some(base), Some(current))
    }

    fn replace(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        content: Vec<u8>,
    ) -> Result<(), LaneError> {
        self.replace_path(path, lane, Some(base), Some(content))
    }

    fn promote(&mut self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        self.promote_path(path, lane, Some(base))?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
    }

    fn promote_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        op_ids: &[String],
    ) -> Result<Vec<u8>, LaneError> {
        self.promote_ops_path(path, lane, Some(base), op_ids)?
            .ok_or_else(|| LaneError::BaseMissing {
                path: path.to_owned(),
            })
    }
}
