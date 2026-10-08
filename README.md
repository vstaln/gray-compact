<p align="center">
  <img src="assets/gray-logo.svg" alt="gray" width="96">
</p>
<h1 align="center">gray-compact</h1>
<p align="center">Auto-compact oversized context into an LLM summary via host/run.</p>
<p align="center">
  <a href="https://github.com/vstaln/gray-compact/blob/main/LICENSE"><img alt="MIT License" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="gray plugin" src="https://img.shields.io/badge/gray-plugin-7aa2f7.svg">
  <img alt="rust" src="https://img.shields.io/badge/built%20with-rust-orange.svg">
</p>

A sidecar plugin for [gray](https://github.com/vstaln/gray), scaffolded by
[gray-account](https://github.com/vstaln/gray-account).

## What it does

On every `context/build` the sidecar serializes the outbound message list to
the `[User]:` / `[Assistant]:` / `[Assistant tool calls]:` / `[Tool result]:`
text form. When the total exceeds `GRAY_COMPACT_CHARS` (default **120000**),
it sends the **oldest 70%** to `host/run` (a `gray -p` turn) with a
summarizer prompt — goals, decisions, code changes, blockers, next steps,
file paths and exact errors verbatim — then replaces the outbound list with:

1. one `user` message: `## Earlier context (auto-compacted)` + the summary
2. the newest 30% of messages, verbatim

The summary is cached per session in `~/.gray/compact/<sid>.json`, keyed by a
hash of the summarized prefix, so the repeated `context/build` calls inside a
single turn reuse it instead of re-summarizing. The transcript on disk is
never touched — only the outbound request shrinks. If `host/run` errors —
capability not granted, timeout, empty reply — the list passes through
unchanged.

## Commands

- `/compact` — arm a compaction pass for the next request, even under the
  threshold, and say what it will do.
- `/compact off` / `on` — disable / re-enable auto-compaction.
- `/compact status` — state, threshold, and last-seen context size.

## Wire methods

- `context/build` — the auto-threshold check and the `{messages}` swap
- `command/run` — `/compact …`
- `plugin/manifest`, `plugin/shutdown`
- `host/run` — summarizer turn (capability `host.turn`; grant with
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

---
Part of the [gray](https://github.com/vstaln/gray) plugin ecosystem —
the open-source AI agent harness. <https://gray.alignment.id>
