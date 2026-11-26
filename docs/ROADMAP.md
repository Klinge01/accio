# accio Roadmap

Follow-up features to build later. Each entry records the goal, how feasible it is
given the current architecture (see `docs/superpowers/specs/2026-11-25-accio-design.md`),
a proposed approach, and open questions. Add new entries at the end of their section.

Status values: `idea` → `planned` → `in progress` → `done`.

---

## Requested features

### 1. Single-file ingestion (drag onto the tray icon)

**Status:** done · **Requested:** 2026-11-26

Files and folders can be dropped on the tray icon, its drop pane, or the chat panel. Files stay at their original paths. Dropped PDFs start in plain-text mode. If an indexed folder already owns a file, Accio reports that when you try to add the file separately.

**Goal:** Ingest individual files, not only folders. Dragging a file over the tray icon
shows a drop pane; dropping the file there ingests it.

**Feasibility:** Feasible, with native macOS code for the tray part.
- Tauri's tray API exposes clicks only, not drag events. The status item's button
  (`NSStatusBarButton`) is an `NSView`, so it can be registered as a drag destination
  (`registerForDraggedTypes:` for file URLs) via `objc2`, with a small `NSView` subclass
  implementing `NSDraggingDestination` added over the button.
- Dropping files on a Tauri window is built in (`onDragDropEvent`), so the drop pane itself
  is easy.

**Approach (staged, each stage useful on its own):**
1. **Drop on the ⌥Space panel:** handle Tauri's drag-drop event in the panel; show
   "Drop to ingest" while hovering. No native code needed.
2. **"Add file…" menu item:** native file picker (multi-select).
3. **Tray drag target:** on `draggingEntered` over the status item, show a small HUD panel
   anchored under the icon ("Drop to ingest"); accept the drop on the icon itself as well,
   in `performDragOperation`. Hide on `draggingExited` or after the drop.

**Data model:** sources are folders today (`scan(root)` walks a directory). Options:
- *(preferred)* A source may be a single file: `scan` returns that one file when the root
  is a file. Files appear in the tray under a "Files" group, with the same per-source
  actions (Re-ingest, Remove, PDF mode for PDFs).
- Alternative: one built-in "Loose files" source holding a list of paths.

Files are indexed **in place** (not copied). If the file moves or is deleted, re-ingest
reports it like a missing folder and deletes nothing.

**Open questions:**
- Dropping a folder: treat it as "Add folder" (probably yes).
- Unsupported extensions: show the reason in the drop pane instead of failing silently.
- PDF mode prompt per dropped PDF, or use a default (plain text) and switch later?

---

### 2. Enable / disable sources without removing them

**Status:** done · **Requested:** 2026-11-26

Use **Use in answers** in the tray or Settings to change scope. Profiles switch a set of sources together. Accio starts fresh conversation context after each scope change.

**Goal:** Temporarily exclude sources from answers, e.g. turn off university notes while
asking coding questions against an ingested docs folder, without losing the index.

**Feasibility:** Very feasible and cheap. **No re-embedding needed:** every chunk is
embedded independently and already belongs to a file and a source
(`chunks → files.source_id`), so disabling a source just filters it out of retrieval.

**Approach:**
- `sources.enabled INTEGER NOT NULL DEFAULT 1` (migrated like `pdf_mode`).
- **Dense search:** build the in-memory `VecIndex` from enabled sources only (join chunks →
  files → sources in `Store::snapshot`). A toggle triggers `Engine::reload_index()`; rebuilding
  ~100k vectors takes well under a second, and query cost stays the same.
- **Keyword search:** restrict `fts_search` with the same join (`WHERE s.enabled = 1`).
- **Follow-ups:** `hits_by_ids` drops carried chunks from disabled sources.
- **Prefetch cache:** cleared by `reload_index` already.
- **Ingestion:** still allowed for disabled sources; they stay hidden until enabled.
- **Tray:** a checkmark item "Use in answers" per source, and disabled sources dimmed in
  the Sources list. The panel footer shows the active scope (e.g. "3 of 5 sources").

**Extension: profiles.** Named sets of enabled sources ("Uni", "Coding"), switched from the
tray or with a prefix in the panel (e.g. `@coding how do I …`). Store as
`profiles(name)` + `profile_sources(profile, source_id)`; switching a profile is a
batch toggle plus one index rebuild.

**Open questions:**
- Should the "not in notes" similarity gate apply to the enabled subset only? (Yes, by
  construction, since the index only contains enabled sources.)
