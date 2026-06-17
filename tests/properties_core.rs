use std::collections::BTreeSet;

use lane::{
    BaseStorageSnapshot, DecodeError, FileOpStorageSnapshot, FilePath, LaneEntryStorageSnapshot,
    LaneError, LaneFileStorageSnapshot, LaneId, LaneRepo, LaneRepoStorageSnapshot, LaneRunState,
};
use proptest::prelude::*;
use proptest::test_runner::{Config as ProptestConfig, FileFailurePersistence};

const EXEC_OUTPUT_PREVIEW_LIMIT: usize = 4096;
const PATH: &str = "src/property.bin";
const AGENT_A: &str = "agent-a";
const AGENT_B: &str = "agent-b";
const AGENT_C: &str = "agent-c";

proptest! {
    #![proptest_config(ProptestConfig::with_failure_persistence(
        FileFailurePersistence::Direct("tests/properties_core.proptest-regressions"),
    ))]

    #[test]
    fn replace_then_read_returns_replacement(
        base in prop::collection::vec(any::<u8>(), 0..64),
        replacement in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut repo = repo_with_lanes();

        repo.replace_path(PATH, AGENT_A, Some(&base), Some(replacement.clone())).unwrap();

        prop_assert_eq!(
            repo.read_path(PATH, AGENT_A, Some(&base)).unwrap(),
            Some(replacement)
        );
    }

    #[test]
    fn create_then_read_returns_created_bytes(
        created in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut repo = repo_with_lanes();

        repo.replace_path(PATH, AGENT_A, None, Some(created.clone())).unwrap();

        prop_assert_eq!(
            repo.read_path(PATH, AGENT_A, None).unwrap(),
            Some(created)
        );
    }

    #[test]
    fn delete_then_read_returns_missing(
        base in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut repo = repo_with_lanes();

        repo.delete_path(PATH, AGENT_A, Some(&base)).unwrap();

        prop_assert_eq!(repo.read_path(PATH, AGENT_A, Some(&base)).unwrap(), None);
    }

    #[test]
    fn snapshot_roundtrip_preserves_read_behavior(
        base in prop::collection::vec(any::<u8>(), 0..64),
        replacement in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut repo = repo_with_lanes();
        repo.replace_path(PATH, AGENT_A, Some(&base), Some(replacement.clone())).unwrap();

        let decoded = LaneRepo::from_storage_snapshot(repo.storage_snapshot()).unwrap();

        prop_assert_eq!(
            decoded.read_path(PATH, AGENT_A, Some(&base)).unwrap(),
            Some(replacement)
        );
    }

    #[test]
    fn stale_base_rejects_existing_overlay(
        base in prop::collection::vec(any::<u8>(), 0..64),
        replacement_seed in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let replacement = bytes_different_from(&base, replacement_seed);
        let stale_base = stale_base_for(&base);
        let mut repo = repo_with_lanes();
        repo.replace_path(PATH, AGENT_A, Some(&base), Some(replacement)).unwrap();

        prop_assert_eq!(
            repo.read_path(PATH, AGENT_A, Some(&stale_base)),
            Err(LaneError::BaseChanged { path: PATH.to_owned() })
        );
    }

    #[test]
    fn accepting_non_overlapping_ops_preserves_other_lane_intent(
        left_value in prop::collection::vec(b'a'..=b'z', 0..8),
        right_value in prop::collection::vec(b'a'..=b'z', 0..8),
    ) {
        let base = b"left=0\nright=0\n";
        let mut left_content = base.to_vec();
        left_content.splice(5..6, left_value.clone());
        let mut right_content = base.to_vec();
        right_content.splice(13..14, right_value.clone());
        let mut expected = left_content.clone();
        let adjusted_right_start = 13 + left_value.len() - 1;
        expected.splice(adjusted_right_start..adjusted_right_start + 1, right_value);
        let mut repo = repo_with_lanes();
        repo.replace_path(PATH, AGENT_A, Some(base), Some(left_content)).unwrap();
        repo.replace_path(PATH, AGENT_B, Some(base), Some(right_content)).unwrap();

        let accepted = accept_all_ops(&mut repo, PATH, AGENT_A, base);

        prop_assert_eq!(
            repo.read_path(PATH, AGENT_B, Some(&accepted)).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn same_position_inserts_converge_across_acceptance_orders(
        agent_a_insert in prop::collection::vec(b'A'..=b'Z', 1..8),
        agent_b_insert in prop::collection::vec(b'A'..=b'Z', 1..8),
        agent_c_insert in prop::collection::vec(b'A'..=b'Z', 1..8),
    ) {
        let base = b"tail\n";
        let inserts = [
            (AGENT_A, agent_a_insert),
            (AGENT_B, agent_b_insert),
            (AGENT_C, agent_c_insert),
        ];
        let mut final_versions = Vec::new();
        for acceptance_order in ACCEPTANCE_ORDERS {
            let mut repo = repo_with_named_lanes(&[AGENT_A, AGENT_B, AGENT_C]);
            for (lane, insert) in &inserts {
                let mut lane_bytes = insert.clone();
                lane_bytes.extend_from_slice(base);
                repo.replace_path(PATH, lane, Some(base), Some(lane_bytes)).unwrap();
            }

            let mut current_base = base.to_vec();
            for lane_index in acceptance_order {
                current_base =
                    accept_all_ops(&mut repo, PATH, inserts[lane_index].0, &current_base);
            }
            final_versions.push(current_base);
        }

        let first = final_versions[0].clone();
        prop_assert!(first.ends_with(base));
        prop_assert!(final_versions.iter().all(|version| version == &first));
    }
}

#[test]
fn from_storage_snapshot_rejects_invalid_order_key() {
    let error = LaneRepo::from_storage_snapshot(snapshot_with_ops(vec![FileOpStorageSnapshot {
        id: 1,
        base_start: 0,
        base_len: 0,
        order_key: "$".to_owned(),
        inserted: b"created".to_vec(),
    }]))
    .unwrap_err();

    assert_eq!(error, DecodeError::InvalidOrderKey);
}

#[test]
fn from_storage_snapshot_rejects_overlapping_ops() {
    let error = LaneRepo::from_storage_snapshot(snapshot_with_ops(vec![
        FileOpStorageSnapshot {
            id: 1,
            base_start: 0,
            base_len: 2,
            order_key: "U".to_owned(),
            inserted: b"a".to_vec(),
        },
        FileOpStorageSnapshot {
            id: 2,
            base_start: 1,
            base_len: 1,
            order_key: "j".to_owned(),
            inserted: b"b".to_vec(),
        },
    ]))
    .unwrap_err();

    assert_eq!(error, DecodeError::OperationConflict);
}

#[test]
fn from_storage_snapshot_rejects_overflowing_op_ranges() {
    let error = LaneRepo::from_storage_snapshot(snapshot_with_ops(vec![FileOpStorageSnapshot {
        id: 1,
        base_start: u64::MAX,
        base_len: 1,
        order_key: "U".to_owned(),
        inserted: b"a".to_vec(),
    }]))
    .unwrap_err();

    assert_eq!(error, DecodeError::OperationOutOfBounds);
}

#[test]
fn from_storage_snapshot_rejects_overlay_for_missing_lane() {
    let mut snapshot = snapshot_with_ops(vec![FileOpStorageSnapshot {
        id: 1,
        base_start: 0,
        base_len: 0,
        order_key: "U".to_owned(),
        inserted: b"created".to_vec(),
    }]);
    snapshot.lanes.clear();

    let error = LaneRepo::from_storage_snapshot(snapshot).unwrap_err();

    assert_eq!(error, DecodeError::OverlayLaneMissing(lane_id(AGENT_A)));
}

#[test]
fn lane_id_parse_rejects_reserved_lane_names() {
    let error = LaneId::parse("base").unwrap_err();

    assert_eq!(error, LaneError::ReservedLane("base".to_owned()));
}

#[test]
fn lane_id_parse_rejects_empty_lane_names() {
    let error = LaneId::parse("  ").unwrap_err();

    assert_eq!(error, LaneError::InvalidLane("  ".to_owned()));
}

#[test]
fn lane_id_parse_rejects_leading_or_trailing_whitespace() {
    for lane in [" agent-a", "agent-a ", "\tagent-a"] {
        let error = LaneId::parse(lane).unwrap_err();
        assert_eq!(error, LaneError::InvalidLane(lane.to_owned()));
    }
}

#[test]
fn lane_id_serializes_as_plain_json_string() {
    let encoded = serde_json::to_value(lane_id(AGENT_A)).unwrap();

    assert_eq!(encoded, serde_json::json!(AGENT_A));
}

#[test]
fn file_path_parse_normalizes_repo_labels() {
    let path = FilePath::parse(r"src\.\property.bin").unwrap();

    assert_eq!(path.as_str(), PATH);
}

#[test]
fn file_path_parse_rejects_reserved_root_metadata() {
    for path in [
        ".lane/repo.json",
        ".LANE/repo.json",
        ".git/config",
        r".GIT\config",
    ] {
        assert!(
            matches!(FilePath::parse(path), Err(LaneError::InvalidPath(_))),
            "{path} should be rejected"
        );
    }
}

#[test]
fn file_path_parse_rejects_paths_outside_repo() {
    for path in [
        "",
        ".",
        "/tmp/file.txt",
        r"\tmp\file.txt",
        "src/../file.txt",
    ] {
        assert!(
            matches!(FilePath::parse(path), Err(LaneError::InvalidPath(_))),
            "{path} should be rejected"
        );
    }
}

#[test]
fn core_file_apis_reject_reserved_paths() {
    let mut repo = repo_with_lanes();
    let error = repo
        .replace_path(".LANE/repo.json", AGENT_A, None, Some(b"bad".to_vec()))
        .unwrap_err();

    assert!(matches!(error, LaneError::InvalidPath(_)));
}

#[test]
fn file_path_serializes_as_plain_json_string() {
    let encoded = serde_json::to_value(file_path(PATH)).unwrap();

    assert_eq!(encoded, serde_json::json!(PATH));
}

#[test]
fn lane_text_preview_keeps_boundary_sized_output_untruncated() {
    let output = "a".repeat(EXEC_OUTPUT_PREVIEW_LIMIT);
    let state = LaneRunState::new(Some(0), None, &output, "", Vec::new());

    assert_eq!(state.stdout.text, output);
    assert!(!state.stdout.truncated);
}

#[test]
fn lane_text_preview_truncates_stdout_and_stderr_over_limit() {
    let output = "a".repeat(EXEC_OUTPUT_PREVIEW_LIMIT + 1);
    let state = LaneRunState::new(Some(0), None, &output, &output, Vec::new());

    assert_eq!(state.stdout.text.len(), EXEC_OUTPUT_PREVIEW_LIMIT);
    assert_eq!(state.stderr.text.len(), EXEC_OUTPUT_PREVIEW_LIMIT);
    assert!(state.stdout.truncated);
    assert!(state.stderr.truncated);
}

#[test]
fn lane_text_preview_truncates_at_utf8_boundary() {
    let prefix = "€".repeat(1365);
    let output = format!("{prefix}é");
    let state = LaneRunState::new(Some(0), None, &output, "", Vec::new());

    assert_eq!(state.stdout.text, prefix);
    assert!(state.stdout.truncated);
}

fn repo_with_lanes() -> LaneRepo {
    repo_with_named_lanes(&[AGENT_A, AGENT_B])
}

fn repo_with_named_lanes(lanes: &[&str]) -> LaneRepo {
    let mut repo = LaneRepo::new();
    for lane in lanes {
        repo.create_lane(*lane).unwrap();
    }
    repo
}

const ACCEPTANCE_ORDERS: [[usize; 3]; 6] = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
];

