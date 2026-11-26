use crate::lang::{is_function_word, words};
use std::collections::HashMap;

pub fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<_> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().nth(1).is_some())
        .map(|token| format!("\"{token}\""))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// Content words of a query: no function words, at least 3 characters, unique.
pub fn content_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in words(query) {
        if word.chars().count() >= 3 && !is_function_word(&word) && !terms.contains(&word) {
            terms.push(word);
        }
    }
    terms
}

/// FTS query matching any of `terms` as a prefix (so "planet" also matches "Planeten").
pub fn prefix_query(terms: &[String]) -> Option<String> {
    (!terms.is_empty()).then(|| {
        terms
            .iter()
            .map(|term| format!("\"{term}\"*"))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// Whether `text` contains enough of `terms` (as word prefixes) to count as a keyword
/// match: all of them for one or two terms, all but one for more.
pub fn keyword_match(text: &str, terms: &[String]) -> bool {
    let text_words: Vec<String> = words(text).collect();
    let found = terms
        .iter()
        .filter(|term| text_words.iter().any(|word| word.starts_with(term.as_str())))
        .count();
    let needed = if terms.len() <= 2 { terms.len() } else { terms.len() - 1 };
    !terms.is_empty() && found >= needed
}

/// Collapses ids whose content key is equal (the same text indexed from copies of a
/// file) to the first id met, scanning `lists` in order. Order inside each list is
/// kept; later repeats are dropped. Returns the rewritten lists.
pub fn dedupe<K: Eq + std::hash::Hash>(
    lists: Vec<Vec<i64>>,
    key: impl Fn(i64) -> Option<K>,
) -> Vec<Vec<i64>> {
    let mut canonical: HashMap<K, i64> = HashMap::new();
    lists
        .into_iter()
        .map(|list| {
            let mut seen = Vec::with_capacity(list.len());
            for id in list {
                let id = match key(id) {
                    Some(key) => *canonical.entry(key).or_insert(id),
                    None => id,
                };
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            seen
        })
        .collect()
}

pub fn rrf(lists: &[Vec<i64>], k: f32) -> Vec<i64> {
    let mut scores = HashMap::new();
    let mut order = 0;
    for list in lists {
        for (rank, &id) in list.iter().enumerate() {
            let entry = scores.entry(id).or_insert_with(|| {
                order += 1;
                (0.0, order)
            });
            entry.0 += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut ranked: Vec<_> = scores.into_iter().collect();
    ranked.sort_unstable_by(|(_, a), (_, b)| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    ranked.into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use anyhow::Result;

    #[test]
    fn fts_query_sanitizes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let store = Store::open(&temp.path().join("index.sqlite"), "test", 4)?;
        for (input, expected) in [
            (
                "C++ AND -foo* NEAR(x) \"a",
                Some(r#""AND" OR "foo" OR "NEAR""#),
            ),
            ("", None),
            ("-*:\"", None),
            ("Größe 2026", Some(r#""Größe" OR "2026""#)),
        ] {
            let query = fts_query(input);
            assert_eq!(query.as_deref(), expected);
            if let Some(query) = query {
                store.fts_search(&query, 50)?;
            }
        }
        Ok(())
    }

    #[test]
    fn keyword_terms_and_coverage() -> Result<()> {
        let terms = content_terms("What is a planet transit?");
        assert_eq!(terms, ["planet", "transit"]);
        assert_eq!(prefix_query(&terms).as_deref(), Some(r#""planet"* OR "transit"*"#));
        assert!(keyword_match("Der Planeten-Transit (TRAPPIST)", &terms));
        assert!(!keyword_match("Der Mond-Transit", &terms));

        let terms = content_terms("Wie funktioniert der TRAPPIST-Transit?");
        assert_eq!(terms, ["funktioniert", "trappist", "transit"]);
        assert!(keyword_match("TRAPPIST Transit: Helligkeit, Umlaufzeit", &terms));
        assert!(!keyword_match("Transit stages", &terms));

        assert!(content_terms("what is it?").is_empty());
        assert!(!keyword_match("anything", &[]));
        let temp = tempfile::tempdir()?;
        let store = Store::open(&temp.path().join("index.sqlite"), "test", 4)?;
        let terms = content_terms("C++ \"NEAR\" (planet) transit*");
        store.fts_search(&prefix_query(&terms).unwrap(), 5)?;
        Ok(())
    }

    #[test]
    fn dedupe_keeps_first_copy_across_lists() {
        let key = |id: i64| Some(id % 10); // 1, 11 and 21 share content
        assert_eq!(
            dedupe(vec![vec![11, 2, 1], vec![21, 3, 2]], key),
            [vec![11, 2], vec![11, 3, 2]]
        );
    }

    #[test]
    fn rrf_merges() {
        assert_eq!(rrf(&[vec![1, 2, 3], vec![3, 4]], 60.0), [3, 1, 2, 4]);
    }
}
