//! (Neptune) `VICTORY_UM_COALESCE_KB`: joins changed ranges sent up to a card copy across small gaps.

/// Gap in bytes under which ranges are joined (`VICTORY_UM_COALESCE_KB`, in KiB; 0 or unset: off).
pub(crate) fn gap_bytes() -> usize {
    static GAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *GAP.get_or_init(|| std::env::var("VICTORY_UM_COALESCE_KB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(0) << 10)
}

/// `ranges` (sorted, none touching another) with neighbours joined whose gap is at most `gap`
/// bytes and holds no byte of `blocked` (sorted ranges, same offsets: bytes the card side may
/// hold newer than the CPU side, which a send of the gap would undo).
pub(crate) fn merge_gaps(ranges: &[(usize, usize)], gap: usize, blocked: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    let mut at = 0; // first blocked range that may still reach the gap
    for &(from, to) in ranges {
        if let Some(last) = out.last_mut() {
            let (lo, hi) = (last.1, from);
            while at < blocked.len() && blocked[at].1 <= lo {
                at += 1;
            }
            // Blocked ranges end after `lo`; one starting before `hi` lies in the gap.
            if hi - lo <= gap && blocked.get(at).is_none_or(|b| b.0 >= hi) {
                last.1 = to;
                continue;
            }
        }
        out.push((from, to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::merge_gaps;

    #[test]
    fn joins_small_gaps_only() {
        let r = [(0, 10), (15, 20), (100, 110)];
        assert_eq!(merge_gaps(&r, 5, &[]), vec![(0, 20), (100, 110)]);
        assert_eq!(merge_gaps(&r, 4, &[]), r.to_vec());
        assert_eq!(merge_gaps(&r, 0, &[]), r.to_vec());
        assert_eq!(merge_gaps(&r, 1000, &[]), vec![(0, 110)]);
    }

    #[test]
    fn never_joins_across_blocked_bytes() {
        let r = [(0, 10), (20, 30), (40, 50)];
        // A pending range inside the first gap: only the second gap joins.
        assert_eq!(merge_gaps(&r, 100, &[(12, 13)]), vec![(0, 10), (20, 50)]);
        // Touching the gap's edge is not inside it.
        assert_eq!(merge_gaps(&r, 100, &[(5, 10), (40, 45)]), vec![(0, 50)]);
        // A blocked range spanning both gaps and the range between.
        assert_eq!(merge_gaps(&r, 100, &[(5, 45)]), r.to_vec());
        // Blocked range beyond the gap, earlier ones skipped.
        assert_eq!(merge_gaps(&r, 100, &[(0, 3), (45, 60)]), vec![(0, 50)]);
    }
}
