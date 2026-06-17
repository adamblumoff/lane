use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use lane::{LaneError, LaneRepo};
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use proptest::test_runner::{Config as ProptestConfig, FileFailurePersistence};

const PATH: &str = "src/model.txt";
const LANES: [&str; 3] = ["agent-a", "agent-b", "agent-c"];

#[derive(Clone, Debug)]
enum Command {
    CreateLane(usize),
    ReplacePath(usize, Vec<u8>),
    DeletePath(usize),
    AcceptAllOps(usize),
    DiscardLane(usize),
    StorageRoundtrip,
}

#[derive(Clone, Debug)]
struct ModelRepo {
    base: Option<Vec<u8>>,
    live_lanes: BTreeSet<usize>,
    overlays: BTreeMap<usize, Option<Vec<u8>>>,
}

impl ModelRepo {
    fn new(base: Option<Vec<u8>>) -> Self {
        Self {
            base,
            live_lanes: BTreeSet::new(),
            overlays: BTreeMap::new(),
        }
    }

    fn is_live(&self, lane: usize) -> bool {
        self.live_lanes.contains(&lane)
    }

    fn has_overlay(&self, lane: usize) -> bool {
        self.overlays.contains_key(&lane)
    }

    fn visible_content(&self, lane: usize) -> Option<Vec<u8>> {
        self.overlays
            .get(&lane)
            .cloned()
            .unwrap_or_else(|| self.base.clone())
    }

    fn set_overlay(&mut self, lane: usize, content: Option<Vec<u8>>) {
        if content == self.base {
            self.overlays.remove(&lane);
        } else {
            self.overlays.insert(lane, content);
        }
    }

    fn accept_lane_content(&mut self, lane: usize) {
        let accepted_content = self.visible_content(lane);
        let previous_overlays = self
            .overlays
            .iter()
            .map(|(lane, content)| (*lane, content.clone()))
            .collect::<BTreeMap<_, _>>();
        self.base = accepted_content;
        self.overlays.clear();

        for (retained_lane, content) in previous_overlays {
            if retained_lane != lane && content != self.base {
                self.overlays.insert(retained_lane, content);
            }
        }
    }
}

struct Harness {
    repo: LaneRepo,
    model: ModelRepo,
}

impl Harness {
    fn new(base: Option<Vec<u8>>) -> Self {
        let mut repo = LaneRepo::new();
        let mut model = ModelRepo::new(base);
        for lane in 0..2 {
            assert_eq!(repo.create_lane(lane_name(lane)), Ok(true));
            model.live_lanes.insert(lane);
        }

        Self { repo, model }
    }

    fn apply(&mut self, command: &Command) {
        match command {
            Command::CreateLane(lane) => self.create_lane(*lane),
            Command::ReplacePath(lane, content) => self.replace_path(*lane, content.clone()),
            Command::DeletePath(lane) => self.replace_path(*lane, None),
            Command::AcceptAllOps(lane) => self.accept_all_ops(*lane),
            Command::DiscardLane(lane) => self.discard_lane(*lane),
            Command::StorageRoundtrip => self.storage_roundtrip(),
        }
    }

    fn create_lane(&mut self, lane: usize) {
        let expected = self.model.live_lanes.insert(lane);

        assert_eq!(self.repo.create_lane(lane_name(lane)), Ok(expected));
    }

    fn replace_path(&mut self, lane: usize, content: impl Into<Option<Vec<u8>>>) {
        let content = content.into();
        let before = self.repo.clone();
        let result = self.repo.replace_path(
            PATH,
            lane_name(lane),
            self.model.base.as_deref(),
            content.clone(),
        );

        if self.model.is_live(lane) {
            assert_eq!(result, Ok(()));
            self.model.set_overlay(lane, content);
        } else {
            assert_eq!(
                result,
                Err(LaneError::LaneMissing(lane_name(lane).to_owned()))
            );
            assert_eq!(self.repo, before);
        }
    }

    fn accept_all_ops(&mut self, lane: usize) {
        let before = self.repo.clone();
        let change_ops = self
            .repo
            .change_ops(PATH, lane_name(lane), self.model.base.as_deref());

        if !self.model.is_live(lane) {
            assert_eq!(
                change_ops,
                Err(LaneError::LaneMissing(lane_name(lane).to_owned()))
            );
            assert_eq!(self.repo, before);
            return;
        }

        let change_ops = change_ops.unwrap();
        if !self.model.has_overlay(lane) {
            assert!(change_ops.is_empty());
            assert_eq!(self.repo, before);
            return;
        }

        let op_ids = change_ops
            .into_iter()
            .map(|op| op.op_id)
            .collect::<Vec<_>>();
        assert!(
            !op_ids.is_empty(),
            "model expected an acceptable overlay for {PATH} in {}",
            lane_name(lane)
        );

        let expected_base = self.model.visible_content(lane);
        let accepted = self
            .repo
            .accept_ops_path(PATH, lane_name(lane), self.model.base.as_deref(), &op_ids)
            .unwrap();

        assert_eq!(accepted, expected_base);
        self.model.accept_lane_content(lane);
    }

