//! History reverse-search scoring. The UI is a Lua plugin
//! (`runtime/lua/smelt/plugins/history_search.lua`) that calls into
//! `smelt.history.search(query)`, which wraps this function. Tests
//! exercise the scorer directly.

use super::score::{recency_bonus, split_words};

pub fn history_score(text: &str, query: &str, recency_rank: usize) -> Option<u32> {
    let base = crate::fuzzy::fuzzy_score(text, query)? as i64;
    let text_norm = text.trim().to_lowercase();
    let query_norm = query.trim().to_lowercase();

    if query_norm.is_empty() {
        return Some(recency_rank as u32);
    }

    let text_words = split_words(&text_norm);
    let query_words = split_words(&query_norm);
    let query_has_multiple_words = query_words.len() > 1;

    let mut score = base * 10;

    if text_norm == query_norm {
        score -= 2_000;
    } else if text_norm.starts_with(&query_norm) {
        score -= 200;
    }

    if !query_has_multiple_words && text_words.len() > 1 {
        score += ((text_words.len() - 1) as i64) * 60;
    }

    let mut saw_exact_word_match = false;
    let mut saw_prefix_word_match = false;
    let mut saw_substring_match = false;

    for word in &query_words {
        if text_words.iter().any(|candidate| candidate == word) {
            saw_exact_word_match = true;
            score -= 400;
        } else if text_words
            .iter()
            .any(|candidate| candidate.starts_with(word))
        {
            saw_prefix_word_match = true;
            score -= 140;
        } else if text_norm.contains(word) {
            saw_substring_match = true;
            score -= 40;
        }
    }

    if !query_has_multiple_words {
        if let Some(first_word) = query_words.first() {
            let boundary_prefix_matches = text_words
                .iter()
                .filter(|candidate| candidate.starts_with(first_word))
                .count();
            if boundary_prefix_matches > 0 {
                score -= 80;
            }
        }
    }

    if !query_has_multiple_words {
        // For single-word reverse search, plain fuzzy subsequence matches like
        // "default allow" for "full" should come well after true word hits.
        if !saw_exact_word_match && !saw_prefix_word_match && !saw_substring_match {
            score += 900;
        }
    }

    score -= recency_bonus(recency_rank);

    Some(score.max(0) as u32)
}

#[cfg(test)]
mod tests {
    use super::history_score;

    /// Rank `entries` (oldest first, like the live history vec) against `query`.
    /// Returns the original labels in best-first order, matching what Ctrl+R
    /// displays at the bottom of the picker.
    fn ranked(entries: &[&str], query: &str) -> Vec<String> {
        // Oldest-first → iterate reversed so the rank index marks recency.
        let mut scored: Vec<(u32, usize, String)> = entries
            .iter()
            .rev()
            .enumerate()
            .filter_map(|(rank, text)| {
                history_score(text, query, rank).map(|s| (s, rank, (*text).to_string()))
            })
            .collect();
        scored.sort_by_key(|(s, rank, _)| (*s, *rank));
        scored.into_iter().map(|(_, _, t)| t).collect()
    }

    #[test]
    fn prefers_exact_single_word_prompt() {
        let labels = ranked(&["hot dog bun", "bundle assets", "bun"], "bun");
        assert_eq!(labels.first().map(String::as_str), Some("bun"));
    }

    #[test]
    fn prefers_whole_word_over_embedded_match() {
        let labels = ranked(&["bundle assets", "hot dog bun"], "bun");
        let bun_pos = labels
            .iter()
            .position(|label| label == "hot dog bun")
            .unwrap();
        let bundle_pos = labels
            .iter()
            .position(|label| label == "bundle assets")
            .unwrap();
        assert!(bun_pos < bundle_pos, "whole-word bun should beat bundle");
    }

    #[test]
    fn prefers_more_recent_history_for_similar_matches() {
        let labels = ranked(&["older bun prompt", "newer bun prompt"], "bun");
        assert_eq!(labels.first().map(String::as_str), Some("newer bun prompt"));
    }

    #[test]
    fn prefers_real_word_match_over_fuzzy_letters() {
        let labels = ranked(
            &[
                "use the gh cli search for issue in the llama.cpp repo",
                "don't cat into a file, just tell me here",
                "create a full stack application fully with bun and typscript for recepies. work with subagents",
                "add them with default allow",
                "full",
            ],
            "full",
        );
        let exact_pos = labels.iter().position(|label| label == "full").unwrap();
        let word_pos = labels
            .iter()
            .position(|label| {
                label == "create a full stack application fully with bun and typscript for recepies. work with subagents"
            })
            .unwrap();
        let fuzzy_pos = labels
            .iter()
            .position(|label| label == "add them with default allow")
            .unwrap();

        assert!(
            exact_pos < word_pos,
            "exact match should beat longer word hit"
        );
        assert!(
            word_pos < fuzzy_pos,
            "word hit should beat fuzzy-only subsequence"
        );
    }
}
