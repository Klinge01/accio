# Contributing

Issues, ideas and pull requests are welcome. Accio is an experiment in how fast a local
RAG assistant can be, so changes that make it faster, more accurate, or simpler are the
most interesting.

## Setup

See [Build and install](README.md#build-and-install). For development you don't need to
bundle the app:

```bash
(cd ingest && uv sync)
(cd app/ui && npm ci)
cd app/src-tauri && cargo tauri dev
```

## Before you open a pull request

All three suites must pass:

```bash
(cd ingest && uv run pytest -q)
cargo test -p accio-core --features testutil
(cd app/ui && npx vitest run && npx tsc --noEmit)
```

- Write the test first, then the change.
- Tests that need downloaded models are marked (`#[ignore]` in Rust, `@pytest.mark.slow`
  in Python) and are not part of the default run.
- Latency claims come with a measurement: say what you ran and on which machine.

## Privacy rules

- Test data is synthetic. Never commit real notes, documents, paths or names.
- Nothing from a user's files goes into logs or error messages beyond file paths.
- Anything new that sends data off the Mac must be listed in the README's
  "What leaves your Mac" table.

## Bigger changes

For anything larger (a new file type, a new answer provider, a roadmap item), open an issue
first so we can agree on the approach. The design lives in
[docs/superpowers/specs/](docs/superpowers/specs/) and the plans for what's next in
[docs/ROADMAP.md](docs/ROADMAP.md).
