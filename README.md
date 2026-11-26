# Accio

**Summon answers from your own notes.**

Accio is a macOS menu-bar assistant for your notes and documents. Press ⌥Space, ask a
question, and it searches your Obsidian vaults, PDFs, Office files and images on your Mac,
then streams an answer from a fast cloud model with clickable sources. Questions your
notes can't answer get a general answer, marked `(not in notes)`.

It is built for one thing: the shortest possible time from question to first word.

```text
⌥Space → "Welche Kündigungsfrist steht im Mietvertrag?"
         retrieval 16 ms · first token 355 ms · ~1000 tok/s
         → answer with [7] linking to the note it came from
```

## How fast

Measured on a MacBook with an M4 and 32 GB, on a real index of 19,129 chunks
(445 files: Markdown, PDFs, Office files, images):

| Step | Time |
|---|---|
| Embed the question (on the GPU) | 9–14 ms |
| Vector + keyword search, fusion | < 10 ms |
| Retrieval while you type | usually done before you press Return |
| First token from Groq (`openai/gpt-oss-20b`) | ~260–600 ms |
| Memory (app + embedding helper) | ~1.2 GB |

## How it works

```text
your folders ──► accio-ingest (Python, runs only during ingestion)
                   Markdown (Obsidian-aware), PDF (text or Markdown mode, OCR for scans),
                   docx/pptx/xlsx, doc/ppt, images (Apple Vision OCR)
                 ──► chunks ──► embedding helper (EmbeddingGemma on the GPU via MLX)
                 ──► SQLite (FTS5 keyword index + vectors)

question ──► + German/English translation (local, CPU) ──► embed both (GPU)
         ──► binary-quantized vector search + BM25 per wording ──► fusion, copies merged ──► top 8
         ──► similarity gate ("not in notes", exact key-term matches pass) ──► Groq, streamed
         ──► panel with sources
```

- **One resident Rust process** (Tauri) owns the index, search and the Groq client.
- **One resident Python helper** serves embeddings from the Apple GPU (MLX).
- A **parser worker** runs only while a folder is being ingested.
- **Retrieval while typing:** the search runs 150 ms after your last keystroke, so it is
  usually finished when you press Return.
- **German ⇄ English:** each question is also searched in the other language, translated
  locally by opus-mt (~50–100 ms on the CPU), so English questions find German notes and
  the other way round.
- **Copies count once:** the same file in several folders yields one result, not three.
- **Incremental ingestion:** re-ingest only parses files whose content changed. Nothing is
  ingested automatically; you add folders and re-ingest from the menu bar.
- **Conversations** stay in memory only. Follow-up questions keep the notes from the
  previous answer in view. ⌘N or 10 minutes hidden clears them.

Design and decisions: [docs/superpowers/specs/2026-11-25-accio-design.md](docs/superpowers/specs/2026-11-25-accio-design.md).

## What leaves your Mac

| Where | What | When |
|---|---|---|
| Groq | your question, the conversation, and up to 8 note excerpts | every question |
| Hugging Face | nothing of yours; model downloads only (embedding, translation) | first use |

Parsing, OCR, embeddings, search and the index stay on your Mac
(`~/Library/Application Support/accio/`). If some sources must never reach a cloud model,
see the roadmap item on sensitive sources.

## Requirements

