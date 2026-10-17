# Contributing to durable-pty

Thanks for helping. This is a small Rust crate (one binary, `dpty`), so the bar for a change is: it works, it is tested, and it stays simple.

## Set up

You need a Rust toolchain (stable) on macOS or Linux. Windows is not supported.

```
git clone <repo> && cd durable-pty
cargo build
cargo test
./scripts/e2e.sh                     # crash recovery, agent resume, shell directory, MCP, HTTP
python3 scripts/attach-test.py       # a real `dpty attach` / detach in a pty
```

Run the daemon against a throwaway home so you never touch your real sessions:

```
export DPTY_HOME=$(mktemp -d /tmp/dpty-dev.XXXX)
cargo run -- serve --no-http
```

## Layout

| File | What it does |
|---|---|
| `src/session.rs` | Session manager: spawning, output, restart policy, recovery on boot |
| `src/db.rs` | SQLite storage (sessions, output, typed commands, events) |
| `src/agents.rs` | Agent adapters: choosing and finding session ids, building the resume command |
| `src/rpc.rs` | The one RPC surface the socket, HTTP and MCP all share |
| `src/server.rs`, `src/http.rs` | Unix socket (and binary attach frames), HTTP / WebSocket / SSE |
| `src/mcp.rs` | MCP server over stdio and HTTP |
| `src/install.rs` | launchd / systemd unit |
| `src/main.rs` | CLI |

A new capability should be one RPC method in `rpc.rs`; the CLI, MCP tool and WebSocket then just call it.

## Adding an agent

Edit `src/agents.rs`: teach `detect_kind` the command name, `prepare` how to choose or read its session id, `resume` how to rebuild its arguments, and `scan` where it keeps transcripts. Add a unit test next to the others, and check the flags against the real CLI's `--help`: do not guess them.

## Pull requests

- Keep a change to one thing; do not reformat unrelated code.
- Add a test for the behaviour you changed. Crash and recovery behaviour belongs in `scripts/e2e.sh`.
- `cargo build` must be warning-free and `cargo test` and the e2e script must pass.
- Do not add a limit, cache or background task without saying why in the PR.
- Security-sensitive areas are the unix socket permissions, the HTTP token and what is stored in `~/.durable-pty`. Say so in the PR if you touch them.

## Reporting a bug

Include `dpty status`, the output of `dpty events -n 50`, your OS, and whether the daemon was started by launchd or by a client. Do not paste the contents of `~/.durable-pty/token` or the database.
