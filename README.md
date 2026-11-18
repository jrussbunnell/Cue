> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Cue

[![Cue tests](https://github.com/jrussbunnell/Cue/actions/workflows/echo_tests.yml/badge.svg?branch=cue-main)](https://github.com/jrussbunnell/Cue/actions/workflows/echo_tests.yml)

Cue is a personal fork of [Zed](https://github.com/zed-industries/zed) built around working with coding agents by voice. It narrates what agents do, takes spoken instructions, and keeps track of every session and subagent running at once.

It is not affiliated with Zed Industries. For the editor itself, use [Zed](https://zed.dev).

---

### What Cue adds

- **Read-aloud narration.** Speaks agent replies, tool calls, and turn wrap-ups as they happen, with a "catch me up" summary.
- **Voice mode.** A real-time conversation with the agent through Inworld, voiced by the persona Echo. It can steer, start, approve, and stop sessions, with a wake word and echo cancellation.
- **Native subagents.** Claude Code subagents run as child sessions with their own threads. You can stop, message, and persist each one without touching its parent.
- **Fleet view.** Shows the parallel sessions and workflow runs Claude Code already has going, in the sidebar.
- **Database panel.** Read-only query and table views for MySQL, Postgres, ClickHouse, and Convex, plus tools that let agents use them.

The design docs live in [`docs/superpowers/specs`](./docs/superpowers/specs).

### Building

Cue builds like Zed. Follow the upstream guides for [macOS](./docs/src/development/macos.md), [Linux](./docs/src/development/linux.md), or [Windows](./docs/src/development/windows.md). Then:

```sh
cargo run              # runs the `zedcue` binary
script/bundle-mac      # builds Cue.app
```

Cue installs alongside Zed. It uses its own binary (`zedcue`), URL scheme, data directories, and bundle id (`dev.jrb.ZedCue`). It doesn't use the bare name `cue`, which would clash with the CUE language CLI.

Voice features need an Inworld API key (`INWORLD_API_KEY`).

### Branches

- **`cue-main`** is the working branch and the default.
- **`main`** mirrors upstream and is not maintained.

`cue-main` is kept current by merging `upstream/main` into it. Upstream's CI workflows only run in `zed-industries`. The fork's own tests run in [`echo_tests.yml`](./.github/workflows/echo_tests.yml).

### Licensing

Like Zed, Cue's source is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked. See the license files in the repository root.

Third-party license compliance is checked with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about). Accepted licenses are listed in `script/licenses/zed-licenses.toml`.
