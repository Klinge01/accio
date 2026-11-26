//! Code repositories: search for coding agents (through the local socket and
//! `accio-mcp`) and for the panel's `@repo` questions.

use crate::{
    engine::{Engine, Hit},
    ingest::IngestManager,
    prompt::code_location,
    scan::CODE_TEST_GLOBS,
    search::content_terms,
    store::{Source, SourceKind, SourceStatus},
};
use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    fmt::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

/// A search starts a background re-index of the repository when its files were last
/// checked longer ago than this. The search itself never waits for it.
pub const REFRESH_AFTER: Duration = Duration::from_secs(60);
pub const DEFAULT_LIMIT: usize = 8;
pub const MAX_LIMIT: usize = 20;
/// Lines of code shown per hit.
const SNIPPET_LINES: usize = 4;
const SNIPPET_LINE_CHARS: usize = 240;
const SYMBOL_CHARS: usize = 160;
/// Most lines of file context an agent can ask for per hit.
pub const MAX_CONTEXT: usize = 80;
/// Candidates searched when a path or kind filter narrows the results.
const FILTERED_CANDIDATES: usize = 240;
/// Documentation is second tier: prose matches prose questions better than code does,
/// so without a cap README sections crowd out the code they describe.
pub const MAX_DOC_HITS: usize = 2;
/// Lines of an exhaustive usage search kept and shown; files beyond are only listed.
const MAX_USAGE_LINES: usize = 300;
const USAGE_LINE_CHARS: usize = 200;
/// File kinds, in the order usage results list them.
pub const KINDS: [&str; 5] = ["code", "sql", "config", "test", "doc"];

fn test_globs() -> &'static GlobSet {
    static GLOBS: OnceLock<GlobSet> = OnceLock::new();
    GLOBS.get_or_init(|| {
        let mut builder = GlobSetBuilder::new();
        for glob in CODE_TEST_GLOBS {
            // Folder globs also cover everything inside the folder.
            for pattern in [glob.to_string(), format!("{glob}/**")] {
                if let Ok(glob) = GlobBuilder::new(&pattern).literal_separator(true).build() {
                    builder.add(glob);
                }
            }
        }
        builder.build().unwrap_or_else(|_| GlobSet::empty())
    })
}

/// Kind of a file by its path relative to the repository root: one of `KINDS`.
pub fn kind_of(relative: &Path) -> &'static str {
    if test_globs().is_match(relative) {
        return "test";
    }
    let ext = relative
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" | "mdx" | "rst" | "adoc" | "txt" => "doc",
        "sql" => "sql",
        "yml" | "yaml" | "properties" | "json" | "toml" | "xml" | "ini" => "config",
        _ => "code",
    }
}

/// Optional narrowing of a search, as an agent passes it.
#[derive(Clone, Debug, Default)]
pub struct SearchOptions {
    pub limit: usize,
    /// Folder or file prefix relative to the repository root, e.g. `app/frontend/src`.
    pub path: Option<String>,
    /// One of `KINDS`.
    pub kind: Option<String>,
    /// Lines of the current file to show per hit instead of the short snippet.
    pub context: usize,
}

