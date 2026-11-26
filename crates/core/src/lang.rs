//! German/English heuristics for query translation and keyword matching.

use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
        }
    }
}

const GERMAN: &[&str] = &[
    "der", "die", "das", "den", "dem", "des", "ein", "eine", "einen", "einem", "einer", "eines",
    "und", "oder", "aber", "ist", "sind", "war", "waren", "wird", "werden", "wurde", "hat",
    "haben", "hatte", "kann", "können", "muss", "müssen", "soll", "sollte", "nicht", "kein",
    "keine", "mit", "für", "von", "vom", "zum", "zur", "auf", "aus", "bei", "nach", "über",
    "unter", "zwischen", "durch", "gegen", "ohne", "um", "im", "ins", "am", "an", "als", "auch",
    "noch", "nur", "schon", "sehr", "wie", "was", "wer", "wen", "wem", "wessen", "wo", "wann",
    "warum", "wieso", "weshalb", "welche", "welcher", "welches", "welchen", "ich", "du", "er",
    "sie", "es", "wir", "ihr", "mein", "meine", "unser", "unsere", "dass", "wenn", "denn",
    "gibt", "es", "man", "sich", "dieser", "diese", "dieses", "eigentlich", "erkläre", "erklär",
];

const ENGLISH: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "is", "are", "was", "were", "be", "been", "being",
    "will", "would", "has", "have", "had", "do", "does", "did", "can", "could", "should", "must",
    "not", "no", "with", "for", "from", "of", "to", "in", "on", "at", "by", "about", "into",
    "over", "under", "between", "through", "without", "as", "also", "only", "very", "how",
    "what", "who", "whom", "whose", "where", "when", "why", "which", "i", "you", "he", "she",
    "it", "we", "they", "my", "our", "your", "their", "this", "that", "these", "those", "there",
    "then", "than", "if", "so", "me", "us", "them", "its", "explain", "tell", "please",
];

static GERMAN_WORDS: LazyLock<HashSet<&str>> = LazyLock::new(|| GERMAN.iter().copied().collect());
static ENGLISH_WORDS: LazyLock<HashSet<&str>> = LazyLock::new(|| ENGLISH.iter().copied().collect());

pub fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

/// True for German or English function words ("was", "ist", "the", "how", ...).
pub fn is_function_word(word: &str) -> bool {
    GERMAN_WORDS.contains(word) || ENGLISH_WORDS.contains(word)
}

/// German when German function words or umlauts outweigh English function words.
/// Anything else (including keyword-only queries) counts as English.
pub fn detect(text: &str) -> Lang {
    let mut german = text.chars().filter(|c| "äöüßÄÖÜ".contains(*c)).count().min(1);
    let mut english = 0;
    for word in words(text) {
        // Words in both lists ("in", "an", "was", "die" as a verb) count for neither.
        match (GERMAN_WORDS.contains(word.as_str()), ENGLISH_WORDS.contains(word.as_str())) {
            (true, false) => german += 1,
            (false, true) => english += 1,
            _ => {}
        }
    }
    if german > english { Lang::De } else { Lang::En }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_question_language() {
        for (text, lang) in [
            ("what is a planet transit?", Lang::En),
            ("TRAPPIST transit then?", Lang::En),
            ("how do I measure rainfall per month?", Lang::En),
            ("Was ist der Unterschied zwischen Laptop und Tablet?", Lang::De),
            ("Welche Pflanzen eignen sich für schattige Balkone?", Lang::De),
            ("Wie funktioniert der Sleep-Wake-Cycle?", Lang::De),
            ("Regenmenge", Lang::En),
            ("Größe", Lang::De),
            ("", Lang::En),
        ] {
            assert_eq!(detect(text), lang, "{text}");
        }
    }

    #[test]
    fn function_words_in_both_languages() {
        assert!(is_function_word("what") && is_function_word("welche"));
        assert!(!is_function_word("transit") && !is_function_word("planet"));
    }
}
