# Changelog

All notable changes to Durable PTY (`dpty`). The format follows [Keep a Changelog](https://keepachangelog.com/), and
versions follow [Semantic Versioning](https://semver.org/). Notes under "Unreleased" are moved under a version heading by
`scripts/release.sh`.

## Unreleased

## 0.1.0

First release.

- One binary: a PTY session daemon, a CLI and an MCP server (stdio, streamable HTTP, SSE).
- Every session runs behind its own helper process (the shim), so programs keep running when the daemon crashes, restarts or is
  upgraded; the next daemon reattaches and replays what it missed.
- SQLite storage for sessions, scrollback, typed commands and events.
- After a reboot, agent sessions are relaunched with their resume command (`claude --resume`, `codex resume`, `grok --resume`);
  shells come back in the directory they were last in. A resume that fails starts a fresh conversation instead of looping.
- Tested end to end against the real claude and codex CLIs (crash the daemon, reboot, resume).
- `dpty install` runs the daemon at login (launchd on macOS, `systemd --user` on Linux).
- Install with Homebrew, a checksum-verified curl installer, or `cargo install`.