    fn discard_lane(&mut self, lane: usize) {
        let expected = self.model.live_lanes.remove(&lane);
        self.model.overlays.remove(&lane);

        assert_eq!(self.repo.discard_lane(lane_name(lane)), expected);
    }

    fn storage_roundtrip(&mut self) {
        self.repo = LaneRepo::from_storage_snapshot(self.repo.storage_snapshot()).unwrap();
    }

    fn assert_matches_model(&self) {
        let base_read = self
            .repo
            .read_path(PATH, "base", self.model.base.as_deref())
            .unwrap();
        assert_eq!(base_read, self.model.base);

        let decoded = LaneRepo::from_storage_snapshot(self.repo.storage_snapshot()).unwrap();
        assert_eq!(decoded, self.repo);

        for lane in 0..LANES.len() {
            self.assert_lane_matches_model(lane, &decoded);
        }

        self.assert_stale_base_rejected_when_overlays_exist();
    }

    fn assert_lane_matches_model(&self, lane: usize, decoded: &LaneRepo) {
        let actual = self
            .repo
            .read_path(PATH, lane_name(lane), self.model.base.as_deref());
        let decoded_actual = decoded.read_path(PATH, lane_name(lane), self.model.base.as_deref());
        let overlay_paths = self.repo.overlay_paths(lane_name(lane));
        let change_ops = self
            .repo
            .change_ops(PATH, lane_name(lane), self.model.base.as_deref());

        if !self.model.is_live(lane) {
            let expected = Err(LaneError::LaneMissing(lane_name(lane).to_owned()));
            assert_eq!(actual, expected);
            assert_eq!(
                decoded_actual,
                Err(LaneError::LaneMissing(lane_name(lane).to_owned()))
            );
            assert_eq!(
                overlay_paths,
                Err(LaneError::LaneMissing(lane_name(lane).to_owned()))
            );
            assert_eq!(
                change_ops,
                Err(LaneError::LaneMissing(lane_name(lane).to_owned()))
            );
            return;
        }

        assert_eq!(actual.unwrap(), self.model.visible_content(lane));
        assert_eq!(decoded_actual.unwrap(), self.model.visible_content(lane));

        let paths = overlay_paths
            .unwrap()
            .into_iter()
            .map(|path| path.as_str().to_owned())
            .collect::<Vec<_>>();
        let expected_paths = if self.model.has_overlay(lane) {
            vec![PATH.to_owned()]
        } else {
            Vec::new()
        };
        assert_eq!(paths, expected_paths);

        if self.model.has_overlay(lane) {
            assert!(
                !change_ops.unwrap().is_empty(),
                "model expected operation summaries for {PATH} in {}",
                lane_name(lane)
            );
        } else {
            assert!(change_ops.unwrap().is_empty());
        }
    }

    fn assert_stale_base_rejected_when_overlays_exist(&self) {
        if self.model.overlays.is_empty() {
            return;
        }

        let stale_base = stale_base_for(&self.model.base);
        for lane in &self.model.live_lanes {
            assert_eq!(
                self.repo
                    .read_path(PATH, lane_name(*lane), stale_base.as_deref()),
                Err(LaneError::BaseChanged {
                    path: PATH.to_owned()
                })
            );
        }
    }
}

mod stateful_whole_file_model {
    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_failure_persistence(
            FileFailurePersistence::Direct("tests/model_repo_engine.proptest-regressions"),
        ))]

        #[test]
        fn generated_repo_histories_match_reference_model(
            initial_base in maybe_base_strategy(),
            commands in command_sequence_strategy(),
        ) {
            let mut harness = Harness::new(initial_base);
            harness.assert_matches_model();

            for command in commands {
                harness.apply(&command);
                harness.assert_matches_model();
            }
        }
    }
}