- Per-question scoping in the panel (`@source`) vs global toggles only.

---

### 3. Settings window

**Status:** done · **Requested:** 2026-11-26

Open **Settings…** from the tray or chat panel. New API keys are tested before Accio stores them in Keychain. The existing Groq environment key remains a fallback. Local MLX uses an MLX server on loopback port 8080. Privacy shows the last question’s prompt. Sensitive-source routing is tracked in feature 5.

**Goal:** One place to configure accio: which model answers, which sources exist and are
used, profiles, and the few tuning knobs worth exposing. Opened from the tray ("Settings…",
⌘,) and from the panel.

**Design principles**
- Intuitive, user friendly and simple. One window, a short sidebar, plain labels.
- No chips, no eyebrows, no decorative badges. Status is plain text next to the thing it
  describes ("Indexed 2 h ago · 445 files").
- Changes apply immediately; there is no Save button. API keys are the exception: they're checked
  first ("Test" → shows time to first token or the error).
- Defaults that work. Every setting has one sentence of explanation at most, and only where
  the label is not enough.
- Native look: system font, light/dark, standard macOS controls (Tauri window, same
  styling as the panel).

**Sections**

1. **Model:** who writes the answers.
   - Providers:
     - *Groq API* (default, fastest).
     - *Anthropic API key* and *OpenAI API key* (bring your own key; the right
       choice when others use the app).
     - *Claude Code on this Mac* (uses your Claude subscription via `claude -p`).
     - *Codex on this Mac* (uses your ChatGPT subscription via `codex exec`).
     - *Local model* (MLX, nothing leaves the Mac; see 5).
   - Model picker per provider, populated from the provider's model list.
   - Keys stored in the macOS Keychain (replaces `GROQ_API_KEY` in `.env`).
   - Subscription CLIs: mark them "slower: starts a CLI per question (~1–3 s), for personal
     use on your own Mac". Notes from the docs (2026-11-26):
     - `claude -p` with a subscription cannot use `--bare` (bare mode never reads OAuth
       credentials); use `--safe-mode --tools "" --model haiku --system-prompt-file …
       --no-session-persistence --output-format stream-json --verbose --include-partial-messages`.
     - Anthropic does not allow third-party apps to offer Claude.ai login or route requests
       through Pro/Max/Ultra credentials on behalf of their users; each user must use their own
       key or their own subscription. Distribution to others = bring-your-own API key.
     - Pro includes no API credits; Max, Ultra and Team include monthly API credits after
       linking a Claude Console organization.
     - ChatGPT/Codex: check OpenAI's terms before relying on it.
   - Implementation: an `AnswerProvider` trait in core (stream text, cancel, report time to first token);
     Groq, OpenAI-compatible and Anthropic Messages over HTTP, CLI providers as subprocesses
     parsing their stream-JSON output.
2. **Sources:** a file explorer.
   - Tree of folders with checkboxes; checking a folder adds it as a source, unchecking a
     subfolder excludes it (this replaces hand-written ignore globs).
   - Per source: status, file count, size, last ingest, failed files, Re-ingest / Force
     rebuild, PDF extraction mode, image captions on/off, "Use in answers" (roadmap 2),
     "Sensitive" (roadmap 5).
   - Single files can be added here too (roadmap 1).
3. **Profiles:** named sets of enabled sources ("Uni", "Coding"); create, rename, pick
   sources, set the default. Switch from the tray, the settings window, or `@profile` in
   the panel (roadmap 2).
4. **Search:** "not in notes" sensitivity (the similarity threshold, as a plain
   *Stricter ↔ Looser* slider), number of excerpts sent (default 8), conversation memory
   (default 6 turns), retrieve-while-typing on/off.
5. **Shortcut & panel:** global hotkey (default ⌥Space), panel size, reset-after-hidden
   time (default 10 min), launch at login.
6. **Privacy:** per-source "Sensitive" handling (roadmap 5), inumaki status, what is sent
   to the provider for the last question (inspect the exact prompt).
7. **Data:** index location and size, embedding helper status and memory, clear index,
   reveal logs.

**Open questions:**
- Settings in a separate window, or a second view inside the panel? (Separate window is
  more conventional on macOS and leaves the panel for asking.)
- Where settings live: SQLite `settings` table (single source of truth with sources and
  profiles) plus Keychain for secrets.

---

