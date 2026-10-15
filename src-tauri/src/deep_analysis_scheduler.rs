//! Self-contained scheduler policy; no engine or database dependencies.
use serde::{Deserialize, Serialize};

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
    use super::{AnalysisOperation, AnalysisSettings, Completion, Stage};

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
