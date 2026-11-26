# accio — Design Spec

**Date:** 2026-11-25
**Goal:** Lowest-latency RAG chatbot for macOS menu bar. Ask about local notes/documents or general questions. Experiment; only speed and accuracy matter. No auth, no safety layers.

## 1. User-facing behavior

- Menu-bar (tray) app, no Dock icon. Global hotkey **⌥Space** toggles a frameless, always-on-top panel (centered, upper third of screen). `Esc` or focus loss hides it.
- Panel: one input line, streaming answer below, citations list, timing footer (`retrieval 8ms · ttft 212ms · 940 tok/s`).
- Conversation is a visible thread, kept in memory only (never persisted). The last 6 turns go to the LLM; a follow-up also carries up to 4 note chunks from the previous answer (max 10 context chunks). Cleared by "New chat" (`⌘N`) or 10 minutes hidden.
- Tray menu:
  - `Ask…  ⌥Space`
  - `Sources` → one submenu per source: status line (`✓ 2d ago · 1,612 files` / `⟳ 412/583` / `✗ folder not found`), `Re-ingest`, `Force rebuild`, `Failed files (N)` → submenu listing up to 20 `filename — error` (click = reveal in Finder), `Remove`. While running: `Cancel` instead of Re-ingest/Force rebuild.
  - `Add folder…` → native folder picker → PDF extraction prompt (Plain text / Markdown) → ingestion starts immediately.
  - Per source: `PDF extraction` submenu; switching re-parses only that source's PDFs.
  - `Re-ingest all`
  - `Quit`
- **Ingestion never happens automatically**, except right after `Add folder…`. No file watcher.
- Answers cite notes as `[n]`; clicking a citation opens the file (`.md` → `obsidian://open?path=<abs>`; others → default app). A citation whose file's mtime is newer than its ingest time gets a `changed since ingest` badge.
- When retrieval finds nothing relevant, the model answers from general knowledge and starts its answer with `(not in notes)`.

## 2. Supported files

| Ext | Parser | Chunk header |
|---|---|---|
| `.md` | Obsidian-aware markdown chunker | `Title › H1 › H2 › H3 [tags: a, b]` |
| `.pdf` | Per-source mode: plain `get_text` (default, ~7 ms/page) or pymupdf4llm markdown (~125 ms/page). Page with <20 chars of text → render 150 dpi → Apple Vision OCR | `file.pdf › p.N` |
| `.docx` | MarkItDown → markdown chunker | `file.docx › H1 › …` |
| `.pptx` | MarkItDown, split per slide | `file.pptx › Slide N: <slide title>` |
| `.xlsx` | MarkItDown, per sheet, tables chunked by 30 rows with header row repeated | `file.xlsx › <sheet>` |
| `.doc` | `textutil -convert txt -stdout` → chunker | `file.doc` |
| `.ppt` | `soffice --headless --convert-to pptx` → pptx path; if `soffice` missing → file fails with `LibreOffice not installed (brew install --cask libreoffice)` | as pptx |
| `.png .jpg .jpeg .heic .webp` | Apple Vision OCR. Optional VLM caption (`ACCIO_CAPTION=1`, `mlx-community/Qwen2.5-VL-3B-Instruct-4bit`, ~11–27 s/image, input downscaled to ≤1024 px, loaded from local cache only; `accio-ingest fetch-models` downloads it). Skip (0 chunks, not failed) if file <20 KB or min side <256 px. | `file.png (image)`; text = `Caption: …\n\nText in image: …` |

