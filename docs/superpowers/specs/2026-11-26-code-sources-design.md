# Code sources: ask questions about a codebase

Date: 2026-11-26 · Status: approved; built on branch `code-sources` (experimental)

## Why

Large codebases such as the one this was built for (≈590k lines of production Java, ≈410k lines of Vue/JS, plus
≈2M lines of tests) are slow to explore. Agents (Claude Code, Codex) spend several grep
rounds finding where a concept lives. An earlier attempt with Graphify did not help: on a
codebase this size it became unwieldy, and the agent grepped anyway.

Accio already has a fast local pipeline: EmbeddingGemma on the GPU, SQLite FTS5 plus binary
vectors, German⇄English query translation, and retrieval in tens of milliseconds. This
project tests whether that pipeline, pointed at code, brings fast exploration back.

The real code is the source of truth. Documentation inside the repo is a second-tier source.
The code may go to Groq; it is not sensitive.

## Goals

- Index any git repository as a **code source**: the owner's main monorepo first, any other codebase the same way.
- **Agents:** an MCP tool, `code_search`, that Claude Code and Codex can call. It returns
  compact pointers (repo-relative file:line, symbol, signature, a few lines of code).
- **The owner:** ⌥Space questions about a repo, answered by Groq from code excerpts, with
  clickable file:line sources that open in an editor.
- **Measure the benefit:** a repeatable experiment comparing Claude Code with and without
  the tool on real questions about that repository.

## Non-goals

- A call graph or relationship map (callers, implementations as a query). That is
  approach 2, deferred until the experiment shows a need.
- Multi-step agentic answering inside Accio. v1 answers in a single retrieval step.
- Indexing test code by default.
- Languages beyond those listed under Chunking.

## Anti-goals: failure even if it works

- **Agents still don't use it.** Graphify's failure mode. Tool output must be small, and one
  call must beat grep for concept questions.
- **Agent sessions multiply memory.** Each Claude Code session starts its own MCP server; a
  server that loads its own 1.2 GB embedding helper is not acceptable.
- **Code pollutes note answers.** Code chunks must never show up in normal ⌥Space note
  questions unless the user chose that.
- **Stale enough to mislead.** Pointers to code that has since moved should be rare and
  short-lived.

## Approaches considered

1. **Code-aware chunks plus the existing hybrid search. Chosen.** Code is cut at
   class/method boundaries by a parser. Each chunk carries a header with qualified name,
   signature, annotations and extends/implements. Search is the existing FTS5 + vectors +
   RRF path.
   - Concept questions are found by meaning. Structural keywords (`implements
     CommunicationProvider`, `@KafkaListener`) are found by BM25 because they sit in the
     header.
   - Cheapest path, reuses everything.
2. **Approach 1 plus a relationship graph** (callers, implementations, Spring wiring as
   queries). About twice the work and close to what Graphify did; grep already approximates
   "implementations of X". Deferred until the experiment says otherwise.
3. **Off-the-shelf** (Serena via the Java language server, claude-context, Sourcegraph).
   No build work, but it does not use Accio's pipeline. Serena runs jdtls, which is slow on
   9k files; claude-context needs cloud embeddings. Usable as a comparison point in the
   experiment, not as the solution.

The owner picked **1**.

## Design

### Code sources

- `sources` gets a `kind` column: `notes` (existing behaviour, default) or `code`.
- Tray menu: **Add code repository…** (folder picker) next to *Add folder…*. The folder must
  be inside a git repository; the source path is the repo root (`git rev-parse
  --show-toplevel`).
- **Default ignore globs for code sources:** the existing scan already honours `.gitignore`
  (the test repository: `node_modules`, `venv`). It adds:
  - test trees: `**/src/test/**`, `**/src/integrationTest/**`, `**/__tests__/**`,
    `**/*.spec.*`, `**/*.test.*`;
  - build output: `**/build/**`, `**/dist/**`, `**/target/**`;
  - minified files: `**/*.min.*`.

  The owner can edit these per source, the same as for note sources.
- **Code sources never take part in note retrieval.** The panel's normal ⌥Space search
  ignores them, whatever the enabled and profile state.
- **Asking about a repo in the panel:** the owner picks a code source as the scope (see
  Panel). While a code source is the scope, only that source is searched.

### Supported files and chunking

Parsing stays in the Python parser worker (`dispatch.py` by extension); chunking uses
tree-sitter (`tree-sitter` plus language grammars from PyPI). Chunks carry a start and end
line.