### 4. Talk with your sources (speech in, speech out)

**Status:** idea · **Requested:** 2026-11-26

**Goal:** Ask by voice and hear the answer, keeping the panel's low latency.

**Approach (local by default):**
- **Speech to text:** on-device, so questions do not leave the Mac.
  - Apple's on-device speech recognition (fast, built in, German and English).
  - Or Whisper on the GPU via MLX (e.g. whisper-large-v3-turbo), which handles mixed German/English better.
  - Groq's hosted Whisper is faster but sends audio to the cloud. Offer it only as an option.
- **Push to talk:** hold ⌥Space (or a mic button in the panel). Partial transcripts feed
  retrieve-while-typing, so retrieval is done when you stop talking.
- **Text to speech:** speak the answer sentence by sentence as tokens stream in.
  - `AVSpeechSynthesizer` (instant, built in).
  - Or a natural local voice via `mlx-audio` (already a dependency of the ingest environment), e.g. Kokoro.
  - Skip code blocks, tables, formulas and citation markers when speaking; say "see the
    panel for the table".
- **Conversation mode:** hands-free loop with voice activity detection; speaking interrupts
  playback.

**Open questions:** voice per language (German answers need a German voice), and whether
the spoken answer should be a shorter summary than the written one.

---

### 5. Sensitive sources via [inumaki](https://github.com/Klinge01/inumaki)

**Status:** idea · **Requested:** 2026-11-26

**Goal:** Chat with sensitive sources (e.g. a vault of confidential notes, customer data) without
sending identifying information to the cloud model.

**What inumaki offers (from its README and docs/configuration.md, 2026-11-26):**
- `inumakid` runs a local model-API proxy on `127.0.0.1:8787`.
- It replaces identifiers with tokens (`<PERSON_a7f2>`), verifies the result with an independent second pass, forwards
  the request, and restores the real values in the answer locally, streaming included.
- Routes: `/openai` (`/v1/chat/completions`, `/v1/responses`), `/anthropic`
  (`/v1/messages`), `/codex`.

**Approach:**
- Per-source **Sensitive** flag (settings → Sources). When a question's context contains
  any chunk from a sensitive source, the request goes through inumaki's proxy instead of
  directly to the provider.
- **Fail closed:** if inumaki is not running or its check fails, sensitive excerpts are not
  sent. The panel says so and offers to answer without them, or with the local model.
- **Groq gap:** inumaki's proxy upstreams are `anthropic`, `openai` and `openai-codex` only.
  Options:
  - add a Groq upstream to inumaki (Groq speaks the OpenAI Chat Completions format, so a small change there);
  - or route sensitive questions to OpenAI/Anthropic through the proxy (slower, needs those keys).
- Chunk headers include file paths (with the user name); inumaki's path detectors should
  cover them. Verify with inumaki's canary check.
- Settings → Privacy shows inumaki's state, profile, and a "Run check" button.

**Honest limit:** inumaki *reduces* what the provider sees; it is not a guarantee. Its own
README states that identifiers the detectors miss are sent as is (PERSON recall ~86–94%
on its synthetic tests). For sources that must never leave the Mac, offer a **local-only**
mode:
- answer with a local model via MLX (e.g. a ~8B 4-bit model on the M4: slower, but zero egress);
- per-source choice: *Cloud*, *Cloud via inumaki*, or *Local only*.

This also covers the earlier idea of a per-source "never send to the cloud" flag.

---

## Other candidates noted during the build

Smaller items found while building and measuring; not requested yet.

- **Faster document embedding:** length-sorted batches of ~8 instead of 32 (padding waste on
  real ~500-token chunks; the benchmark showed 17 vs 13 chunks/s). Real ingest of a
  445-file test folder ran at ~6 chunks/s.
- **Eval set:** write ~50 questions in `eval/questions.jsonl` and calibrate
  `ACCIO_MIN_SIMILARITY` (default 0.3) with `cargo run -p accio-core --release --bin eval`.
- **Excalidraw drawings:** `*.excalidraw.md` embeds compressed drawing data; index only its
  "Text Elements" section, or ignore these files by default.
- **Legacy `.ppt`:** needs LibreOffice (`brew install --cask libreoffice`); otherwise listed
  under "Failed files".
- **Image captions per source:** captions are global opt-in (`ACCIO_CAPTION=1`, ~11–27 s
  per image); a per-source toggle like the PDF mode would allow them for diagram-heavy folders.
