//! Fuzzy subsequence matching for completion candidates.
//!
//! Used by `input::completion_candidates_for`, `skill::completion_candidates`,
//! and `mcp::completion_candidates` so the user can type a partial,
//! case-insensitive sequence of characters and have every candidate
//! whose name contains that sequence as an ordered subsequence
//! surface in the picker.
//!
//! The match algorithm is intentionally simple:
//!
//! - Lowercase both query and candidate.
//! - Walk the candidate once, consuming query chars left-to-right as
//!   they appear in the candidate.
//! - Score is the sum of gap distances between consecutive matched
//!   query chars (contiguous matches score 0, scattered matches
//!   score higher).
//! - Empty query matches everything with score 0 so callers can
//!   short-circuit on `query.is_empty()` when listing all items.
//!
//! A candidate that does not contain the full query as a subsequence
//! returns `None`. Prefix matches naturally score 0 because there
//! are no gaps between the matched chars at the start.

/// Lowercase subsequence score. `None` when the query is not a
/// subsequence of `candidate`. Lower scores mean tighter matches
/// (smaller gaps between matched chars), so callers should sort by
/// `(score, candidate)` to surface the best hits first.
pub fn score(query: &str, candidate: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let q = query.to_ascii_lowercase();
    let c = candidate.to_ascii_lowercase();
    let mut q_chars = q.chars().peekable();
    let mut last_pos: Option<usize> = None;
    let mut total_skip: u32 = 0;
    for (i, ch) in c.char_indices() {
        let Some(&need) = q_chars.peek() else {
            break;
        };
        if ch == need {
            if let Some(p) = last_pos {
                // Gap of 0 means the next query char is right next to
                // the previous one (contiguous match); a gap of N
                // means we skipped N candidate chars in between.
                total_skip = total_skip.saturating_add((i - p - 1) as u32);
            }
            last_pos = Some(i);
            q_chars.next();
        }
    }
    if q_chars.peek().is_some() {
        return None;
    }
    Some(total_skip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_scores_zero() {
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn exact_prefix_scores_zero() {
        // `mod` is a contiguous prefix of `model` -> no gaps -> score 0.
        assert_eq!(score("mod", "model"), Some(0));
    }

    #[test]
    fn subsequence_matches_with_skip_penalty() {
        // `khg` appears in `karpathy-guidelines` at offsets 0, 6, 9.
        // Gap sums = 5 + 2 = 7.
        assert_eq!(score("khg", "karpathy-guidelines"), Some(7));
    }

    #[test]
    fn non_subsequence_returns_none() {
        assert_eq!(score("zzz", "model"), None);
        assert_eq!(score("abc", "cba"), None);
    }

    #[test]
    fn is_case_insensitive() {
        assert_eq!(score("KHG", "karpathy-guidelines"), Some(7));
        assert_eq!(score("khg", "Karpathy-Guidelines"), Some(7));
    }

    #[test]
    fn contiguous_chars_inside_word_score_lower_than_scattered() {
        // `mo` is a contiguous prefix of `model` (gap 0).
        assert_eq!(score("mo", "model"), Some(0));
        // `oe` skips `d` -> gap 1.
        assert_eq!(score("oe", "model"), Some(1));
    }
    #[test]
    fn tighter_match_outranks_looser_one() {
        let tight = score("mod", "model").unwrap();
        let loose = score("mdl", "model").unwrap();
        assert!(tight < loose, "tighter match must score lower");
    }
}