impl SearchOptions {
    pub fn limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(kind) = &self.kind {
            anyhow::ensure!(
                KINDS.contains(&kind.as_str()),
                "unknown kind \"{kind}\"; use one of {}",
                KINDS.join(", ")
            );
        }
        Ok(())
    }

    fn prefix(&self) -> Option<PathBuf> {
        let path = self
            .path
            .as_deref()?
            .trim()
            .trim_start_matches("./")
            .trim_end_matches('/');
        (!path.is_empty() && path != ".").then(|| PathBuf::from(path))
    }

    fn matches(&self, relative: &Path) -> bool {
        self.prefix()
            .is_none_or(|prefix| relative.starts_with(prefix))
            && self
                .kind
                .as_deref()
                .is_none_or(|kind| kind_of(relative) == kind)
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(prefix) = self.prefix() {
            parts.push(format!("in {}", prefix.display()));
        }
        if let Some(kind) = &self.kind {
            parts.push(format!("kind {kind}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    }
}

/// The first `limit` hits keeping at most `max_docs` documentation hits.
fn cap_docs(hits: Vec<Hit>, limit: usize, max_docs: usize, root: &Path) -> Vec<Hit> {
    let mut docs = 0;
    hits.into_iter()
        .filter(|hit| {
            if kind_of(relative(hit, root)) != "doc" {
                return true;
            }
            docs += 1;
            docs <= max_docs
        })
        .take(limit)
        .collect()
}

fn relative<'a>(hit: &'a Hit, root: &Path) -> &'a Path {
    hit.path.strip_prefix(root).unwrap_or(&hit.path)
}

#[derive(Clone, Debug, Serialize)]
pub struct CodeHit {
    /// Relative to the repository root.
    pub path: String,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub symbol: String,
    /// One of `KINDS`.
    pub kind: &'static str,
    pub score: f32,
    /// The file changed after it was indexed; line numbers may be off.
    pub stale: bool,
    /// A few lines around the best match (or the requested context, read from the
    /// file), with their line numbers when known.
    pub snippet: Vec<(Option<u32>, String)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CodeSearch {
    pub repo: String,
    pub root: PathBuf,
    pub query: String,
    /// Filters applied, for the header line ("" when none).
    pub filters: String,
    pub translation: Option<String>,
    /// A re-index of the repository is queued or running.
    pub refreshing: bool,
    pub hits: Vec<CodeHit>,
    pub ms: f32,
}

pub struct CodeService {
    engine: Arc<Engine>,
    ingest: Arc<IngestManager>,
    checked: Mutex<HashMap<i64, Instant>>,
}

/// Display name of a repository: its folder name.
pub fn repo_name(source: &Source) -> String {
    source.path.file_name().map_or_else(
        || source.path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// The main checkout of the git repository containing `path`. For a worktree this is
/// the repository the worktree belongs to.
fn main_checkout(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
    let checkout = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent()?.to_owned()
    } else {
        common
    };
    checkout.canonicalize().ok()
}

fn containing<'a>(sources: &'a [Source], path: &Path) -> Option<&'a Source> {
    sources
        .iter()
        .filter(|source| path.starts_with(&source.path))
        .max_by_key(|source| source.path.components().count())
}

fn lines_of(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn shorten(text: &str, limit: usize) -> String {
    let text = text.trim_end();
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// The `SNIPPET_LINES` lines of `hit` that mention the most query terms, numbered when
/// the chunk is a verbatim slice of the file.
fn snippet(hit: &Hit, terms: &[String]) -> Vec<(Option<u32>, String)> {
    let lines = lines_of(&hit.text);
    let verbatim = match (hit.line, hit.end_line) {
        (Some(start), Some(end)) => end >= start && (end - start + 1) as usize == lines.len(),
        _ => false,
    };
    let mentions = |line: &str| {
        let line = line.to_lowercase();
        terms
            .iter()
            .filter(|term| line.contains(term.as_str()))
            .count()
    };
    let window = SNIPPET_LINES.min(lines.len());
    let best = (0..=lines.len().saturating_sub(window))
        .max_by_key(|&start| {
            let score: usize = lines[start..start + window]
                .iter()
                .map(|line| mentions(line))
                .sum();
            // Prefer earlier windows on ties (max_by_key keeps the last maximum).
            (score, std::cmp::Reverse(start))
        })
        .unwrap_or(0);
    lines[best..best + window]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let number = verbatim.then(|| hit.line.unwrap() + (best + offset) as u32);
            (number, shorten(line, SNIPPET_LINE_CHARS))
        })
        .collect()
}

/// Up to `lines` lines of the file as it is now, covering the hit: the whole chunk when
/// it fits, otherwise a window around `center`. None when the file cannot be read.
fn file_context(
    path: &Path,
    hit: &Hit,
    center: Option<u32>,
    lines: usize,
) -> Option<Vec<(Option<u32>, String)>> {
    let start_line = hit.line? as usize;
    let text = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&text);
    let file: Vec<&str> = text.lines().collect();
    if file.is_empty() {
        return None;
    }
    let end_line = hit
        .end_line
        .map_or(start_line, |end| end as usize)
        .max(start_line);
    let first = if end_line - start_line < lines {
        start_line
    } else {
        // Some context above the best line, but never above the chunk.
        center.map_or(start_line, |line| {
            (line as usize).saturating_sub(lines / 3).max(start_line)
        })
    };
    let first = first.clamp(1, file.len());
    let last = if end_line - start_line < lines {
        end_line
    } else {
        first + lines - 1
    };
    let last = last.min(file.len()).min(first + lines - 1);
    Some(
        (first..=last)
            .map(|number| (Some(number as u32), shorten(file[number - 1], 300)))
            .collect(),
    )
}