mod metamorphic_laws {
    use super::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_failure_persistence(
            FileFailurePersistence::Direct("tests/model_repo_engine.proptest-regressions"),
        ))]

        #[test]
        fn accepting_independent_lane_ops_converges_across_acceptance_order(
            left_value in text_chunk_strategy(0..8),
            right_value in text_chunk_strategy(0..8),
        ) {
            let base = b"left=0\nright=0\n";
            let left_content = content_with_replacement(base, 5..6, &left_value);
            let right_content = content_with_replacement(base, 13..14, &right_value);

            let mut left_first = repo_with_two_replacements(base, &left_content, &right_content);
            let mut left_first_base = accept_all_ops(&mut left_first, "agent-a", base);
            left_first_base = accept_all_ops(&mut left_first, "agent-b", &left_first_base);

            let mut right_first = repo_with_two_replacements(base, &left_content, &right_content);
            let mut right_first_base = accept_all_ops(&mut right_first, "agent-b", base);
            right_first_base = accept_all_ops(&mut right_first, "agent-a", &right_first_base);

            prop_assert_eq!(left_first_base, right_first_base);
        }

        #[test]
        fn discarding_one_lane_preserves_base_and_other_lane_projection(
            base in prop::collection::vec(any::<u8>(), 0..24),
            discarded_content in prop::collection::vec(any::<u8>(), 0..24),
            retained_content in prop::collection::vec(any::<u8>(), 0..24),
        ) {
            let mut repo = repo_with_lanes();
            repo.replace_path(PATH, "agent-a", Some(&base), Some(discarded_content)).unwrap();
            repo.replace_path(PATH, "agent-b", Some(&base), Some(retained_content.clone())).unwrap();

            prop_assert!(repo.discard_lane("agent-a"));

            prop_assert_eq!(
                repo.read_path(PATH, "base", Some(&base)).unwrap(),
                Some(base.clone())
            );
            prop_assert_eq!(
                repo.read_path(PATH, "agent-b", Some(&base)).unwrap(),
                Some(retained_content)
            );
            prop_assert_eq!(
                repo.read_path(PATH, "agent-a", Some(&base)),
                Err(LaneError::LaneMissing("agent-a".to_owned()))
            );
        }
    }
}

fn command_sequence_strategy() -> BoxedStrategy<Vec<Command>> {
    prop::collection::vec(command_strategy(), 1..48).boxed()
}

fn command_strategy() -> BoxedStrategy<Command> {
    let lane = 0usize..LANES.len();
    let content = content_strategy();

    prop_oneof![
        2 => lane.clone().prop_map(Command::CreateLane),
        6 => (lane.clone(), content).prop_map(|(lane, content)| Command::ReplacePath(lane, content)),
        2 => lane.clone().prop_map(Command::DeletePath),
        3 => lane.clone().prop_map(Command::AcceptAllOps),
        2 => lane.prop_map(Command::DiscardLane),
        2 => Just(Command::StorageRoundtrip),
    ]
    .boxed()
}

fn maybe_base_strategy() -> BoxedStrategy<Option<Vec<u8>>> {
    prop_oneof![
        1 => Just(None),
        4 => content_strategy().prop_map(Some),
    ]
    .boxed()
}

fn content_strategy() -> BoxedStrategy<Vec<u8>> {
    // Force coarse whole-file replacement ops so the state model can stay small;
    // text rebase laws are covered by targeted metamorphic properties.
    prop::collection::vec(any::<u8>(), 0..23)
        .prop_map(|tail| {
            let mut content = vec![0];
            content.extend(tail);
            content
        })
        .boxed()
}

fn text_chunk_strategy(size: Range<usize>) -> BoxedStrategy<Vec<u8>> {
    prop::collection::vec(b'a'..=b'z', size).boxed()
}

fn repo_with_lanes() -> LaneRepo {
    let mut repo = LaneRepo::new();
    repo.create_lane("agent-a").unwrap();
    repo.create_lane("agent-b").unwrap();
    repo
}

fn repo_with_two_replacements(base: &[u8], left_content: &[u8], right_content: &[u8]) -> LaneRepo {
    let mut repo = repo_with_lanes();
    repo.replace_path(PATH, "agent-a", Some(base), Some(left_content.to_vec()))
        .unwrap();
    repo.replace_path(PATH, "agent-b", Some(base), Some(right_content.to_vec()))
        .unwrap();
    repo
}

fn accept_all_ops(repo: &mut LaneRepo, lane: &str, base: &[u8]) -> Vec<u8> {
    let op_ids = repo
        .change_ops(PATH, lane, Some(base))
        .unwrap()
        .into_iter()
        .map(|op| op.op_id)
        .collect::<Vec<_>>();
    if op_ids.is_empty() {
        return base.to_vec();
    }
    repo.accept_ops_path(PATH, lane, Some(base), &op_ids)
        .unwrap()
        .unwrap()
}

fn content_with_replacement(base: &[u8], range: Range<usize>, replacement: &[u8]) -> Vec<u8> {
    let mut content = base.to_vec();
    content.splice(range, replacement.iter().copied());
    content
}

fn stale_base_for(base: &Option<Vec<u8>>) -> Option<Vec<u8>> {
    match base {
        Some(base) => {
            let mut stale = base.clone();
            if let Some(first) = stale.first_mut() {
                *first = first.wrapping_add(1);
            } else {
                stale.push(0);
            }
            Some(stale)
        }
        None => Some(vec![0]),
    }
}

fn lane_name(lane: usize) -> &'static str {
    LANES[lane]
}