- **Java:**
  - **One outline chunk per type** (class, interface, enum, record). It holds the package,
    annotations, declaration line with extends/implements, fields, enum constants, and the
    signatures of all methods (no bodies). This is what answers "which X exist", "who
    defines Y" and "what implements Z".
  - **One chunk per method or constructor**, with its Javadoc and annotations. Trivial
    accessors (getters/setters/one-line delegations) get no chunk of their own; their
    signatures are already in the outline.
  - **Header:** `<package>.<Type> › <method signature>`, plus the type's annotations and
    extends/implements. Example:
    `com.example.orders.internal.OrderIntakeService › accept(PartnerOrder order): Order · @Service · implements OrderIntake`.
  - **Long bodies** are split at statement boundaries near the existing ~1,800-character
    chunk size, repeating the header.
- **Vue SFC:**
  - The `<script>` block is chunked like JS. The component name is derived from the file
    name and goes in the header.
  - One chunk holds props/emits and the component's imports, which answers "where is X used".
  - The `<template>` block is one chunk, truncated to the chunk size.
- **JS/TS:** one chunk per top-level function, class, or exported const; a module outline
  chunk lists the exports.
- **SQL** (Flyway migrations and others): one chunk per statement group, header = file name
  plus created/altered table names. Answers "where is the order-status table defined".
- **Config:** `application*.yml`, `application*.properties`, `*.yaml` under resources, one
  chunk per top-level key. Spring wiring of providers often lives here.
- **Markdown inside the repo:** chunked as today and marked `kind: doc` in results. It is
  second tier: no re-weighting in v1, but every hit says whether it is `code` or `doc`.
- **Other files** in a code source are skipped.

Expected size for the test repository's production code: ≈26k chunks. The embedding helper was measured at
≈15 code chunks/s (1,500-character chunks, batch 16), so the **first index takes ≈30
minutes**. Outline/method chunking with skipped accessors should land somewhat lower; the
builder records the real number.

### Freshness

- Code changes constantly, so code sources refresh themselves. Notes keep today's
  manual-only rule.
- **Trigger:** on every `code_search` call and every scoped panel question, if the source's
  last scan was more than 60 s ago, start an incremental re-index in the background.
- **Mechanics:** same mtime/size/hash diff as today. The query is answered immediately from
  the current index and never waits for the refresh.
- **Concurrency:** one refresh per source at a time. A refresh never runs while that source
  is being ingested.

### Search service in the app

- The running Accio app owns the embedding helper and the index. It listens on a Unix socket
  at `$ACCIO_HOME/accio.sock`: user-only permissions, JSON lines, one request per line.
- **Request:** `{"op": "code_search", "repo": "<absolute path anywhere inside a repo>",
  "query": "...", "limit": 8}`.
- **Repo resolution:**
  - Find the code source whose root contains `repo`.
  - Failing that, resolve `repo` to its main checkout via `git rev-parse
    --git-common-dir`, which makes worktrees such as `repo/.claude/worktrees/x` map to the
    main checkout's source.
  - No match: error `"no code source for <path>; add it in Accio → Add code repository…"`.
- **Retrieval:** the existing engine pipeline (translation, dense + BM25 per wording, dedupe,
  RRF), restricted to that one source. **No similarity gate**: return the best `limit` hits
  with scores and let the agent judge.
- **Response per hit:**
  - repo-relative path, start–end line, symbol (qualified name), kind (`code`/`doc`);
  - header, score;
  - up to 5 lines of the chunk around the strongest match (or the first 5 lines).

  Total response target: ≤ 2,500 characters for 8 hits.
- Paths are repo-relative, so they also work inside a worktree. Line numbers may be off by
  the worktree's local changes, which is acceptable; the agent reads the file anyway.

### MCP server for agents

- **A new binary `accio-mcp`** in the workspace: stdio MCP server, no model, no index. It
  forwards to the app socket and starts in milliseconds; any number of agent sessions cost
  nothing extra.
- **One tool:** `code_search(query: string, limit?: int = 8)`. The repo is the server's
  working directory (Claude Code and Codex start MCP servers in the project directory).
- **Tool description** (this decides whether agents use it; the builder tunes it during the
  experiment): use it *first* for concept and "where/which/who" questions about this
  codebase, before grep. It finds code by meaning and by symbol, in German or English.
  Follow up by reading the returned file:line.
- **App not running:** the tool returns the error text `"Accio is not running; start
  /Applications/Accio.app"`. It does not hang.
- **Setup:** a README section with the `claude mcp add` / Codex `config.toml` lines, and a
  suggested line for a repo's `CLAUDE.md`/`AGENTS.md` pointing agents at the tool.

### Panel (⌥Space) on a code source

- **Scope picker:** the panel's source scope (beside the existing profile selection) lists
  code sources. Choosing one scopes the conversation to that repo; ⌘N or switching back
  returns to notes.
