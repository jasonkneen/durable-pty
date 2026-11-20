# durable-pty (`dpty`)

One binary that is a **PTY session daemon**, a **CLI**, and an **MCP server**. Every session runs in its own small helper process, so
neither an app crash nor a daemon restart takes a terminal with it. Everything (sessions, scrollback, typed commands, events)
is stored in SQLite, and agent sessions (claude, codex) are **resumed** after a reboot.

```
dpty new -n work -C ~/proj -- claude "fix the bug"   # starts the daemon if needed, attaches (Ctrl-\ detaches)
dpty ls                                              # all sessions, running / exited / lost
dpty browse                                          # interactive session browser (also: dpty, dpty ui)
dpty attach work        dpty screen work        dpty logs work        dpty history work
dpty send work "npm test"        dpty send work -k ctrl-c        dpty wait work "passed" --timeout 60
dpty restart work       dpty kill work -s TERM  dpty rm work    dpty events
dpty stop               # stop the daemon; sessions keep running (dpty stop --sessions ends them)
dpty install            # run at login, restart on exit (launchd / systemd --user)
dpty mcp                # MCP over stdio;  dpty mcp --http 127.0.0.1:7979  for streamable HTTP + SSE
```

In the session browser the left side lists sessions and the right side shows the selected session's details and a live preview of its screen (it is a copy: nothing is attached or resized). Arrow keys or `j`/`k` select. Enter opens the session in the whole window under a one-line dpty header; click the header or press `Ctrl-]` to come back to the list. `/` searches names, titles and directories as you type, and after a short pause also every session's output, typed commands and claude/codex transcripts, showing the line that matched. For an exited or lost session, Enter shows its last screen and `r` restarts it.

| Key | Does |
|---|---|
| Enter | open in the whole window under the dpty header (running) or show the last screen (ended) |
| Tab | use the side panel as a terminal on the selected session |
| `Ctrl-]` | back to the list from a session (or click the header / outside the panel) |
| `/` | search; Esc clears |
| `n` | new session (type a command, empty for a shell) |
| `l` | output so far |
| `s` | stop, after confirmation; history is kept |
| `r` | restart an ended session |
| `R` | rename |
| `d` | delete the session and its history, after confirmation |
| `i` | Nerd Font icons on/off (remembered) |
| `q` | quit |

The side panel is a working terminal once it has the keyboard (Tab, or click the preview): typing, paste and the mouse go to the session, which is resized to the panel. Shift-PageUp/PageDown or the wheel scroll back. Hyperlinks a program prints are real links in your terminal, and images (kitty graphics protocol, on terminals that draw them: Ghostty, kitty) show in place and scroll with the text. Selecting text needs your terminal's override while the browser is open; `dpty attach <id>` from a shell gives a session the real terminal, with native selection and scrollback. `Ctrl-\` also leaves, unless the browser is itself running inside a dpty session, where the outer attach takes that key.

A session has one size. It takes the size of whichever client attached or resized last, and when that client leaves the size goes back to the one before it, so using the side panel does not leave a session small for the terminal that was already showing it.

Mouse: drag the line between panes to resize them (remembered in `browser.json`), click a session to select it, wheel to move through the list. While the browser is open, selecting text needs your terminal's override, usually Shift-drag (Option-drag in macOS Terminal).

## How durability works

Each session has a **shim**: a tiny process that owns the terminal and the program running in it. The daemon connects to it, and
when the daemon goes away the shim just keeps running, buffering output. The next daemon reconnects, replays what it missed (up
to 8 MB, in order, with no repeats) and carries on. `dpty stop` leaves sessions running; `dpty stop --sessions` ends them too.

