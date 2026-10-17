# durable-pty (`dpty`)

One binary that is a **PTY session daemon**, a **CLI**, and an **MCP server**. Sessions live in the daemon, not in your app,
so an app crash never takes a terminal with it. Everything — sessions, scrollback, typed commands, events — is stored in SQLite,
and agent sessions (claude, codex, …) are **resumed** after the daemon dies.

```
dpty new -n work -C ~/proj -- claude "fix the bug"   # starts the daemon if needed, attaches (Ctrl-\ detaches)
dpty ls                                              # all sessions, running / exited / lost
dpty attach work        dpty screen work        dpty logs work        dpty history work
dpty send work "npm test"        dpty send work -k ctrl-c        dpty wait work "passed" --timeout 60
dpty restart work       dpty kill work -s TERM  dpty rm work    dpty events
dpty install            # run at login, restart on exit (launchd / systemd --user)
dpty mcp                # MCP over stdio;  dpty mcp --http 127.0.0.1:7979  for streamable HTTP + SSE
```

## How durability works

| Event | What happens |
|---|---|
| Your app / browser crashes | Nothing. Reattach by id, name or label. You get the current screen, then live output. |
| Daemon crashes or restarts | On start, anything recorded as running is marked `lost`, then relaunched in the same cwd/env if its `restart` policy allows (default for agents: `on-failure`). Same session id, history kept. |
| Agent sessions | claude gets `--session-id <uuid>` at launch (so the id is known immediately); codex ids are found by scanning `~/.codex/sessions`. On recovery: `claude --resume <id>`, `codex resume <id>`, original prompt dropped, other flags kept. Other agents: `--kind`/`--resume-cmd "grok --resume {id}"`. A hook can report ids: `dpty tag-agent "$DPTY_SESSION" <id>`. |
| Shells | `dpty` tracks each shell's live directory, so a restored shell starts where it was, with its scrollback replayed. |
| Reboot / logout | Processes die; sessions and history survive and are restored on next daemon start. |
| Crash loop | Max 5 automatic restarts per 60s, then it gives up and records `restart-loop`. |

Not possible: keeping a *running process* alive when the daemon itself dies (the daemon owns the PTY master). A per-session
shim process would fix that; the code is structured so it can be added.

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
cargo install --path .        # puts `dpty` on your PATH (~/.cargo/bin)
dpty install                  # optional: start at login, restart if it exits
```

## License and contributing

MIT, see [LICENSE](LICENSE). Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md).

