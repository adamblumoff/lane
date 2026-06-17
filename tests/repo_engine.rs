use lane::{BaseStorageSnapshot, LaneError, LaneId, LaneOpKind, LaneRepo};
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
fn selected_ops_accept_every_changed_path_for_lane() {
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

    let accepted_path = repo.accept_all_ops(PATH, "agent-a", BASE).unwrap();
    let accepted_settings = repo
        .accept_all_ops(SETTINGS_PATH, "agent-a", SETTINGS_BASE)
        .unwrap();

    assert_eq!(accepted_path, b"export const mode = 'fast';\n");
    assert_eq!(accepted_settings, b"{\"mode\":\"fast\"}\n");
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
}

#[test]
fn accept_returns_new_base_and_preserves_other_lane_projections() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();
    repo.write(PATH, "agent-b", BASE, 21..25, b"safe".to_vec())
        .unwrap();

    let accepted = repo.accept_all_ops(PATH, "agent-a", BASE).unwrap();

    assert_eq!(accepted, b"export const mode = 'fast';\n");
    assert_eq!(repo.read(PATH, "base", &accepted).unwrap(), accepted);
    assert_eq!(repo.read(PATH, "agent-a", &accepted).unwrap(), accepted);
    assert_eq!(
        repo.read(PATH, "agent-b", &accepted).unwrap(),
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
fn untouched_lanes_follow_accepted_base() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    let accepted = repo.accept_all_ops(PATH, "agent-a", BASE).unwrap();

    assert_eq!(accepted, b"export const mode = 'fast';\n");
    assert_eq!(repo.read(PATH, "agent-b", &accepted).unwrap(), accepted);
}

#[test]
fn non_overlapping_accepted_lanes_follow_later_base_changes() {
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

    let badabing = repo.accept_all_ops(PATH, "badabing", BASE).unwrap();
    assert_eq!(badabing, b"export const mode = 'base';\nbadabing\n");

    let accepted = repo.accept_all_ops(PATH, "agent-a", &badabing).unwrap();

    assert_eq!(accepted, b"export const mode = 'fast';\nbadabing\n");
    assert_eq!(repo.read(PATH, "agent-a", &accepted).unwrap(), accepted);
    assert_eq!(repo.read(PATH, "badabing", &accepted).unwrap(), accepted);
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

    let decoded = round_trip_repo(&repo);

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
fn empty_created_path_has_acceptable_create_op_after_storage_roundtrip() {
    let mut repo = seeded_repo();
    let path = "src/empty-created.txt";
    repo.replace_path(path, "agent-a", None, Some(Vec::new()))
        .unwrap();

    let decoded = round_trip_repo(&repo);
    let ops = decoded.change_ops(path, "agent-a", None).unwrap();

    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].kind, LaneOpKind::Create);
    assert_eq!(ops[0].inserted_len, 0);

    let mut decoded = decoded;
    let accepted = decoded
        .accept_ops_path(path, "agent-a", None, &[ops[0].op_id.clone()])
        .unwrap();
    assert_eq!(accepted, Some(Vec::new()));
    assert_eq!(
        decoded.overlay_paths("agent-a").unwrap(),
        Vec::<&str>::new()
    );
}