| Event | What happens |
|---|---|
| Your app / browser crashes | Nothing. Reattach by id, name or label: you get the current screen, then live output. |
| Daemon crashes, restarts or is upgraded | The programs keep running with the same pids. The new daemon reattaches and catches up. |
| Reboot / logout (the programs are gone) | Sessions and history survive. On the next start, agent sessions are relaunched with their resume command and shells come back in the directory they were last in, with scrollback replayed. |
| Agent sessions | claude gets `--session-id <uuid>` at launch (so the id is known immediately); codex ids are found by scanning `~/.codex/sessions`. After a reboot: `claude --resume <id>`, `codex resume <id>`, original prompt dropped, other flags kept. If a resume fails straight away, a fresh conversation is started instead of looping. Other agents: `--kind` / `--resume-cmd "grok --resume {id}"`. A hook can report ids: `dpty tag-agent "$DPTY_SESSION" <id>`. |
| Session titles | When a program sets the terminal title (claude and codex do this with the thread name), it is stored as the session's `title` and shown in `dpty ls` / `pty_list`, with a `title` event on each change. `name` stays as you set it. |
| Hibernate / recall | `dpty hibernate <id>` saves the session (screen, scrollback, directory, environment, and for a shell the command it is running) and stops its program; it shows as `hibernated` and stays that way through daemon restarts and reboots. `dpty recall <id>` brings it back: a shell reopens in its directory, replays its scrollback and re-runs that command; an agent resumes its conversation. MCP: `pty_hibernate`, `pty_recall`. |
| Crash loop | At most 5 automatic restarts per 60 s, then it gives up and records `restart-loop`. |

Not possible: keeping a program alive through a reboot or a killed shim. History, directory, the command a shell was running (re-run when it comes back) and agent resume are what survive. A program's memory is not kept: macOS has no way to image a live process (Linux has CRIU; dpty does not use it yet).

## Surfaces

* **Unix socket** `~/.durable-pty/dpty.sock` (0600): JSON lines RPC; `attach` upgrades the connection to binary frames.
* **HTTP** (default `127.0.0.1:7878`, bearer token from `dpty token`, `--no-http` to disable):
  `POST /rpc`, `GET /ws?id=&cols=&rows=&token=` (binary = terminal bytes, text = JSON control `{"type":"resize",…}`),
  `POST /mcp` (streamable HTTP), `GET /sse` + `POST /messages` (legacy SSE), `GET /health`.
* **MCP tools**: `pty_list pty_new pty_send pty_screen pty_logs pty_wait pty_history pty_search pty_restart pty_hibernate pty_recall pty_kill`.

A command run inside a session cannot attach to, type into, resize, stop, restart or delete that same session, and the browser marks it "you are here". This applies to the command line, the browser and `dpty mcp`; requests over HTTP/WebSocket cannot be traced to a session and are not covered.

## Data

`~/.durable-pty/` (override with `DPTY_HOME`): `dpty.db` (sessions, 16 MB output per session, commands, events), `token`, `dpty.log`. All 0600/0700.
Output stored by an older version is indexed once, in the background, the first time the new daemon starts. Output is also kept as plain text lines for search (`dpty` browser `/`, RPC `search`, MCP `pty_search`), covering the same span as the stored output.
The stored environment is never returned by `list`/`get`/`pty_list`; they give the variable names only.
The daemon tidies up by itself: a running shell that is idle, unattached and has nothing running in it is removed after 24 h (`DPTY_IDLE_SHELL_SECS`), and a shell or process session is removed 7 days after it ended (`DPTY_EXITED_KEEP_SECS`). Set either to `0` in the daemon's environment to turn it off. Agent sessions and anything labelled `keep` are never removed.
Typed commands are not recorded while the terminal has echo off (passwords) or a full-screen app is up.
Session environment (PATH, HOME, locale, agent API variables, plus `-e` values) is stored so recovery can relaunch; `--env-all` passes everything.

## Develop

`cargo test` · `./scripts/e2e.sh` (crash/resume/MCP/HTTP) · `python3 scripts/attach-test.py` (real attach/detach)

## Install

```
brew install jasonkneen/tap/dpty                                            # Homebrew (macOS, Linux)
curl -fsSL https://raw.githubusercontent.com/jasonkneen/durable-pty/main/install.sh | sh   # prebuilt binary, checksum verified
cargo install --path .                                                      # from a checkout
```

The curl installer puts `dpty` in `~/.local/bin` (`DPTY_INSTALL_DIR` changes it, `DPTY_VERSION=v0.1.0` pins a release) and refuses
to install if the download's sha256 does not match. Then run `dpty install` to start the daemon at login (launchd on macOS,
`systemd --user` on Linux). Uninstall: `dpty uninstall`, then remove the binary (`install.sh --uninstall`, `brew uninstall dpty`
or `cargo uninstall durable-pty`). Your data stays in `~/.durable-pty` until you delete it.

### Releasing (maintainers)

`scripts/release.sh X.Y.Z` bumps the version, rolls `CHANGELOG.md`, tests, tags and pushes; the tag builds the binaries, publishes
them and updates the Homebrew tap. See [RELEASING.md](RELEASING.md).

## License and contributing

MIT, see [LICENSE](LICENSE). Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

