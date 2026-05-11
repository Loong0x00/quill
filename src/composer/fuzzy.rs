use crate::completion::Suggestion;
pub fn fuzzy_filter(candidates: Vec<Suggestion>, query: &str) -> Vec<Suggestion> {
    if query.is_empty() {
        return candidates;
    }

    let mut scored = candidates
        .into_iter()
        .filter_map(|candidate| {
            let text_score = score_subsequence(&candidate.text, query);
            let display_score = score_subsequence(&candidate.display, query);
            text_score
                .max(display_score)
                .map(|score| (score, candidate))
        })
        .collect::<Vec<_>>();

    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.display.len().cmp(&right.display.len()))
            .then_with(|| left.display.cmp(&right.display))
    });
    scored.into_iter().map(|(_, candidate)| candidate).collect()
}
pub fn score_subsequence(text: &str, query: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }

    let text_chars: Vec<(usize, char)> = text.char_indices().collect();
    let query_chars: Vec<char> = query.chars().collect();
    let mut text_idx = 0usize;
    let mut last_match: Option<usize> = None;
    let mut score = 0i32;

    for query_char in query_chars {
        let mut matched = None;
        while text_idx < text_chars.len() {
            let (byte_idx, text_char) = text_chars[text_idx];
            text_idx += 1;
            if text_char.eq_ignore_ascii_case(&query_char) {
                matched = Some((byte_idx, text_char));
                break;
            }
        }

        let (byte_idx, text_char) = matched?;
        score += 10;
        if text_char == query_char {
            score += 2;
        }
        if last_match.is_some_and(|prev| prev + 1 == byte_idx) {
            score += 8;
        }
        if is_word_start(text, byte_idx) {
            score += 6;
        }
        score -= (byte_idx.min(i32::MAX as usize) as i32).min(20);
        last_match = Some(byte_idx);
    }

    Some(score)
}

fn is_word_start(text: &str, byte_idx: usize) -> bool {
    if byte_idx == 0 {
        return true;
    }
    text[..byte_idx]
        .chars()
        .next_back()
        .is_none_or(|ch| matches!(ch, '-' | '_' | '/' | '.' | ' '))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::SuggestionGroup;

    #[test]
    fn scoring_rules_cover_order_contiguous_and_word_start() {
        assert!(score_subsequence("checkout", "cho").is_some());
        assert!(score_subsequence("checkout", "coh").is_none());

        let contiguous = score_subsequence("checkout", "che").unwrap();
        let scattered = score_subsequence("cache-entry", "che").unwrap();
        assert!(contiguous > scattered);

        let word_start = score_subsequence("--all-files", "af").unwrap();
        let middle = score_subsequence("--waffle", "af").unwrap();
        assert!(word_start > middle);
    }

    #[test]
    fn filter_sorts_best_first() {
        let suggestion = |text: &str| Suggestion {
            text: text.to_string(),
            display: text.to_string(),
            description: String::new(),
            group: SuggestionGroup::Dynamic,
        };
        let out = fuzzy_filter(
            vec![
                suggestion("commit"),
                suggestion("checkout"),
                suggestion("cherry-pick"),
            ],
            "ch",
        );

        assert_eq!(out[0].text, "checkout");
        assert_eq!(out[1].text, "cherry-pick");
    }
}