- macOS 13+ on Apple Silicon (MLX and Apple Vision are required)
- [uv](https://docs.astral.sh/uv/) (`brew install uv`): Accio runs its local models with it
- A [Groq](https://console.groq.com) API key, for written answers (code search for agents
  works without one)
- Optional: LibreOffice for legacy `.ppt` files (`brew install --cask libreoffice`)

## Install

1. Download `Accio_<version>_aarch64.dmg` from
   [Releases](https://github.com/Klinge01/accio/releases) and drag Accio to Applications.
2. The app is not notarized. Clear the download quarantine once:

   ```bash
   xattr -cr /Applications/Accio.app
   ```

3. Open Accio. Paste your Groq key under Settings → Model (it is stored in the Keychain).

The first start sets up Accio's Python environment and downloads the models (1–2 GB in
total, a few minutes), so the menu-bar icon takes a while to appear. Later starts take a
few seconds. The environment lives in `~/Library/Application Support/accio/python`.

### Build from source

Also needs Rust (stable), Node.js 20+ and the Tauri CLI
(`cargo install tauri-cli --version "^2" --locked`).

```bash
git clone https://github.com/Klinge01/accio.git
cd accio
scripts/package-macos.sh --no-dmg  # without --no-dmg it also builds the DMG
rm -rf /Applications/Accio.app && cp -R dist/Accio.app /Applications/
open /Applications/Accio.app
```

Pushing a tag `v<version>` builds the DMG on GitHub Actions and attaches it to a release.
For development, `cargo tauri dev` in `app/src-tauri` runs the Python helpers from the
checkout and reads a `.env` there (see `.env.example`).

## Use

- **Add a folder:** menu bar icon → Add folder… → choose how its PDFs are extracted:
  - *Plain text* (fast, ~7 ms/page);
  - *Markdown* (keeps tables, ~40× slower).

  Progress shows in the menu.
- **Ask:** ⌥Space, type, Return. Click `[n]` or a source to open it (Markdown opens in Obsidian).
- **Follow up:** just keep asking; ⌘N starts over. Esc hides the panel.
- **Re-ingest:** per source in the Sources menu. Only changed files are processed.
  *Force rebuild* re-processes everything. *Failed files* lists what could not be parsed.

## Code repositories (for you and for coding agents)

Accio can also index a git repository's code, so that you and coding agents can ask how
it works ("which payment providers are implemented?", "wo wird der
Warenkorb gespeichert?") and get file:line answers in a fraction of a second.

- **Add:** menu bar → *Add code repository…*, pick any folder inside the repository.
  - Indexes Java, JS/TS, Vue, Python, SQL, Spring config, and README/CONTEXT/AGENTS/CLAUDE files.
  - Skips tests, build output, dependencies, generated files, and files over 512 KB.
  - Adjust what's included under Settings → Sources → Subfolders.
- **How it is cut:** at code structure, not every N characters.
  - Java: one outline per class (annotations, extends/implements, fields, enum constants,
    method signatures), plus one piece per non-trivial method.
  - Vue, JS and Python: by declaration.
  - SQL: by statement, naming the tables.
- **Never mixed into notes:** code repositories don't appear in normal questions.
- **Ask in the panel:** start with `@<repo name>` (for example `@shop how is a refund
  processed?`). `@notes` or ⌘N goes back. Sources open at their line in your
  editor (`ACCIO_CODE_EDITOR`).
- **Coding agents:** register the MCP server that ships inside the app, once:

  ```bash
  claude mcp add accio --scope user -- /Applications/Accio.app/Contents/MacOS/accio-mcp
  # Codex (~/.codex/config.toml):
  # [mcp_servers.accio]
  # command = "/Applications/Accio.app/Contents/MacOS/accio-mcp"
  ```

  The agent gets one tool, `code_search`, for the repository it runs in (worktrees map
  to their main checkout):
  - *search* (default): ranked matches by meaning, with file:line and code lines;
    `context=N` shows N current lines per hit.
  - *usages*: every line mentioning an exact identifier or string, tests included,
    complete and unranked (for refactors and "who uses X").
  - `path` (folder prefix) and `kind` (code, sql, config, test, doc) narrow both. `accio-mcp` holds no model: it asks the running
  app over `~/Library/Application Support/accio/accio.sock`, so any number of agent
  sessions share one embedding helper. To make agents reach for it first, add a line
  like this to the repository's CLAUDE.md or AGENTS.md: *"For questions about how the
  code works or where something lives, call `code_search` first, then read the files it
  points to."*
- **Stays current by itself:** a search starts a background re-index of changed files
  when the last check is over a minute old. Hits from files changed since indexing are
  marked.
- **Measure it:** `eval/code/` has a question set, a retrieval check (`recall.py`) and an
  A/B harness that runs Claude Code with and without the tool (`ab.py`).

## Configuration

`.env` in the repository root (and optionally `~/Library/Application Support/accio/.env`,
which overrides it):

| Key | Default | Meaning |
|---|---|---|
| `GROQ_API_KEY` | none | Required for answers |
| `GROQ_MODEL` | `openai/gpt-oss-20b` | Any Groq chat model |
| `ACCIO_MIN_SIMILARITY` | `0.30` | Below this best-match similarity, a question counts as "not in notes" unless notes contain its key terms. Settings → Search overrides it |
| `ACCIO_TRANSLATE` | `1` | `0` turns off searching the German/English translation of each question |
| `ACCIO_CODE_EDITOR` | `zed` | Opens code citations at their line: `zed`, `idea`, `cursor` or `code` |
| `ACCIO_CAPTION` | `0` | `1` adds local image captions (~11–27 s per image; see NOTICE for the model license) |
| `ACCIO_HOME` | `~/Library/Application Support/accio` | Index and model cache |

On Groq's free tier (8,000 tokens per minute) a question with notes costs about 2,400 tokens,
so expect roughly three such questions per minute.

## Development

```bash
(cd ingest && uv run pytest -q)                     # parsers, OCR, embed helper protocol
cargo test -p accio-core --features testutil        # store, search, ingestion, prompt, Groq client
(cd app/ui && npx vitest run && npx tsc --noEmit)   # panel rendering and chat state
```

Useful tools, all against your real index:

```bash
cargo run -p accio-core --release --example ask -- "question" [--no-llm]
cargo run -p accio-core --release --example ingest_source -- <folder> [--force]
cargo run -p accio-core --release --bin eval -- eval/questions.jsonl   # recall@8 and latency
```

Layout: `crates/core` (Rust engine), `app/src-tauri` (menu bar app), `app/ui` (panel),
`ingest` (Python parsers and embedding helper), `docs` (spec, plan, roadmap).

## Status

Experimental, built for and tested on one Mac. It works end to end there; expect rough
edges anywhere else.

What's next: [docs/ROADMAP.md](docs/ROADMAP.md). It covers:
- a settings window (API keys, or your local Claude Code or Codex instead of Groq);
- turning sources on and off, with profiles;
- dragging single files onto the menu bar icon;
- voice;
- sensitive sources via [inumaki](https://github.com/Klinge01/inumaki).

## The name

*Accio* is the summoning charm from the Harry Potter books: say it, and the thing you want
flies into your hand. This project is not affiliated with or endorsed by the rights
holders. See [NOTICE](NOTICE).

## License

Apache-2.0, see [LICENSE](LICENSE). Model weights are downloaded at first use under their own
licenses, listed in [NOTICE](NOTICE).
