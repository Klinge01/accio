use crate::engine::Hit;
use serde::{Deserialize, Serialize};
use std::{fmt::Write, path::Path};

pub const NOTES_PROMPT: &str = "You are a fast assistant. Answer concisely in the language of the question. NOTES below are excerpts from the user's own files. If any note is relevant, base the answer on the notes and cite every note you use with its number in square brackets, e.g. [1] or [2][5], right after the statement it supports. Only if no note is relevant, answer from general knowledge and start the answer with \"(not in notes)\". Never write \"(not in notes)\" in an answer that uses or cites a note.";
pub const GENERAL_PROMPT: &str = "You are a fast assistant. Answer concisely in the language of the question. No relevant notes were found: answer from general knowledge and start the answer with \"(not in notes)\".";

const CODE_PROMPT: &str = "You are a fast assistant for the codebase \"{repo}\". Answer concisely in the language of the question. CODE below are search results from the repository, each with its file and line numbers. Base the answer on the code: name the classes, methods and files involved, and cite every excerpt you use with its number in square brackets, e.g. [1] or [2][5], right after the statement it supports. The excerpts may be incomplete: if they don't answer the question, say what is missing and which classes or files to look at next, and start the answer with \"(not found in {repo})\".";

/// Conversation memory sent to the LLM: last 6 question/answer pairs.
pub const HISTORY_MESSAGES: usize = 12;
/// Previous-turn chunks carried into a follow-up, and the overall context cap.
pub const MAX_CARRIED: usize = 4;
pub const MAX_CONTEXT: usize = 10;
/// Per-excerpt cap in the prompt, so one oversized chunk cannot bloat the request.
pub const MAX_EXCERPT_CHARS: usize = 3000;

fn excerpt(text: &str) -> std::borrow::Cow<'_, str> {
    match text.char_indices().nth(MAX_EXCERPT_CHARS) {
        Some((cut, _)) => format!("{} …[truncated]", &text[..cut]).into(),
        None => text.into(),
    }
}

