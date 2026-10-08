# gray-compact

Auto-compact oversized context into an LLM summary via `host/run`. Port of
pi's `custom-compaction` + `trigger-compact` extensions.

A sidecar plugin for [gray](https://github.com/vstaln/gray), scaffolded by
[gray-account](https://github.com/vstaln/gray-account).

## What it does

On every `context/build` the sidecar serializes the outbound message list to
pi's `[User]:` / `[Assistant]:` / `[Assistant tool calls]:` / `[Tool result]:`
text form. When the total exceeds `GRAY_COMPACT_CHARS` (default **120000**),
it sends the **oldest 70%** to `host/run` (a `gray -p` turn) with pi's
summarizer prompt — goals, decisions, code changes, blockers, next steps,
file paths and exact errors verbatim — then replaces the outbound list with:

1. one `user` message: `## Earlier context (auto-compacted)` + the summary
2. the newest 30% of messages, verbatim

The summary is cached per session in `~/.gray/compact/<sid>.json`, keyed by a
hash of the summarized prefix, so the repeated `context/build` calls inside a
single turn reuse it instead of re-summarizing. The transcript on disk is
never touched — only the outbound request shrinks (the `trigger-compact.ts`
auto-threshold half of the port). If `host/run` errors — capability not
granted, timeout, empty reply — the list passes through unchanged.

## Commands

- `/compact` — arm a compaction pass for the next request, even under the
  threshold, and say what it will do (the `/trigger-compact` manual half).
- `/compact off` / `on` — disable / re-enable auto-compaction.
- `/compact status` — state, threshold, and last-seen context size.

## Wire methods

- `context/build` — the auto-threshold check and the `{messages}` swap
- `command/run` — `/compact …`
- `plugin/manifest`, `plugin/shutdown`
- `host/run` — summarizer turn (capability `host.turn`, needs consent:
  `gray plugin capabilities compact --all`)

## Install

```sh
gray plugin install compact
gray plugin capabilities compact --all   # grants host.turn
```

## Develop

```sh
cargo test
cargo build --release
gray account check      # entry point + manifest handshake
gray account publish    # check → build → release → publish to the gray registry
```

Bump `version` in `Cargo.toml` before each `publish`; the registry refuses to
republish a version.