#[test]
fn repo_state_snapshot_uses_sha256_base_fingerprint() {
    let mut repo = seeded_repo();
    repo.write(PATH, "agent-a", BASE, 21..25, b"fast".to_vec())
        .unwrap();

    let mut expected = [0; 32];
    expected.copy_from_slice(&Sha256::digest(BASE));
    let snapshot = repo.storage_snapshot();

    assert_eq!(
        snapshot.files.get(PATH).unwrap().base,
        BaseStorageSnapshot::Present(expected)
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
fn many_independent_ops_keep_stable_increasing_order_keys() {
    let mut repo = seeded_repo();
    let base = (0..128)
        .map(|index| format!("line-{index:03}\n"))
        .collect::<String>();
    let edited = (0..128)
        .map(|index| format!("line-{index:03}\ninsert-{index:03}\n"))
        .collect::<String>();

    repo.replace(
        "src/order.txt",
        "agent-a",
        base.as_bytes(),
        edited.as_bytes().to_vec(),
    )
    .unwrap();
    let ops = repo
        .change_ops("src/order.txt", "agent-a", Some(base.as_bytes()))
        .unwrap();

    assert_eq!(ops.len(), 128);
    assert!(ops.windows(2).all(|window| {
        window[0].base_start < window[1].base_start && window[0].order_key < window[1].order_key
    }));
    assert_eq!(
        repo.read("src/order.txt", "agent-a", base.as_bytes())
            .unwrap(),
        edited.as_bytes()
    );
}

#[test]
fn non_overlapping_same_file_ops_compose_after_accept() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\n";
    repo.write("src/math.txt", "agent-a", base, 6..7, b"10".to_vec())
        .unwrap();
    repo.write("src/math.txt", "agent-b", base, 13..14, b"20".to_vec())
        .unwrap();

    let accepted = repo
        .accept_all_ops("src/math.txt", "agent-a", base)
        .unwrap();

    assert_eq!(accepted, b"alpha=10\nbeta=2\n");
    assert_eq!(
        repo.read("src/math.txt", "agent-b", &accepted).unwrap(),
        b"alpha=10\nbeta=20\n"
    );
    assert_eq!(
        repo.change_ops("src/math.txt", "agent-b", Some(&accepted))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn selected_ops_accept_without_accepting_the_whole_lane_file() {
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
    let accepted = repo
        .accept_ops("src/math.txt", "agent-a", base, &[ops[0].op_id.clone()])
        .unwrap();

    assert_eq!(accepted, b"alpha=10\nbeta=2\ngamma=3\n");
    assert_eq!(
        repo.read("src/math.txt", "agent-a", &accepted).unwrap(),
        edited
    );
    assert_eq!(
        repo.read("src/math.txt", "agent-b", &accepted).unwrap(),
        b"alpha=10\nbeta=20\ngamma=3\n"
    );
    assert_eq!(
        repo.change_ops("src/math.txt", "agent-a", Some(&accepted))
            .unwrap()
            .len(),
        1
    );
    let remaining_a_ops = repo
        .change_ops("src/math.txt", "agent-a", Some(&accepted))
        .unwrap();
    assert_eq!(remaining_a_ops[0].op_id, "agent-a:2");
    assert_eq!(remaining_a_ops[0].base_start, 23);
    assert_eq!(
        remaining_a_ops[0].order_key,
        "00000000000000000023:j:agent-a:00000000000000000002"
    );
    let remaining_b_ops = repo
        .change_ops("src/math.txt", "agent-b", Some(&accepted))
        .unwrap();
    assert_eq!(remaining_b_ops[0].op_id, "agent-b:1");
    assert_eq!(remaining_b_ops[0].base_start, 15);
}

#[test]
fn selected_delete_acceptance_preserves_other_lane_as_create() {
    let mut repo = seeded_repo();
    let base = b"mode=base\n";
    repo.delete_path("src/mode.txt", "agent-a", Some(base))
        .unwrap();
    repo.write("src/mode.txt", "agent-b", base, 5..9, b"safe".to_vec())
        .unwrap();

    let accepted = repo
        .accept_ops_path(
            "src/mode.txt",
            "agent-a",
            Some(base),
            &["agent-a:delete".to_owned()],
        )
        .unwrap();

    assert_eq!(accepted, None);
    assert_eq!(repo.read_path("src/mode.txt", "agent-a", None), Ok(None));
    assert_eq!(
        repo.read_path("src/mode.txt", "agent-b", None).unwrap(),
        Some(b"mode=safe\n".to_vec())
    );
    let agent_b_ops = repo.change_ops("src/mode.txt", "agent-b", None).unwrap();
    assert_eq!(agent_b_ops.len(), 1);
    assert_eq!(agent_b_ops[0].kind, LaneOpKind::Create);
    assert_eq!(agent_b_ops[0].conflicts_with, Vec::<LaneId>::new());
}

#[test]
fn missing_selected_op_does_not_mutate_repo() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\n";
    repo.write("src/math.txt", "agent-a", base, 6..7, b"10".to_vec())
        .unwrap();

    assert_eq!(
        repo.accept_ops("src/math.txt", "agent-a", base, &["agent-a:999".to_owned()],),
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
fn accept_replacement_op_accepts_replacement_bytes_and_preserves_other_lane_alternative() {
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
    let accepted_clean = repo
        .accept_ops("src/vars.txt", "agent-a", base, &clean_op_ids)
        .unwrap();
    assert_eq!(accepted_clean, b"a=A\nb=2\nc=C\n");

    let detail = repo
        .op_detail(
            "src/vars.txt",
            "agent-a",
            Some(&accepted_clean),
            "agent-a:2",
        )
        .unwrap();
    assert_eq!(detail.base, b"2");
    assert_eq!(detail.inserted, b"B");
    assert_eq!(detail.summary.conflicts_with, vec![lane_id("agent-b")]);

    let accepted = repo
        .accept_replacement_op_path(
            "src/vars.txt",
            "agent-a",
            Some(&accepted_clean),
            "agent-a:2",
            b"Y".to_vec(),
        )
        .unwrap()
        .unwrap();

    assert_eq!(accepted, b"a=A\nb=Y\nc=C\n");
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
    assert_eq!(
        repo.read("src/vars.txt", "agent-b", &accepted).unwrap(),
        b"a=A\nb=X\nc=C\n"
    );
}

#[test]
fn accept_replacement_ops_combines_conflicting_replacements_and_consumes_selected_lanes() {
    let mut repo = seeded_repo();
    repo.create_lane("agent-c").unwrap();
    let base = b"TODO";
    let path = "src/tasks.ts";
    repo.replace(path, "agent-a", base, b"function a() {}".to_vec())
        .unwrap();
    repo.replace(path, "agent-b", base, b"function b() {}".to_vec())
        .unwrap();
    repo.replace(path, "agent-c", base, b"function c() {}".to_vec())
        .unwrap();

    let selections = ["agent-a", "agent-b", "agent-c"]
        .into_iter()
        .map(|lane| {
            let op = repo
                .change_ops(path, lane, Some(base))
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            assert_eq!(op.conflicts_with.len(), 2);
            (lane_id(lane), op.op_id)
        })
        .collect::<Vec<_>>();
    let replacement = b"function a() {}\n\nfunction b() {}\n\nfunction c() {}".to_vec();

    let accepted = repo
        .accept_replacement_ops_path(path, Some(base), &selections, replacement.clone())
        .unwrap()
        .unwrap();

    assert_eq!(accepted, replacement);
    assert_eq!(repo.read(path, "base", &accepted).unwrap(), replacement);
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
    assert_eq!(repo.overlay_paths("agent-b").unwrap(), Vec::<&str>::new());
    assert_eq!(repo.overlay_paths("agent-c").unwrap(), Vec::<&str>::new());
}

#[test]
fn accept_replacement_ops_rejects_unrelated_clean_ops_without_mutating_repo() {
    let mut repo = seeded_repo();
    let base = b"alpha=1\nbeta=2\n";
    let path = "src/math.txt";
    repo.write(path, "agent-a", base, 6..7, b"10".to_vec())
        .unwrap();
    repo.write(path, "agent-b", base, 13..14, b"20".to_vec())
        .unwrap();

    let agent_a_op = repo
        .change_ops(path, "agent-a", Some(base))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let agent_b_op = repo
        .change_ops(path, "agent-b", Some(base))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(agent_a_op.conflicts_with.is_empty());
    assert!(agent_b_op.conflicts_with.is_empty());

    let result = repo.accept_replacement_ops_path(
        path,
        Some(base),
        &[
            (lane_id("agent-a"), agent_a_op.op_id),
            (lane_id("agent-b"), agent_b_op.op_id),
        ],
        b"bad".to_vec(),
    );

    assert!(matches!(
        result,
        Err(LaneError::InvalidOperationSelection { path: failed_path, reason })
            if failed_path == path && reason.contains("not one conflict-connected group")
    ));
    assert_eq!(
        repo.read(path, "agent-a", base).unwrap(),
        b"alpha=10\nbeta=2\n"
    );
    assert_eq!(
        repo.read(path, "agent-b", base).unwrap(),
        b"alpha=1\nbeta=20\n"
    );
    assert_eq!(repo.read(path, "base", base).unwrap(), base);
}

#[test]
fn accept_replacement_ops_preserves_unselected_empty_file_insert_as_alternative_to_delete_replacement()
 {
    let mut repo = seeded_repo();
    repo.create_lane("agent-c").unwrap();
    let base = b"";
    let path = "src/empty.txt";
    repo.delete_path(path, "agent-a", Some(base)).unwrap();
    repo.write(path, "agent-b", base, 0..0, b"B".to_vec())
        .unwrap();
    repo.write(path, "agent-c", base, 0..0, b"C".to_vec())
        .unwrap();

    let delete_op = repo
        .change_ops(path, "agent-a", Some(base))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let agent_b_op = repo
        .change_ops(path, "agent-b", Some(base))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(delete_op.conflicts_with.len(), 2);
    assert_eq!(agent_b_op.conflicts_with, vec![lane_id("agent-a")]);

    let accepted = repo
        .accept_replacement_ops_path(
            path,
            Some(base),
            &[
                (lane_id("agent-a"), delete_op.op_id),
                (lane_id("agent-b"), agent_b_op.op_id),
            ],
            b"merged".to_vec(),
        )
        .unwrap()
        .unwrap();

    assert_eq!(accepted, b"merged");
    assert_eq!(repo.overlay_paths("agent-a").unwrap(), Vec::<&str>::new());
    assert_eq!(repo.overlay_paths("agent-b").unwrap(), Vec::<&str>::new());
    assert_eq!(repo.read(path, "agent-c", &accepted).unwrap(), b"C");
}

#[test]
fn overlapping_same_file_ops_remain_alternatives_after_accept() {
    let mut repo = seeded_repo();
    let base = b"mode=base\n";
    repo.write("src/mode.txt", "agent-a", base, 5..9, b"fast".to_vec())
        .unwrap();
    repo.write("src/mode.txt", "agent-b", base, 5..9, b"safe".to_vec())
        .unwrap();

    let before = repo
        .change_ops("src/mode.txt", "agent-a", Some(base))
        .unwrap();
    assert_eq!(before[0].conflicts_with, vec![lane_id("agent-b")]);

    let accepted = repo
        .accept_all_ops("src/mode.txt", "agent-a", base)
        .unwrap();

    assert_eq!(accepted, b"mode=fast\n");
    assert_eq!(
        repo.read("src/mode.txt", "agent-b", &accepted).unwrap(),
        b"mode=safe\n"
    );
    assert!(
        !repo
            .change_ops("src/mode.txt", "agent-b", Some(&accepted))
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

    let accepted = repo
        .accept_all_ops("src/imports.txt", "agent-a", base)
        .unwrap();

    assert_eq!(accepted, b"use a;\ntail\n");
    assert_eq!(
        repo.read("src/imports.txt", "agent-b", &accepted).unwrap(),
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

    let accepted = repo
        .accept_all_ops("src/empty.txt", "agent-a", base)
        .unwrap();

    assert_eq!(accepted, b"a");
    assert_eq!(
        repo.read("src/empty.txt", "agent-b", &accepted).unwrap(),
        b"ab"
    );
}

#[test]
fn accepting_identical_insert_removes_other_lane_overlay_instead_of_duplicating() {
    let mut repo = seeded_repo();
    let base = b"tail\n";
    repo.write(
        "src/imports.txt",
        "agent-a",
        base,
        0..0,
        b"use same;\n".to_vec(),
    )
    .unwrap();
    repo.write(
        "src/imports.txt",
        "agent-b",
        base,
        0..0,
        b"use same;\n".to_vec(),
    )
    .unwrap();

    let accepted = repo
        .accept_all_ops("src/imports.txt", "agent-a", base)
        .unwrap();

    assert_eq!(accepted, b"use same;\ntail\n");
    assert_eq!(
        repo.read("src/imports.txt", "agent-b", &accepted).unwrap(),
        b"use same;\ntail\n"
    );
    assert!(
        repo.change_ops("src/imports.txt", "agent-b", Some(&accepted))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn accepting_identical_whole_file_replacement_removes_other_lane_overlay() {
    let mut repo = seeded_repo();
    let base = b"mode=old\n";
    let replacement = b"mode=new\n";
    repo.replace_path(
        "src/mode.txt",
        "agent-a",
        Some(base),
        Some(replacement.to_vec()),
    )
    .unwrap();
    repo.replace_path(
        "src/mode.txt",
        "agent-b",
        Some(base),
        Some(replacement.to_vec()),
    )
    .unwrap();

    let accepted = repo
        .accept_all_ops("src/mode.txt", "agent-a", base)
        .unwrap();

    assert_eq!(accepted, replacement);
    assert_eq!(
        repo.read("src/mode.txt", "agent-b", &accepted).unwrap(),
        replacement
    );
    assert!(
        repo.change_ops("src/mode.txt", "agent-b", Some(&accepted))
            .unwrap()
            .is_empty()
    );
}

fn seeded_repo() -> LaneRepo {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    repo.create_lane("agent-b").unwrap();
    repo
}

fn round_trip_repo(repo: &LaneRepo) -> LaneRepo {
    LaneRepo::from_storage_snapshot(repo.storage_snapshot()).unwrap()
}

fn lane_id(lane: &str) -> LaneId {
    LaneId::parse(lane).unwrap()
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
    fn accept_all_ops(&mut self, path: &str, lane: &str, base: &[u8])
    -> Result<Vec<u8>, LaneError>;
    fn accept_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        op_ids: &[String],
    ) -> Result<Vec<u8>, LaneError>;
}

impl RepoTestExt for LaneRepo {
    fn read(&self, path: &str, lane: &str, base: &[u8]) -> Result<Vec<u8>, LaneError> {
        Ok(self
            .read_path(path, lane, Some(base))?
            .expect("test expected file bytes"))
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

    fn accept_all_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
    ) -> Result<Vec<u8>, LaneError> {
        let op_ids = self
            .change_ops(path, lane, Some(base))?
            .into_iter()
            .map(|op| op.op_id)
            .collect::<Vec<_>>();
        self.accept_ops(path, lane, base, &op_ids)
    }

    fn accept_ops(
        &mut self,
        path: &str,
        lane: &str,
        base: &[u8],
        op_ids: &[String],
    ) -> Result<Vec<u8>, LaneError> {
        Ok(self
            .accept_ops_path(path, lane, Some(base), op_ids)?
            .expect("test expected accepted file bytes"))
    }
}