Markdown rules (Obsidian):
- YAML frontmatter stripped; `tags` (list or string) go into the header.
- `[[target|alias]]` → `alias`; `[[target#heading]]` / `[[target]]` → `target`; `![[embed]]` removed.
- Sections split at H1–H3; deeper headings stay inside the chunk.
- Fenced code blocks (``` / ~~~) and `$$…$$` blocks are atomic: never split.
- ` ```mermaid ` blocks are replaced by `[mermaid diagram] <labels joined by "; ">` (labels = text in `[]`, `()`, `{}`, `""`, and edge labels `|x|`).
- Chunk size: max 1800 chars (split on blank lines; an atomic block larger than max becomes its own chunk); chunks <200 chars merge with the next chunk of the same section.

## 3. Architecture

```
┌─────────────── Tauri v2 app (one Rust process, resident) ───────────────┐
│  Tray + menu · ⌥Space panel (webview: TS, markdown-it, KaTeX, mermaid,  │
│  highlight.js)                                                           │
│  accio-core (Rust lib):                                                 │
│    Store (SQLite WAL, FTS5)  VecIndex (in-RAM binary codes + f16)        │
│    Embedder + Reranker (fastembed-rs, ONNX, CPU)                         │
│    Engine: prefetch/retrieve  ·  IngestManager  ·  Groq streaming client │
└───────────────┬──────────────────────────────────────────────────────────┘
                │ JSON lines over stdin/stdout, only while a job runs
┌───────────────▼───────────────┐
│ accio-ingest serve (Python 3.12) │  stateless parser: path → chunks
│ pymupdf4llm, MarkItDown,      │  no DB access
│ pyobjc Vision OCR, mlx-vlm    │
└───────────────────────────────┘
```

- **Rust owns the database and all embedding.** Python only parses files into chunks. One writer, one embedding implementation.
- **Ingestion job** (per source): scan (`ignore` crate, honors `.gitignore` + per-source globs, no symlink following) → diff against `files` table → stream changed paths to Python worker → embed returned chunks → per-file transaction replaces that file's chunks → at job end rebuild `VecIndex` and swap atomically. Queries keep using the old index during a job.
- **Diff rules:** known file with equal mtime+size and status `ok` → skip. mtime/size differ → blake3 hash; equal hash → update mtime/size only; else re-parse. Status `failed` → always retry. Unknown → parse. Known but no longer on disk → delete. `force` → re-parse all. Source root missing → job fails with `folder not found: <path>`, nothing deleted.
- **Overlapping sources:** `files.path` is globally unique; a file already owned by another source is skipped.
- **Jobs:** one at a time globally, FIFO queue; starting an already queued/running source is a no-op. Cancel kills the worker; files already committed stay; `last_ingested_at` not updated.
- **Per-file timeout:** 300 s (covers VLM model load and cloud-storage placeholder downloads). On timeout/worker crash → kill and respawn worker, file marked failed with `timeout after 300s` / `worker crashed`.

## 4. Retrieval

> **Revised 2026-11-26** after measurement on the M4: fastembed/onnxruntime on CPU took ~90 ms per query embedding, ~5 chunks/s for documents, 30–40 s per rerank of 20 chunks, and peaked at ~20 GB RSS (CoreML EP was slower). Embedding moved to a resident MLX helper on the GPU; the cross-encoder reranker was removed.

- Embedding model: EmbeddingGemma 300M (`mlx-community/embeddinggemma-300m-bf16`) served by `accio-ingest embed-server` (resident helper, JSON lines, base64 f32): ~10 ms/query, ~13 chunks/s, ~1.6 GB. Output truncated to **512 dims** (Matryoshka) and re-normalized. Query text `task: search result | query: {q}`; document text `title: {header} | text: {text}`. Fallback if unavailable in fastembed: `MultilingualE5Base` (768 dims, no truncation, prefixes `query: ` / `passage: `).
- `meta` table stores `embed_model` and `dim`; on mismatch at startup all `files` and `chunks` are cleared (forces re-ingest).
- Pipeline for query `q`:
  0. Translation (revised 2026-11-26): `q` is detected as German or English (function words, umlauts) and translated into the other language by opus-mt (CTranslate2, int8, CPU, inside the embedding helper; ~50–100 ms). Both wordings are searched. `ACCIO_TRANSLATE=0` disables it; while the models load, only `q` is searched.
  1. Dense: embed both wordings in one call → `VecIndex.search(top 50)` per wording: Hamming distance on sign bits → top 200 candidates → exact f16 dot product rescore → top 50.
  2. Sparse: FTS5 `bm25` top 50 per wording (query sanitized: alphanumeric tokens, each double-quoted, joined with `OR`; empty → skip).
  3. If the best dense cosine < **0.30** (`ACCIO_MIN_SIMILARITY`, Settings → Search overrides it), dense and sparse lists are dropped. Only notes containing the question's content words (no function words, ≥ 3 characters, matched as word prefixes; all of them for ≤ 2 words, all but one otherwise) are kept. None → general-knowledge mode. Measured: questions about the notes 0.35–0.66, general questions ≤ 0.27.
  4. Duplicates: chunks with identical text (copies of a file in several folders) are merged to the first one met before fusion.
  5. Fuse: RRF, k = 60, over all lists. Top **8** chunks go to the LLM (no reranker).
- **Retrieve-while-typing:** UI calls `prefetch` 150 ms after the last keystroke. Engine caches the latest `(query → Retrieval)`; `ask` with an identical retrieval query uses the cache. Older in-flight prefetches are discarded (generation counter).
- Follow-up questions: retrieval query = previous user question + `\n` + current question.

## 5. LLM

- Groq OpenAI-compatible endpoint `https://api.groq.com/openai/v1/chat/completions`, `stream: true`.
- Model: `GROQ_MODEL`, default `openai/gpt-oss-20b`. Params: `reasoning_effort: "low"`, `include_reasoning: false`, `max_completion_tokens: 2048`.
- One shared `reqwest` client (HTTP/2, rustls, keep-alive). Panel open → `GET /openai/v1/models` to warm the connection.
- System prompt with notes:
  > You are a fast assistant. Answer concisely in the language of the question. NOTES below are excerpts from the user's own files. If any note is relevant, base the answer on the notes and cite every note you use with its number in square brackets, e.g. [1] or [2][5], right after the statement it supports. Only if no note is relevant, answer from general knowledge and start the answer with "(not in notes)". Never write "(not in notes)" in an answer that uses or cites a note.
- The panel shows sources whenever an answer cites an existing note, even if the model also wrote the prefix.
- System prompt without notes:
  > You are a fast assistant. Answer concisely in the language of the question. No relevant notes were found: answer from general knowledge and start the answer with "(not in notes)".
- Notes block appended to the system message: `NOTES:\n[1] {header} ({path}[, p.{page}])\n{text}\n\n[2] …`
- History: last 3 user/assistant turns before the current question.
- Errors (missing key, HTTP 4xx/5xx, network drop mid-stream) are shown in the panel as text; missing key message: `GROQ_API_KEY missing — add it to .env`.

## 6. Config & data

- `ACCIO_HOME` (default `~/Library/Application Support/accio/`): `index.sqlite`, `models/` (fastembed cache), optional `.env`.
- `.env` load order: repo root `.env` (dev), then `$ACCIO_HOME/.env` (overrides). Keys: `GROQ_API_KEY`, `GROQ_MODEL`, `ACCIO_RERANK_THRESHOLD`, `ACCIO_CAPTION`, `ACCIO_INGEST_CMD`.
- `ACCIO_INGEST_CMD` default: `uv run --project <repo>/ingest accio-ingest serve`.

## 7. Eval

- `eval/questions.jsonl`: `{"q": "...", "expect": "<substring of expected file path>"}`; `expect: null` = general question (should return zero chunks). User writes ~50.
- `cargo run -p accio-core --release --bin eval` prints recall@5 for `dense`, `fts`, `hybrid`, `hybrid+rerank` and p50/p95 ms per stage, plus false-positive rate on `expect: null` questions. Used to pick τ and validate models.

## 8. Latency targets (M4, 32 GB)

| Stage | Target |
|---|---|
| hotkey → panel visible | <16 ms |
| embed + dense + FTS + RRF | <15 ms (hidden by prefetch) |
| Groq TTFT | ~150–300 ms |
| tokens | ~1000 tok/s (gpt-oss-20b) |

- Ingestion pipelines parsing (Python worker, CPU/ANE) with embedding (GPU): the parser runs up to two files ahead.

## 9. Out of scope

Auth, multi-user, file watching, agentic tool loops, query rewriting/HyDE, multimodal embeddings, Docling, Keychain.
