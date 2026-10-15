//! White-relative analysis scores and a separate centipawn-only scheduling index.

/// A mate distance is signed for White's perspective; never approximate it as centipawns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisScore {
    Centipawns(i32),
    Mate(i32),
    Unknown,
}

/// The 100 upper bounds yield 101 possible indices (0 through 100).
const TREE_THRESHOLDS: [i32; 100] = [
    -30001, -1301, -1001, -801, -601, -501, -401, -351, -301, -276, -251, -226, -201, -186, -171,
    -161, -151, -141, -133, -125, -120, -115, -110, -105, -101, -96, -92, -87, -83, -79, -74, -70,
    -66, -62, -58, -54, -51, -47, -43, -39, -35, -32, -28, -24, -21, -17, -13, -10, -6, -2, 1, 5,
    9, 12, 16, 20, 23, 27, 31, 34, 38, 42, 46, 50, 53, 57, 61, 65, 69, 73, 78, 82, 86, 91, 95, 100,
    104, 109, 114, 119, 124, 132, 140, 150, 160, 170, 185, 200, 225, 250, 275, 300, 350, 400, 500,
    600, 800, 1000, 1300, 30000,
];

impl AnalysisScore {
    pub fn from_stored(kind: Option<&str>, value: Option<i32>) -> Result<Self, String> {
        match (kind, value) {
            (None, None) => Ok(Self::Unknown),
            (Some("cp"), Some(v)) => Ok(Self::Centipawns(v)),
            (Some("mate"), Some(v)) => Ok(Self::Mate(v)),
            _ => Err("invalid stored analysis score".into()),
        }
    }

    pub fn rank(self) -> i64 {
        match self {
            Self::Mate(v) if v > 0 => i64::MAX / 4 - i64::from(v),
            Self::Mate(v) if v < 0 => -i64::MAX / 4 - i64::from(v),
            Self::Centipawns(v) => i64::from(v),
            Self::Mate(_) | Self::Unknown => 0,
        }
    }

    pub fn tree_index(self) -> Option<u8> {
        match self {
            Self::Centipawns(cp) => Some(TREE_THRESHOLDS.partition_point(|&t| t < cp) as u8),
            Self::Mate(_) | Self::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AnalysisScore;

    #[test]
    fn persisted_scores_remain_typed_and_mate_ranks_above_any_centipawns() {
        assert_eq!(
            AnalysisScore::from_stored(Some("cp"), Some(0))
                .unwrap()
                .tree_index(),
            Some(50)
        );
        assert_eq!(
            AnalysisScore::from_stored(Some("mate"), Some(3))
                .unwrap()
                .tree_index(),
            None
        );
        assert!(AnalysisScore::Mate(3).rank() > AnalysisScore::Centipawns(i32::MAX).rank());
        assert!(AnalysisScore::Mate(-3).rank() < AnalysisScore::Centipawns(i32::MIN).rank());
        assert!(AnalysisScore::Mate(2).rank() > AnalysisScore::Mate(3).rank());
        assert!(AnalysisScore::Mate(-3).rank() > AnalysisScore::Mate(-2).rank());
        assert_eq!(
            AnalysisScore::from_stored(None, None).unwrap(),
            AnalysisScore::Unknown
        );
        assert!(AnalysisScore::from_stored(Some("bad"), Some(3)).is_err());
        assert!(AnalysisScore::from_stored(Some("cp"), None).is_err());
    }

    #[test]
    fn threshold_edges_are_first_greater_or_equal_not_rounded() {
        let thresholds = [
            -30001, -1301, -1001, -801, -601, -501, -401, -351, -301, -276, -251, -226, -201, -186,
            -171, -161, -151, -141, -133, -125, -120, -115, -110, -105, -101, -96, -92, -87, -83,
            -79, -74, -70, -66, -62, -58, -54, -51, -47, -43, -39, -35, -32, -28, -24, -21, -17,
            -13, -10, -6, -2, 1, 5, 9, 12, 16, 20, 23, 27, 31, 34, 38, 42, 46, 50, 53, 57, 61, 65,
            69, 73, 78, 82, 86, 91, 95, 100, 104, 109, 114, 119, 124, 132, 140, 150, 160, 170, 185,
            200, 225, 250, 275, 300, 350, 400, 500, 600, 800, 1000, 1300, 30000,
        ];
        assert_eq!(thresholds.len(), 100);
        for (i, bound) in thresholds.into_iter().enumerate() {
            assert_eq!(AnalysisScore::Centipawns(bound).tree_index(), Some(i as u8));
            if i != 0 {
                assert_eq!(
                    AnalysisScore::Centipawns(bound - 1).tree_index(),
                    Some(i as u8)
                );
            }
        }
        assert_eq!(AnalysisScore::Centipawns(-30002).tree_index(), Some(0));
        assert_eq!(AnalysisScore::Centipawns(30001).tree_index(), Some(100));
        assert_eq!(AnalysisScore::Centipawns(i32::MAX).tree_index(), Some(100));
    }

    #[test]
    fn zero_maps_to_fifty_but_mate_and_unknown_have_no_index() {
        assert_eq!(AnalysisScore::Centipawns(0).tree_index(), Some(50));
        assert_eq!(AnalysisScore::Mate(3).tree_index(), None);
        assert_eq!(AnalysisScore::Mate(-3).tree_index(), None);
        assert_eq!(AnalysisScore::Unknown.tree_index(), None);
    }
}