fn accept_all_ops(repo: &mut LaneRepo, path: &str, lane: &str, base: &[u8]) -> Vec<u8> {
    let op_ids = repo
        .change_ops(path, lane, Some(base))
        .unwrap()
        .into_iter()
        .map(|op| op.op_id)
        .collect::<Vec<_>>();
    if op_ids.is_empty() {
        return base.to_vec();
    }
    repo.accept_ops_path(path, lane, Some(base), &op_ids)
        .unwrap()
        .unwrap()
}

fn bytes_different_from(base: &[u8], mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes == base {
        bytes.push(0);
    }
    bytes
}

fn stale_base_for(base: &[u8]) -> Vec<u8> {
    let mut stale = base.to_vec();
    if let Some(first) = stale.first_mut() {
        *first = first.wrapping_add(1);
    } else {
        stale.push(0);
    }
    stale
}

fn snapshot_with_ops(ops: Vec<FileOpStorageSnapshot>) -> LaneRepoStorageSnapshot {
    LaneRepoStorageSnapshot {
        lanes: [lane_id(AGENT_A)].into_iter().collect::<BTreeSet<_>>(),
        files: [(
            file_path(PATH),
            LaneFileStorageSnapshot {
                base: BaseStorageSnapshot::Present([0; 32]),
                lanes: [(lane_id(AGENT_A), LaneEntryStorageSnapshot::Present(ops))]
                    .into_iter()
                    .collect(),
            },
        )]
        .into_iter()
        .collect(),
    }
}

fn lane_id(lane: &str) -> LaneId {
    LaneId::parse(lane).unwrap()
}

fn file_path(path: &str) -> FilePath {
    FilePath::parse(path).unwrap()
}
