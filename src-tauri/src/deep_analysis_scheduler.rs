//! Self-contained scheduler policy; no engine or database dependencies.
use rand::Rng;
use serde::{Deserialize, Serialize};

/// An unanalysed move occurrence; `index` is the backed-up score of its nearest scored parent.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub occurrence_id: i64,
    /// Canonical position shared by transposed occurrences; defaults to the occurrence ID.
    pub position_id: i64,
    pub index: u8,
    pub ply: u32,
    pub preferred: bool,
    pub siblings: u32,
}
impl Candidate {
    pub fn new(occurrence_id: i64, index: u8, ply: u32, preferred: bool, siblings: u32) -> Self {
        Self {
            occurrence_id,
            position_id: occurrence_id,
            index,
            ply,
            preferred,
            siblings,
        }
    }

    pub fn with_position(mut self, position_id: i64) -> Self {
        self.position_id = position_id;
        self
    }
}

/// Bounded, independently testable candidate ranking. Returns occurrence IDs, at most one
/// per eligible canonical position. Stage filters precede deduplication; no legal moves are
/// invented here.
pub fn select_candidates<R: Rng + ?Sized>(
    stage: Stage,
    root_index: u8,
    available: &[Candidate],
    settings: &AnalysisSettings,
    rng: &mut R,
) -> Vec<i64> {
    let limit = settings.max_positions as usize;
    if limit == 0 {
        return vec![];
    }
    let in_range = |c: &&Candidate, delta: u8| {
        c.index <= 99
            && c.index.abs_diff(root_index) <= delta
            && c.index >= root_index.saturating_sub(delta)
            && c.index <= root_index.saturating_add(delta).min(99)
    };
    let base = if stage == Stage::Base {
        let mut chosen = Vec::new();
        for extra in (0..=52u8).step_by(3).chain(std::iter::once(52)) {
            let delta = settings.max_delta.saturating_add(extra);
            chosen = available.iter().filter(|c| in_range(c, delta)).collect();
            if chosen
                .iter()
                .map(|c| c.position_id)
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= limit
            {
                break;
            }
        }
        chosen
    } else {
        available
            .iter()
            .filter(|c| in_range(c, settings.max_delta))
            .collect()
    };
    let mut choices: Vec<&Candidate> = base
        .into_iter()
        .filter(|c| match stage {
            Stage::Alternatives => !c.preferred && c.siblings > 1,
            Stage::Prolong => c.preferred && c.ply > 0,
            _ => true,
        })
        .collect();
    choices.sort_by_key(|c| (c.index.abs_diff(root_index), c.ply, c.occurrence_id));
    let mut seen = std::collections::HashSet::new();
    choices.retain(|c| seen.insert(c.position_id));
    if stage == Stage::Random {
        let mut output = Vec::new();
        while !choices.is_empty() && output.len() < limit {
            let weights: Vec<u32> = choices
                .iter()
                .map(|c| u32::from(6u8.saturating_sub(c.index.abs_diff(root_index))).max(1))
                .collect();
            let mut pick = rng.gen_range(0..weights.iter().sum::<u32>());
            let selected = weights
                .iter()
                .position(|&weight| {
                    if pick < weight {
                        true
                    } else {
                        pick -= weight;
                        false
                    }
                })
                .unwrap();
            output.push(choices.remove(selected).occurrence_id);
        }
        return output;
    }
    choices
        .into_iter()
        .take(limit)
        .map(|c| c.occurrence_id)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisOperation {
    TimeOrDepth,
    TimeAndDepth,
    Ungated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Waiting,
    Success,
    Timeout,
}

impl AnalysisOperation {
    pub fn decide(
        self,
        elapsed_ms: u64,
        depth: u32,
        requested_ms: u32,
        requested_depth: u32,
        valid: bool,
    ) -> Completion {
        let hard = u64::from(requested_ms).saturating_mul(10);
        if elapsed_ms >= hard {
            return if valid {
                Completion::Success
            } else {
                Completion::Timeout
            };
        }
        if !valid {
            return Completion::Waiting;
        }
        let minimum = elapsed_ms >= u64::from(requested_ms);
        let depth_ok = depth >= requested_depth;
        let done = match self {
            Self::TimeOrDepth => minimum || depth_ok,
            Self::TimeAndDepth => minimum && depth_ok,
            Self::Ungated => true,
        };
        if done {
            Completion::Success
        } else {
            Completion::Waiting
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Random,
    Alternatives,
    Base,
    Prolong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnalysisSettings {
    pub max_positions: u32,
    /// Maximum plies of automatic expansion from the project's root.
    pub max_ply: u32,
    pub max_delta: u8,
    pub analysis_time_ms: u32,
    pub analysis_depth: u32,
    pub analysis_operation: AnalysisOperation,
    pub engine_id: String,
    pub prolong_frequency: u32,
    pub prolong_depth: u8,
    pub manual_prolong_count: u8,
}
impl Default for AnalysisSettings {
    fn default() -> Self {
        Self {
            max_positions: 4,
            max_ply: 3,
            max_delta: 10,
            analysis_time_ms: 1000,
            analysis_depth: 12,
            analysis_operation: AnalysisOperation::TimeAndDepth,
            engine_id: String::new(),
            prolong_frequency: 2,
            prolong_depth: 3,
            manual_prolong_count: 3,
        }
    }
}
impl AnalysisSettings {
    pub fn stage(&self, cycle: u64) -> Stage {
        match cycle % 3 {
            1 => Stage::Random,
            2 => Stage::Alternatives,
            _ if (cycle / 3) % u64::from(self.prolong_frequency.max(1)) == 0 => Stage::Prolong,
            _ => Stage::Base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        select_candidates, AnalysisOperation, AnalysisSettings, Candidate, Completion, Stage,
    };
    use rand::SeedableRng;

    #[test]
    fn base_widens_three_at_a_time_but_stops_at_fifty_two_and_trims() {
        let settings = AnalysisSettings {
            max_delta: 0,
            max_positions: 1,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 54, 1, true, 1),
            Candidate::new(2, 99, 1, true, 1),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(2);
        assert_eq!(
            select_candidates(Stage::Base, 50, &candidates, &settings, &mut rng),
            vec![1]
        );
        let far = vec![Candidate::new(2, 99, 1, true, 1)];
        assert_eq!(
            select_candidates(Stage::Base, 50, &far, &settings, &mut rng),
            vec![2]
        );
        let beyond = vec![Candidate::new(3, 100, 1, true, 1)];
        assert!(select_candidates(Stage::Base, 50, &beyond, &settings, &mut rng).is_empty());
    }

    #[test]
    fn base_budget_counts_unique_positions_not_occurrences() {
        let settings = AnalysisSettings {
            max_delta: 4,
            max_positions: 2,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 50, 1, true, 2).with_position(100),
            Candidate::new(2, 51, 1, true, 2).with_position(100),
            Candidate::new(3, 52, 1, true, 2).with_position(200),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(9);
        assert_eq!(
            select_candidates(Stage::Base, 50, &candidates, &settings, &mut rng),
            vec![1, 3]
        );
        assert_eq!(candidates[0].occurrence_id, 1);
        assert_eq!(candidates[0].position_id, 100);
        assert_eq!(Candidate::new(7, 50, 0, false, 0).position_id, 7);
    }

    #[test]
    fn base_widens_to_fill_unique_position_budget() {
        let settings = AnalysisSettings {
            max_delta: 0,
            max_positions: 2,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 50, 1, true, 2).with_position(100),
            Candidate::new(2, 50, 2, true, 2).with_position(100),
            Candidate::new(3, 53, 1, true, 2).with_position(200),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        assert_eq!(
            select_candidates(Stage::Base, 50, &candidates, &settings, &mut rng),
            vec![1, 3]
        );
    }

    #[test]
    fn alternative_requires_meaningful_sibling_and_filters_before_random() {
        let settings = AnalysisSettings {
            max_delta: 4,
            max_positions: 1,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 49, 1, true, 2),
            Candidate::new(2, 52, 1, false, 2),
            Candidate::new(3, 80, 1, false, 2),
            Candidate::new(4, 50, 1, false, 1),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        assert_eq!(
            select_candidates(Stage::Alternatives, 50, &candidates, &settings, &mut rng),
            vec![2]
        );
        for _ in 0..32 {
            assert_ne!(
                select_candidates(Stage::Random, 50, &candidates, &settings, &mut rng),
                vec![3]
            );
        }
    }

    #[test]
    fn random_is_seed_reproducible_and_weighted() {
        let settings = AnalysisSettings {
            max_delta: 5,
            max_positions: 1,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 50, 1, true, 1),
            Candidate::new(2, 54, 1, true, 1),
        ];
        let draw = |seed| {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            (0..256)
                .map(|_| select_candidates(Stage::Random, 50, &candidates, &settings, &mut rng)[0])
                .collect::<Vec<_>>()
        };
        let a = draw(123);
        assert_eq!(a, draw(123));
        assert!(a.iter().filter(|&&id| id == 1).count() > a.iter().filter(|&&id| id == 2).count());
    }

    #[test]
    fn alternatives_filter_occurrences_before_deduplicating_positions() {
        let settings = AnalysisSettings {
            max_delta: 4,
            max_positions: 2,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 50, 1, true, 2).with_position(100),
            Candidate::new(2, 51, 1, false, 2).with_position(100),
            Candidate::new(3, 50, 1, false, 1).with_position(200),
            Candidate::new(4, 52, 1, false, 2).with_position(200),
        ];
        let mut rng = rand::rngs::StdRng::seed_from_u64(11);
        assert_eq!(
            select_candidates(Stage::Alternatives, 50, &candidates, &settings, &mut rng),
            vec![2, 4]
        );
    }

    #[test]
    fn random_samples_unique_positions_with_seeded_reproducibility() {
        let settings = AnalysisSettings {
            max_delta: 4,
            max_positions: 2,
            ..Default::default()
        };
        let candidates = vec![
            Candidate::new(1, 50, 1, true, 2).with_position(100),
            Candidate::new(2, 50, 2, true, 2).with_position(100),
            Candidate::new(3, 52, 1, true, 2).with_position(200),
        ];
        let sample = |seed| {
            let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
            select_candidates(Stage::Random, 50, &candidates, &settings, &mut rng)
        };
        for seed in 0..32 {
            let chosen = sample(seed);
            assert_eq!(chosen, sample(seed));
            assert_eq!(chosen.len(), 2);
            assert!(chosen.contains(&1));
            assert!(chosen.contains(&3));
            assert!(!chosen.contains(&2));
        }
    }

    #[test]
    fn rotation_and_prolong_frequency_are_explicit() {
        let mut settings = AnalysisSettings::default();
        settings.prolong_frequency = 2;
        assert_eq!(
            (1..=9).map(|n| settings.stage(n)).collect::<Vec<_>>(),
            vec![
                Stage::Random,
                Stage::Alternatives,
                Stage::Base,
                Stage::Random,
                Stage::Alternatives,
                Stage::Prolong,
                Stage::Random,
                Stage::Alternatives,
                Stage::Base,
            ]
        );
        settings.prolong_frequency = 0;
        assert_eq!(settings.stage(3), Stage::Prolong);
        assert_eq!(settings.stage(6), Stage::Prolong);
    }

    #[test]
    fn mode_zero_accepts_time_or_depth_but_requires_score() {
        let op = AnalysisOperation::TimeOrDepth;
        assert_eq!(op.decide(999, 9, 1000, 10, true), Completion::Waiting);
        assert_eq!(op.decide(1000, 9, 1000, 10, true), Completion::Success);
        assert_eq!(op.decide(999, 10, 1000, 10, true), Completion::Success);
        assert_eq!(op.decide(1000, 10, 1000, 10, false), Completion::Waiting);
    }

    #[test]
    fn mode_one_needs_both_unless_hard_timeout() {
        let op = AnalysisOperation::TimeAndDepth;
        assert_eq!(op.decide(1000, 9, 1000, 10, true), Completion::Waiting);
        assert_eq!(op.decide(999, 10, 1000, 10, true), Completion::Waiting);
        assert_eq!(op.decide(1000, 10, 1000, 10, true), Completion::Success);
        assert_eq!(op.decide(10_000, 0, 1000, 10, true), Completion::Success);
    }

    #[test]
    fn hard_timeout_is_bounded_even_without_a_valid_pv() {
        for mode in [
            AnalysisOperation::TimeOrDepth,
            AnalysisOperation::TimeAndDepth,
            AnalysisOperation::Ungated,
        ] {
            assert_eq!(mode.decide(10_000, 0, 1000, 10, false), Completion::Timeout);
            assert_eq!(mode.decide(9999, 0, 1000, 10, false), Completion::Waiting);
        }
        assert_eq!(
            AnalysisOperation::Ungated.decide(0, 1, 1000, 10, true),
            Completion::Success
        );
    }
}