/// Top folder of the git checkout containing `path` (a worktree's own folder).
fn checkout_root(path: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(PathBuf::from(String::from_utf8(output.stdout).ok()?.trim()))
}

/// Every line mentioning a string, for "who references this" questions.
#[derive(Clone, Debug, Serialize)]
pub struct CodeUsages {
    pub repo: String,
    pub root: PathBuf,
    pub query: String,
    pub filters: String,
    /// Matched as a whole word (identifiers) rather than anywhere in a line.
    pub whole_word: bool,
    /// Matching lines in all files, including those not kept in `files`.
    pub total: usize,
    pub files: Vec<UsageFile>,
    pub ms: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageFile {
    pub path: String,
    pub kind: &'static str,
    /// Matching lines in this file.
    pub count: usize,
    /// The matching lines kept (all of them unless the result is very long).
    pub lines: Vec<(u32, String)>,
}

/// Parses `git grep -n --full-name` output into files sorted by kind, then path.
fn parse_grep(output: &str, options: &SearchOptions) -> (usize, Vec<UsageFile>) {
    let mut files: BTreeMap<&str, Vec<(u32, &str)>> = BTreeMap::new();
    for line in output.lines() {
        let mut parts = line.splitn(3, ':');
        let (Some(path), Some(number), Some(text)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let Ok(number) = number.parse() else { continue };
        if options.matches(Path::new(path)) {
            files.entry(path).or_default().push((number, text));
        }
    }
    let mut files: Vec<_> = files
        .into_iter()
        .map(|(path, lines)| (kind_of(Path::new(path)), path, lines))
        .collect();
    files.sort_by_key(|(kind, path, _)| (KINDS.iter().position(|k| k == kind), *path));
    let total = files.iter().map(|(_, _, lines)| lines.len()).sum();
    let mut kept = 0;
    let files = files
        .into_iter()
        .map(|(kind, path, lines)| {
            let keep = lines.len().min(MAX_USAGE_LINES - kept);
            kept += keep;
            UsageFile {
                path: path.to_owned(),
                kind,
                count: lines.len(),
                lines: lines[..keep]
                    .iter()
                    .map(|(number, text)| (*number, shorten(text.trim(), USAGE_LINE_CHARS)))
                    .collect(),
            }
        })
        .collect();
    (total, files)
}

impl CodeService {
    pub fn new(engine: Arc<Engine>, ingest: Arc<IngestManager>) -> Arc<Self> {
        Arc::new(Self {
            engine,
            ingest,
            checked: Mutex::new(HashMap::new()),
        })
    }

    pub fn engine(&self) -> &Arc<Engine> {
        &self.engine
    }

    /// The code repository containing `path`. A worktree maps to its main checkout.
    pub fn find_source(&self, path: &Path) -> Result<Option<Source>> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        let sources: Vec<_> = self
            .engine
            .store()
            .list_sources()?
            .into_iter()
            .filter(|source| source.kind == SourceKind::Code)
            .collect();
        if let Some(source) = containing(&sources, &path) {
            return Ok(Some(source.clone()));
        }
        Ok(main_checkout(&path).and_then(|checkout| containing(&sources, &checkout).cloned()))
    }

    /// Starts a background re-index when the repository's files were last checked more
    /// than `REFRESH_AFTER` ago. Returns whether a re-index is queued or running.
    pub fn refresh_if_due(&self, source: &Source) -> bool {
        let source_id = source.id;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let indexed_recently = source
            .last_ingested_at
            .is_some_and(|then| now - then < REFRESH_AFTER.as_secs() as i64);
        let mut checked = self.checked.lock().unwrap();
        let due = !indexed_recently
            && checked
                .get(&source_id)
                .is_none_or(|last| last.elapsed() >= REFRESH_AFTER);
        if due && !self.ingest.is_active(source_id) {
            checked.insert(source_id, Instant::now());
            self.ingest.start(source_id, false);
        }
        self.ingest.is_active(source_id)
    }

    /// Searches the code repository containing `path` (an agent's working directory).
    pub fn search(&self, path: &Path, query: &str, options: &SearchOptions) -> Result<CodeSearch> {
        let query = query.trim();
        anyhow::ensure!(!query.is_empty(), "empty query");
        options.validate()?;
        anyhow::ensure!(
            options.kind.as_deref() != Some("test"),
            "tests are not in the search index; use mode \"usages\" to list every test that mentions an identifier"
        );
        let source = self.find_source(path)?.with_context(|| {
            format!(
                "no code repository indexed for {}; add it in Accio → Add code repository…",
                path.display()
            )
        })?;
        self.search_with(&source, query, options)
    }

    /// Every line in the agent's checkout containing `needle`, from `git grep`: complete
    /// and unranked, including tests and files not yet indexed or committed.
    pub fn usages(&self, path: &Path, needle: &str, options: &SearchOptions) -> Result<CodeUsages> {
        let started = Instant::now();
        let needle = needle.trim();
        anyhow::ensure!(!needle.is_empty(), "empty query");
        anyhow::ensure!(!needle.contains('\n'), "usages takes one line of text");
        options.validate()?;
        let root = checkout_root(path)
            .with_context(|| format!("{} is not inside a git repository", path.display()))?;
        let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
        let whole_word = word(needle.chars().next()) && word(needle.chars().last());
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&root)
            .args([
                "-c",
                "core.quotePath=false",
                "grep",
                "--untracked",
                "-n",
                "-I",
                "-F",
            ])
            .args(["--no-color", "--full-name"]);
        if whole_word {
            command.arg("-w");
        }
        command.arg("-e").arg(needle).arg("--");
        match options.prefix() {
            Some(prefix) => command.arg(prefix),
            None => command.arg("."),
        };
        command.args([
            ":(exclude,glob)**/*.min.*",
            ":(exclude,glob)**/node_modules/**",
        ]);
        let output = command.output().context("running git grep")?;
        // Exit status 1 means no matches.
        if !output.status.success() && output.status.code() != Some(1) {
            bail!(
                "git grep failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let (total, files) = parse_grep(&String::from_utf8_lossy(&output.stdout), options);
        let repo = root.file_name().map_or_else(
            || root.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Ok(CodeUsages {
            repo,
            root,
            query: needle.to_owned(),
            filters: options.describe(),
            whole_word,
            total,
            files,
            ms: started.elapsed().as_secs_f32() * 1000.0,
        })
    }

    pub fn search_source(&self, source: &Source, query: &str, limit: usize) -> Result<CodeSearch> {
        self.search_with(source, query, &SearchOptions::limit(limit))
    }

    fn search_with(
        &self,
        source: &Source,
        query: &str,
        options: &SearchOptions,
    ) -> Result<CodeSearch> {
        let started = Instant::now();
        let name = repo_name(source);
        if source.last_ingested_at.is_none() {
            let running = matches!(source.status, SourceStatus::Queued | SourceStatus::Running);
            if !running {
                self.ingest.start(source.id, false);
            }
            bail!(
                "Accio is still indexing {name} for the first time; search with grep for now and try again in a few minutes"
            );
        }
        let refreshing = self.refresh_if_due(source);
        let limit = options.limit.clamp(1, MAX_LIMIT);
        let filtered = options.prefix().is_some() || options.kind.is_some();
        // Over-fetch so capping documentation (or filtering) still leaves `limit` hits.
        let candidates = if filtered {
            FILTERED_CANDIDATES
        } else {
            limit + 12
        };
        let mut retrieval = self.engine.search_source(source.id, query, candidates)?;
        let kept: Vec<_> = std::mem::take(&mut retrieval.hits)
            .into_iter()
            .filter(|hit| options.matches(relative(hit, &source.path)))
            .collect();
        let max_docs = if options.kind.as_deref() == Some("doc") {
            limit
        } else {
            MAX_DOC_HITS
        };
        retrieval.hits = cap_docs(kept, limit, max_docs, &source.path);
        let context = options.context.min(MAX_CONTEXT);
        let mut terms = content_terms(query);
        if let Some(translation) = &retrieval.translation {
            for term in content_terms(translation) {
                if !terms.contains(&term) {
                    terms.push(term);
                }
            }
        }
        let hits = retrieval
            .hits
            .iter()
            .map(|hit| {
                let mut lines = snippet(hit, &terms);
                if context > 0 {
                    let center = lines.first().and_then(|(number, _)| *number).map(|n| n + 1);
                    if let Some(found) = file_context(&hit.path, hit, center, context) {
                        lines = found;
                    }
                }
                let path = relative(hit, &source.path);
                CodeHit {
                    path: path.display().to_string(),
                    line: hit.line,
                    end_line: hit.end_line,
                    symbol: shorten(&hit.header, SYMBOL_CHARS),
                    kind: kind_of(path),
                    score: hit.score,
                    stale: hit.stale,
                    snippet: lines,
                }
            })
            .collect();
        Ok(CodeSearch {
            repo: name,
            root: source.path.clone(),
            query: query.to_owned(),
            filters: options.describe(),
            translation: retrieval.translation.clone(),
            refreshing,
            hits,
            ms: started.elapsed().as_secs_f32() * 1000.0,
        })
    }
}

impl CodeSearch {
    /// Compact plain text for an agent: one pointer per hit plus a few lines of code.
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        let also = self
            .translation
            .as_ref()
            .map(|translation| format!(" (also searched: \"{translation}\")"))
            .unwrap_or_default();
        let _ = writeln!(
            text,
            "{}: {} matches for \"{}\"{}{also}, {:.0} ms",
            self.repo,
            self.hits.len(),
            self.query,
            self.filters,
            self.ms
        );
        for (index, hit) in self.hits.iter().enumerate() {
            let location = match (hit.line, hit.end_line) {
                (Some(start), Some(end)) if end > start => format!("{}:{start}-{end}", hit.path),
                (Some(start), _) => format!("{}:{start}", hit.path),
                _ => hit.path.clone(),
            };
            let mut notes = Vec::new();
            if hit.kind != "code" {
                notes.push(hit.kind);
            }
            if hit.stale {
                notes.push("changed since indexed");
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!(" [{}]", notes.join(", "))
            };
            let _ = writeln!(
                text,
                "\n{}. {location}{notes}\n   {}",
                index + 1,
                hit.symbol
            );
            for (number, line) in &hit.snippet {
                match number {
                    Some(number) => {
                        let _ = writeln!(text, "   {number:>5}| {line}");
                    }
                    None => {
                        let _ = writeln!(text, "        | {line}");
                    }
                }
            }
        }
        if self.hits.is_empty() {
            text.push_str("\nNo matches. Try other words, drop filters, or use mode \"usages\" for an exact string.\n");
        } else {
            text.push_str(
                "\nRanked matches, not a complete list: for every reference to an identifier use mode \"usages\".\n",
            );
        }
        if self.refreshing {
            text.push_str(
                "(Recent changes are being re-indexed in the background. Hits marked [changed since indexed] come from files edited since indexing; their line numbers may be off.)\n",
            );
        }
        text
    }

    /// Location of a hit for the panel, as `path:line`.
    pub fn location(hit: &Hit, root: &Path) -> String {
        code_location(hit, root)
    }
}

impl CodeUsages {
    /// Every file with its matching lines; past `MAX_USAGE_LINES` only file names.
    pub fn to_text(&self) -> String {
        let mut text = String::new();
        let mut kinds: Vec<String> = Vec::new();
        for kind in KINDS {
            let count = self.files.iter().filter(|file| file.kind == kind).count();
            if count > 0 {
                kinds.push(format!("{kind} {count}"));
            }
        }
        let how = if self.whole_word {
            "whole word"
        } else {
            "exact text"
        };
        let _ = writeln!(
            text,
            "{}: {} lines mention \"{}\"{} in {} files ({}), {:.0} ms",
            self.repo,
            self.total,
            self.query,
            self.filters,
            self.files.len(),
            if kinds.is_empty() {
                "none".into()
            } else {
                kinds.join(", ")
            },
            self.ms
        );
        let _ = writeln!(
            text,
            "Complete list ({how}, case-sensitive) from git grep over the working tree, including tests and uncommitted files."
        );
        if self.files.is_empty() {
            text.push_str("\nNo references.\n");
            return text;
        }
        let mut listed_only = Vec::new();
        for file in &self.files {
            if file.lines.is_empty() {
                listed_only.push(file);
                continue;
            }
            let _ = writeln!(text, "\n{} [{}]", file.path, file.kind);
            for (number, line) in &file.lines {
                let _ = writeln!(text, "   {number:>5}| {line}");
            }
            if file.lines.len() < file.count {
                let _ = writeln!(text, "        | … {} more", file.count - file.lines.len());
            }
        }
        if !listed_only.is_empty() {
            let _ = writeln!(
                text,
                "\nMore files (lines not shown; narrow with path or kind to see them):"
            );
            for file in listed_only {
                let _ = writeln!(text, "{} [{}] {}", file.path, file.kind, file.count);
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(text: &str, line: Option<u32>, end_line: Option<u32>) -> Hit {
        Hit {
            chunk_id: 1,
            path: "/repo/src/A.java".into(),
            header: "a.A › run()".into(),
            text: text.into(),
            page: None,
            line,
            end_line,
            score: 0.5,
            stale: false,
        }
    }

    #[test]
    fn snippet_picks_lines_with_most_terms_and_numbers_verbatim_chunks() {
        let mut lines: Vec<String> = (0..10).map(|i| format!("a{i}")).collect();
        lines[7] = "    orderSource.save();".into();
        let found = snippet(
            &hit(&lines.join("\n"), Some(10), Some(19)),
            &["ordersource".into(), "save".into()],
        );
        assert_eq!(found.len(), SNIPPET_LINES);
        // The earliest window containing the match: lines 4..=7 of the chunk.
        assert_eq!(found[0], (Some(14), "a4".into()));
        assert_eq!(found[3], (Some(17), "    orderSource.save();".into()));
        // An outline (not a verbatim slice) has no line numbers.
        let outline = snippet(&hit("a\nb", Some(10), Some(40)), &[]);
        assert_eq!(outline, [(None, "a".into()), (None, "b".into())]);
    }

    #[test]
    fn documentation_takes_at_most_two_slots() {
        let at = |path: &str| Hit {
            path: path.into(),
            ..hit("x", None, None)
        };
        let hits = vec![
            at("/r/a/README.md"),
            at("/r/b/README.md"),
            at("/r/c/README.md"),
            at("/r/A.java"),
            at("/r/d/README.md"),
            at("/r/B.java"),
            at("/r/C.java"),
        ];
        let kept: Vec<_> = cap_docs(hits, 4, MAX_DOC_HITS, Path::new("/r"))
            .into_iter()
            .map(|hit| hit.path.display().to_string())
            .collect();
        assert_eq!(
            kept,
            ["/r/a/README.md", "/r/b/README.md", "/r/A.java", "/r/B.java"]
        );
    }

    #[test]
    fn text_output_is_compact_pointers() {
        let search = CodeSearch {
            repo: "shop".into(),
            root: "/repo".into(),
            query: "order sources".into(),
            filters: String::new(),
            translation: Some("Bestellquellen".into()),
            refreshing: true,
            hits: vec![CodeHit {
                path: "src/A.java".into(),
                line: Some(10),
                end_line: Some(16),
                symbol: "a.A › run()".into(),
                kind: "code",
                score: 0.5,
                stale: true,
                snippet: vec![(Some(13), "orderSource.save();".into())],
            }],
            ms: 42.0,
        };
        let text = search.to_text();
        assert!(text.starts_with(
            "shop: 1 matches for \"order sources\" (also searched: \"Bestellquellen\"), 42 ms\n"
        ));
        assert!(text.contains("\n1. src/A.java:10-16 [changed since indexed]\n   a.A › run()\n"));
        assert!(text.contains("      13| orderSource.save();\n"));
        assert!(text.contains("Recent changes are being re-indexed"));
        assert!(text.contains("mode \"usages\""));
    }

    #[test]
    fn kinds_come_from_the_path() {
        assert_eq!(kind_of(Path::new("app/src/main/java/A.java")), "code");
        assert_eq!(kind_of(Path::new("app/src/test/java/ATest.java")), "test");
        assert_eq!(
            kind_of(Path::new("web/src/guards/noInlineStyles.spec.ts")),
            "test"
        );
        assert_eq!(kind_of(Path::new("web/__tests__/x.ts")), "test");
        assert_eq!(kind_of(Path::new("README.md")), "doc");
        assert_eq!(kind_of(Path::new("db/V1__init.sql")), "sql");
        assert_eq!(
            kind_of(Path::new("src/main/resources/application.yml")),
            "config"
        );
        // "contest" is not a test folder.
        assert_eq!(kind_of(Path::new("contest/A.java")), "code");
    }

    #[test]
    fn options_filter_by_prefix_and_kind() {
        let options = SearchOptions {
            path: Some("./app/frontend/".into()),
            kind: Some("code".into()),
            ..SearchOptions::limit(8)
        };
        assert!(options.matches(Path::new("app/frontend/src/A.vue")));
        assert!(!options.matches(Path::new("app/frontend2/A.vue")));
        assert!(!options.matches(Path::new("app/frontend/README.md")));
        assert!(!options.matches(Path::new("design/mockups/A.vue")));
        assert_eq!(options.describe(), " (in app/frontend, kind code)");
        assert!(
            SearchOptions {
                kind: Some("tests".into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            SearchOptions {
                path: Some(".".into()),
                ..Default::default()
            }
            .prefix()
            .is_none()
        );
    }

    #[test]
    fn usages_list_code_before_tests_and_cap_lines() {
        let mut output = String::from(
            "web/a.spec.ts:3:  mount(DataGrid)\n\
             web/List.vue:12:import DataGrid from './DataGrid.vue'\n\
             web/List.vue:40:  <DataGrid :rows=\"rows\" />\n\
             README.md:5:DataGrid: shared list\n",
        );
        for number in 1..=MAX_USAGE_LINES {
            output.push_str(&format!("web/z/Big.vue:{number}:DataGrid\n"));
        }
        let (total, files) = parse_grep(&output, &SearchOptions::default());
        assert_eq!(total, MAX_USAGE_LINES + 4);
        let order: Vec<_> = files
            .iter()
            .map(|file| (file.path.as_str(), file.kind))
            .collect();
        assert_eq!(
            order,
            [
                ("web/List.vue", "code"),
                ("web/z/Big.vue", "code"),
                ("web/a.spec.ts", "test"),
                ("README.md", "doc"),
            ]
        );
        assert_eq!(
            files[0].lines[1],
            (40, "<DataGrid :rows=\"rows\" />".into())
        );
        assert_eq!(files[1].lines.len(), MAX_USAGE_LINES - 2);
        assert!(files[2].lines.is_empty());

        let usages = CodeUsages {
            repo: "shop".into(),
            root: "/repo".into(),
            query: "DataGrid".into(),
            filters: String::new(),
            whole_word: true,
            total,
            files,
            ms: 9.0,
        };
        let text = usages.to_text();
        assert!(text.starts_with(&format!(
            "shop: {} lines mention \"DataGrid\" in 4 files (code 2, test 1, doc 1), 9 ms\n",
            MAX_USAGE_LINES + 4
        )));
        assert!(text.contains("\nweb/List.vue [code]\n      12| import DataGrid"));
        assert!(text.contains("        | … 2 more\n"));
        assert!(text.contains("web/a.spec.ts [test] 1\n"));

        let tests_only = SearchOptions {
            kind: Some("test".into()),
            ..Default::default()
        };
        let (total, files) = parse_grep(&output, &tests_only);
        assert_eq!((total, files.len()), (1, 1));
    }

    #[test]
    fn context_reads_the_current_file_around_the_hit() {
        let dir = std::env::temp_dir().join(format!("accio-context-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("A.java");
        let body: Vec<String> = (1..=100).map(|n| format!("line {n}")).collect();
        std::fs::write(&path, body.join("\n")).unwrap();
        // A short chunk is shown whole.
        let short = file_context(&path, &hit("", Some(10), Some(14)), None, 20).unwrap();
        assert_eq!(short.first().unwrap(), &(Some(10), "line 10".into()));
        assert_eq!(short.len(), 5);
        // A long chunk: a window around the best line, never above the chunk.
        let long = file_context(&path, &hit("", Some(20), Some(90)), Some(60), 12).unwrap();
        assert_eq!(long.first().unwrap().0, Some(56));
        assert_eq!(long.len(), 12);
        let outline = file_context(&path, &hit("", Some(20), Some(90)), None, 5).unwrap();
        assert_eq!(outline.first().unwrap().0, Some(20));
        // Near the end of the file the window is cut off.
        let end = file_context(&path, &hit("", Some(95), Some(140)), None, 30).unwrap();
        assert_eq!(end.last().unwrap(), &(Some(100), "line 100".into()));
        assert!(file_context(&dir.join("missing"), &hit("", Some(1), None), None, 5).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn containing_prefers_the_deepest_source() {
        let source = |id, path: &str| Source {
            kind: SourceKind::Code,
            enabled: true,
            captions: None,
            is_file: false,
            size: 0,
            id,
            path: path.into(),
            ignore_globs: vec![],
            pdf_mode: Default::default(),
            last_ingested_at: None,
            status: SourceStatus::Idle,
            file_count: 0,
            failed_count: 0,
        };
        let sources = [source(1, "/code"), source(2, "/code/shop")];
        assert_eq!(
            containing(&sources, Path::new("/code/shop/app"))
                .unwrap()
                .id,
            2
        );
        assert_eq!(
            containing(&sources, Path::new("/code/other")).unwrap().id,
            1
        );
        assert!(containing(&sources, Path::new("/elsewhere")).is_none());
        // Path::starts_with compares whole components.
        assert!(containing(&sources[1..], Path::new("/code/shop2")).is_none());
    }
}
