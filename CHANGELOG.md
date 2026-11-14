# Changelog

All notable changes to Durable PTY (`dpty`). The format follows [Keep a Changelog](https://keepachangelog.com/), and
versions follow [Semantic Versioning](https://semver.org/). Notes under "Unreleased" are moved under a version heading by
`scripts/release.sh`.

## Unreleased

## 0.2.1 (2026-11-14)

- Fix: the release workflow asked for a `macos-14` arm64 runner, whose label GitHub no longer serves ("The configuration 'macos-14-arm64-us-default' is not supported"), so that build was cancelled, `publish` was skipped and v0.2.0 produced no binaries. The release workflow and CI now use `macos-15` for arm64 macOS. `0.2.1` is the first release with downloads, and it contains everything listed under 0.2.0.

## 0.2.0 (2026-11-14)

- Session browser: the right-hand side now shows a live preview of the selected session's screen, in colour, without attaching to it or resizing it. `/` search also looks inside every session's output, typed commands and claude/codex transcripts, including output stored before this version (indexed once in the background), and shows the matching line. New keys: `n` start a session, `R` rename, `d` delete (asks first). The output view scrolls correctly, stops at the end and follows new output; a session that has ended opens on its last screen. The list no longer reorders itself as sessions produce output.
- Session browser: the side panel is now a working terminal. `Tab` (or a click on the preview) gives it the keyboard: typing, paste and the mouse go to the selected session, its cursor is shown, and `Ctrl-\` (or a click elsewhere) gives the keyboard back to the list. The session is resized to the panel, as a full attach resizes it to the window. Shift-PageUp/PageDown or the wheel scroll back through its history. `Enter` opens it in the whole window.
- Session browser: mouse support. Drag the line between the list and the right-hand side, or between details and the preview, to resize them; the sizes are remembered. Click a session to select it; the wheel moves through the list and scrolls the output view. Selecting text with the mouse now needs the terminal's own override (usually Shift-drag, Option-drag on macOS).
- Session browser: Enter now opens a session in the whole window under a one-line dpty header (session name, size, how to get back) instead of handing the terminal over. Click the header or press `Ctrl-]` to return to the list. `Ctrl-]` is the leave key everywhere in the browser: when the browser itself runs inside a dpty session, the outer `dpty attach` takes `Ctrl-\` and detaches the whole window. `Ctrl-\` still works when nothing outside takes it. `dpty attach <id>` from a shell is unchanged and hands the real terminal to a session (native text selection and scrollback).
- Session browser: hyperlinks and images work in the side panel and the whole-window view. Hyperlinks a program prints (OSC 8) are written to your terminal as real links. Images sent with the kitty graphics protocol are passed to your terminal and anchored to the text, so they scroll, clip and clear with it; they are freed when the view closes. Images need a terminal that draws them and supports the protocol's Unicode placeholders (Ghostty, kitty); the browser asks, and elsewhere image commands are dropped so the program falls back to text. Opening a session also replays its recent raw output into the view, so links and images printed before you opened it come back where the replay matches the screen.
- Protocol: an `attach` request with `"history": "raw"` gets a frame of type 2 (recent raw output) before the screen.
- Session browser: new look. Rounded panels with the active one highlighted, the selected session shown as a highlighted row, key hints in the footer, paths written with `~` and cut from the left so the end stays visible. A session's state is one coloured symbol instead of a word (green dot running, hollow ended, red cross failed, `!` lost), which frees a column of the list. `i` switches Nerd Font icons on or off (remembered; `DPTY_ICONS=nerd|plain` decides for one run). The details pane is 4 lines instead of 8.
- Fix: a client that attached to a session at a smaller size (the browser's side panel, a second terminal) left the session at that size when it went away, so the terminal already showing it kept a program painting for the wrong size. A session now takes the size of whichever client attached or resized last, and when that client leaves the size goes back to the client before it, or to what it was before anyone attached.
- A session can no longer act on itself. A command run inside a session (the browser included) is refused when it tries to attach to, type into, resize, stop, restart or delete that same session; `dpty stop --sessions` run inside a session leaves that one running. The browser marks the session it is running in ("you are here") and does not preview it. `list`/`get` mark the caller's own session with `here`. Requests over HTTP/WebSocket are not covered: the daemon cannot tell where they come from.
- New `search` and `snapshot` RPC methods and a `pty_search` MCP tool.
- Fix: a daemon that stalled for 10 s (machine under load, paused in a debugger) lost its connection to a session and treated the still-running program as dead, starting a second copy under `on-failure`/`always`. The daemon now reconnects and carries on; a session recorded as lost whose program is still running is picked up again on the next start.
- Fix: when an agent conversation could not be resumed, the fallback to a fresh conversation only worked within 20 s of the session being created, and then retried the resume anyway. After a reboot it looped 5 times and gave up. It now starts a fresh conversation.
- Fix: `list`, `get` and `pty_list` returned each session's environment, API keys included. They now return the variable names only (`env_keys`).
- Fix: detaching left the terminal in whatever modes the program had set (alternate screen, mouse reporting, bracketed paste). Detach now puts them back, and the detach key works in terminals using an extended keyboard mode.
- Fix: `dpty kill` and `dpty restart` without a signal now follow HUP with TERM and then KILL if the program is still there after 3 s each, so a program that ignores HUP no longer leaves a restart pending.
- Fix: attaching a smaller terminal to a shell cut off the bottom of its screen.
- Attaching to a shell prints its recent scrollback first (`dpty attach --no-history` to skip).
- Removing idle or exited shell sessions automatically is now off unless `DPTY_IDLE_SHELL_SECS` / `DPTY_EXITED_KEEP_SECS` are set, because it deletes their history.
- Commands that print (`ls`, `logs`, `screen`, `history`, `events`) end quietly when piped into something that stops reading, instead of panicking.
- Only one daemon can run per home (lock file); an empty session id no longer matches the only session; session lookups no longer read every session; output pruning only visits sessions that wrote.
- Sessions now track the terminal title the program sets (OSC 0/2), so a claude or codex thread name shows up live as `title` in `dpty ls`, `pty_list` and the RPC/HTTP API, and a `title` event is recorded each time it changes. Leading spinner/status glyphs are ignored, the title is saved across restarts, and `name` is left alone.

## 0.1.2 (2026-10-17)

- Fix: `scripts/release.sh` now really bumps the version in `Cargo.toml` and refuses to continue if the built binary does not report it. The 0.1.1 binaries were built from an unbumped tree and report 0.1.0; 0.1.2 is the first release whose version matches its tag.
## 0.1.1 (2026-10-17)

- Homebrew: the formula installs the prebuilt binary (no Rust needed); the release workflow updates the tap with a deploy key.
- A session reattached after a daemon restart no longer gets a banner written into its screen (a full-screen program would have shown it inside its layout); the event log records the reattach.
- `scripts/real-agent.sh` drives the real claude, codex and grok CLIs through a daemon crash and a simulated reboot.

## 0.1.0 (2026-10-17)

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