/// Context for a follow-up: fresh hits first, then up to `MAX_CARRIED` chunks from
/// the previous answer that are not already present, capped at `MAX_CONTEXT`.
pub fn merge_followup(fresh: Vec<Hit>, carried: Vec<Hit>) -> Vec<Hit> {
    let mut hits = fresh;
    let carried: Vec<_> = carried
        .into_iter()
        .filter(|old| !hits.iter().any(|hit| hit.chunk_id == old.chunk_id))
        .take(MAX_CARRIED)
        .collect();
    hits.extend(carried);
    hits.truncate(MAX_CONTEXT);
    hits
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Turn {
    pub role: String,
    pub content: String,
}

impl Turn {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

pub fn retrieval_query(question: &str, history: &[Turn]) -> String {
    history
        .iter()
        .rev()
        .find(|turn| turn.role == "user")
        .map_or_else(
            || question.to_owned(),
            |last| format!("{}\n{question}", last.content),
        )
}

/// `path:12-40` for a code hit, relative to the repository root when possible.
pub fn code_location(hit: &Hit, root: &Path) -> String {
    let path = hit.path.strip_prefix(root).unwrap_or(&hit.path).display();
    match (hit.line, hit.end_line) {
        (Some(start), Some(end)) if end > start => format!("{path}:{start}-{end}"),
        (Some(start), _) => format!("{path}:{start}"),
        _ => path.to_string(),
    }
}

pub fn build_messages(question: &str, hits: &[Hit], history: &[Turn]) -> Vec<Turn> {
    let mut system = if hits.is_empty() {
        GENERAL_PROMPT
    } else {
        NOTES_PROMPT
    }
    .to_owned();
    for (index, hit) in hits.iter().enumerate() {
        system.push_str(if index == 0 { "\n\nNOTES:\n" } else { "\n\n" });
        let page = hit
            .page
            .map(|page| format!(", p.{page}"))
            .unwrap_or_default();
        write!(
            system,
            "[{}] {} ({}{page})\n{}",
            index + 1,
            hit.header,
            hit.path.display(),
            excerpt(&hit.text)
        )
        .expect("writing to a String cannot fail");
    }
    with_history(system, question, history)
}

/// Messages for a question about the code repository at `root`, named `repo`.
pub fn build_code_messages(
    repo: &str,
    root: &Path,
    question: &str,
    hits: &[Hit],
    history: &[Turn],
) -> Vec<Turn> {
    let mut system = CODE_PROMPT.replace("{repo}", repo);
    for (index, hit) in hits.iter().enumerate() {
        system.push_str(if index == 0 { "\n\nCODE:\n" } else { "\n\n" });
        write!(
            system,
            "[{}] {} ({})\n{}",
            index + 1,
            hit.header,
            code_location(hit, root),
            excerpt(&hit.text)
        )
        .expect("writing to a String cannot fail");
    }
    if hits.is_empty() {
        system.push_str("\n\nCODE: no matches.");
    }
    with_history(system, question, history)
}

fn with_history(system: String, question: &str, history: &[Turn]) -> Vec<Turn> {
    let recent: Vec<_> = history
        .iter()
        .rev()
        .filter(|turn| matches!(turn.role.as_str(), "user" | "assistant"))
        .take(HISTORY_MESSAGES)
        .cloned()
        .collect();
    let mut messages = vec![Turn::new("system", system)];
    messages.extend(recent.into_iter().rev());
    messages.push(Turn::new("user", question));
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_general_mode_when_no_hits() {
        let messages = build_messages("Question?", &[], &[]);
        assert_eq!(
            messages,
            [
                Turn::new("system", GENERAL_PROMPT),
                Turn::new("user", "Question?")
            ]
        );
    }

    #[test]
    fn prompt_notes_block_numbering() {
        let note = Hit {
            chunk_id: 1,
            path: "/x/n.md".into(),
            header: "n".into(),
            text: "note".into(),
            page: None,
            line: None,
            end_line: None,
            score: 1.0,
            stale: false,
        };
        let pdf = Hit {
            chunk_id: 2,
            path: "/x/f.pdf".into(),
            header: "f.pdf › p.3".into(),
            page: Some(3),
            ..note.clone()
        };
        let messages = build_messages("Question?", &[note, pdf], &[]);
        assert!(messages[0].content.starts_with(NOTES_PROMPT));
        assert!(
            messages[0]
                .content
                .contains("NOTES:\n[1] n (/x/n.md)\nnote\n\n[2] f.pdf › p.3 (/x/f.pdf, p.3)")
        );
    }

    #[test]
    fn code_prompt_names_repo_and_relative_lines() {
        let mut found = hit(1);
        found.path = "/repo/src/Order.java".into();
        found.header = "shop.Order › save()".into();
        found.text = "void save() {}".into();
        found.line = Some(12);
        found.end_line = Some(20);
        let system = &build_code_messages("shop", Path::new("/repo"), "q", &[found], &[])[0].content;
        assert!(system.contains("codebase \"shop\""));
        assert!(system.contains("(not found in shop)"));
        assert!(system.contains("CODE:\n[1] shop.Order › save() (src/Order.java:12-20)\nvoid save() {}"));
        let none = &build_code_messages("shop", Path::new("/repo"), "q", &[], &[])[0].content;
        assert!(none.ends_with("CODE: no matches."));
    }

    #[test]
    fn history_capped_to_six_turns_and_previous_question_used() {
        let turns: Vec<_> = (0..8)
            .flat_map(|i| {
                [
                    Turn::new("user", format!("q{i}")),
                    Turn::new("assistant", format!("a{i}")),
                ]
            })
            .collect();
        let messages = build_messages("current", &[], &turns);
        assert_eq!(messages.len(), 14);
        assert_eq!(messages[1].content, "q2");
        assert_eq!(messages[12].content, "a7");
        assert_eq!(retrieval_query("current", &turns), "q7\ncurrent");
    }

    #[test]
    fn oversized_excerpt_is_truncated_on_a_char_boundary() {
        let mut big = hit(1);
        big.text = "ä".repeat(MAX_EXCERPT_CHARS + 500);
        let system = &build_messages("q", &[big], &[])[0].content;
        assert!(system.ends_with(" …[truncated]"));
        assert_eq!(system.matches('ä').count(), MAX_EXCERPT_CHARS);
        let mut small = hit(2);
        small.text = "kurz".into();
        assert!(build_messages("q", &[small], &[])[0].content.ends_with("kurz"));
    }

    fn hit(id: i64) -> Hit {
        Hit {
            chunk_id: id,
            path: format!("/x/{id}.md").into(),
            header: id.to_string(),
            text: String::new(),
            page: None,
            line: None,
            end_line: None,
            score: 0.0,
            stale: false,
        }
    }

    fn ids(hits: &[Hit]) -> Vec<i64> {
        hits.iter().map(|h| h.chunk_id).collect()
    }

    #[test]
    fn followup_keeps_fresh_first_and_dedupes_carried() {
        let merged = merge_followup(vec![hit(1), hit(2)], vec![hit(2), hit(3), hit(4)]);
        assert_eq!(ids(&merged), [1, 2, 3, 4]);
    }

    #[test]
    fn followup_caps_carried_and_total() {
        let carried = (10..20).map(hit).collect();
        assert_eq!(ids(&merge_followup(vec![hit(1)], carried)), [1, 10, 11, 12, 13]);
        let fresh = (0..8).map(hit).collect();
        let carried = (10..20).map(hit).collect();
        assert_eq!(merge_followup(fresh, carried).len(), MAX_CONTEXT);
    }

    #[test]
    fn followup_without_fresh_hits_uses_carried_notes() {
        assert_eq!(ids(&merge_followup(Vec::new(), vec![hit(5)])), [5]);
    }
}