- **Answer prompt:**
  - Excerpts are code with path:line.
  - The model answers from the code, names classes and methods, and cites `[n]`.
  - It says when the excerpts don't cover the question.
  - The "(not in notes)" fallback text becomes "(not found in <repo>)".
- **Citations** show `path:line` and open in the configured editor:
  - Setting **Code editor**, default Zed (`zed <path>:<line>`).
  - IntelliJ IDEA option: `idea --line <line> <path>`.
  - Markdown inside a code source also opens in the editor, not in Obsidian.

### Experiment: does it add value?

- **Question set:** `eval/code/questions.jsonl` (private, gitignored), 12–15 real questions with a reference answer
  and the key files. It covers:
  - the owner's three: an intake pipeline end to end, who defines a core enum, which
    provider integrations are implemented;
  - concept questions in German and English;
  - one or two questions where grep should win, as a control.

  The builder drafts questions and references by reading the code; **the owner confirms the
  references** before runs.
- **Retrieval check:** extend the existing `eval` binary to report recall@8 of key files
  per question for a code source.
- **Agent A/B:**
  - **Runs:** a script runs `claude -p` on each question twice from the repository root, once
    with `accio-mcp` configured and once without, same model, three repeats.
  - **Records:** wall time, number of tool calls (grep/read/code_search), input and output
    tokens, and the final answer.
  - **Grading:** answers are graded against the reference by a separate model call, with
    a 1–3 rubric, plus spot checks by the owner.
- **Report:** `docs/experiments/2026-11-code-search.md` with the table and a verdict.
- **Success** means meaningfully faster or fewer tool calls at equal or better answer
  quality on concept questions. If agents ignore the tool or quality drops, the report says
  so, and approach 2 or dropping the feature are the options.

## Constraints

- macOS on Apple Silicon, same machine and models as today; no new resident processes
  besides the socket listener inside the app.
- **Memory:** a second binary-quantized index for ≈26k chunks is a few MB. The helper is
  shared.
- **Existing note behaviour is unchanged:** gate, translation, profiles, manual re-ingest.
- **New dependencies:**
  - Python: `tree-sitter` and grammar wheels (`tree-sitter-java`,
    `tree-sitter-javascript`, `tree-sitter-typescript`); grammar licenses listed in NOTICE.
  - Rust: an MCP stdio implementation (a small hand-rolled JSON-RPC loop is acceptable;
    the tool surface is one tool).
- **Groq free tier (8k tokens/min):** code excerpts are denser than notes. The panel keeps
  8 excerpts; the builder checks token cost per question and reports it.

## Left to the builder

- Schema details: how start/end lines are stored (new columns vs reuse of `page`), and the
  migration.
- Exact tree-sitter queries, the accessor-detection rule, and the Vue `<script setup>`
  handling.
- MCP library vs hand-rolled JSON-RPC.
- Socket protocol framing details and error shapes beyond those named above.
- Panel UI layout of the scope picker, within the existing settings/profile style.
- Which 12–15 questions, subject to the owner's confirmation.

## Decisions made while building

The owner asked for an autonomous build on a throw-away branch. Calls made along the way:

- **Repo Markdown is mostly excluded.** the test repository's tracked Markdown (≈62 MB, mostly
  `docs/superpowers` plans and specs) is larger than its production code and describes
  code as planned, not as shipped. Code sources index only orientation files
  (README*, CONTEXT*, ARCHITECTURE*, AGENTS*, CLAUDE*) and skip files over 512 KB.
- **Panel scope uses the existing `@name` prefix** (like profiles): `@myrepo …` switches the
  panel to the repository; `@notes` or ⌘N switches back. The tray's Code menu has
  *Ask about this repository*.
- **Editor setting lives in `.env`** (`ACCIO_CODE_EDITOR`, default `zed`), not in the
  Settings window.
- **Background refreshes share the one ingest queue.** A refresh may wait behind a long
  notes job; hits whose file changed since indexing are marked "changed since indexed".
  A refresh starts at most once a minute per repository and not right after an index.
- **Embedding batches span files.** Code files are small; chunks of up to 64 parsed files
  are embedded together, sorted by length, 16 per model call (measured best on the M4).
  Notes benefit too.
- **Identifier words in headers.** Headers carry the symbol split into words
  ("OrderStatusType" → "order status type"), so keyword search matches plain-language
  questions.
- **Experiment arms:** baseline (no tool), tool, and tool plus a one-line system-prompt hint
  (the test repository's CLAUDE.md already steers agents toward grep). All arms use
  `--strict-mcp-config` and read-only tools. Model and budget are the owner's call before
  any run.
