# durable-pty (`dpty`)

One binary that is a **PTY session daemon**, a **CLI**, and an **MCP server**. Every session runs in its own small helper process, so
neither an app crash nor a daemon restart takes a terminal with it. Everything (sessions, scrollback, typed commands, events)
is stored in SQLite, and agent sessions (claude, codex) are **resumed** after a reboot.

```
dpty new -n work -C ~/proj -- claude "fix the bug"   # starts the daemon if needed, attaches (Ctrl-\ detaches)
dpty ls                                              # all sessions, running / exited / lost
dpty attach work        dpty screen work        dpty logs work        dpty history work
dpty send work "npm test"        dpty send work -k ctrl-c        dpty wait work "passed" --timeout 60
dpty restart work       dpty kill work -s TERM  dpty rm work    dpty events
dpty stop               # stop the daemon; sessions keep running (dpty stop --sessions ends them)
dpty install            # run at login, restart on exit (launchd / systemd --user)
dpty mcp                # MCP over stdio;  dpty mcp --http 127.0.0.1:7979  for streamable HTTP + SSE
```

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
| Crash loop | At most 5 automatic restarts per 60 s, then it gives up and records `restart-loop`. |

Not possible: keeping a program alive through a reboot or a killed shim. History, directory and agent resume are what survive.

## Surfaces

* **Unix socket** `~/.durable-pty/dpty.sock` (0600): JSON lines RPC; `attach` upgrades the connection to binary frames.
* **HTTP** (default `127.0.0.1:7878`, bearer token from `dpty token`, `--no-http` to disable):
  `POST /rpc`, `GET /ws?id=&cols=&rows=&token=` (binary = terminal bytes, text = JSON control `{"type":"resize",…}`),
  `POST /mcp` (streamable HTTP), `GET /sse` + `POST /messages` (legacy SSE), `GET /health`.
* **MCP tools**: `pty_list pty_new pty_send pty_screen pty_logs pty_wait pty_history pty_restart pty_kill`.

## Data

`~/.durable-pty/` (override with `DPTY_HOME`): `dpty.db` (sessions, 16 MB output per session, commands, events), `token`, `dpty.log`. All 0600/0700.
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

