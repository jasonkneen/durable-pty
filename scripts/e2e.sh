#!/usr/bin/env bash
# End-to-end check: sessions, history, crash recovery with agent resume, MCP, HTTP. Uses a throwaway DPTY_HOME.
set -u
BIN="${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/dpty}"
export DPTY_HOME="$(mktemp -d /tmp/dpty-e2e.XXXXXX)"
FAKE="$DPTY_HOME/bin"; mkdir -p "$FAKE"
export PATH="$FAKE:$PATH"
fail=0
ok()  { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

cat > "$FAKE/claude" <<SH
#!/bin/sh
echo "\$@" >> "$DPTY_HOME/claude-args.log"
echo "fake claude up: \$@"
exec sleep 300
SH
chmod +x "$FAKE/claude"

"$BIN" serve --http 127.0.0.1:0 >"$DPTY_HOME/daemon.out" 2>&1 &
DAEMON=$!
for _ in $(seq 50); do [ -S "$DPTY_HOME/dpty.sock" ] && break; sleep 0.1; done
check "daemon socket up" '[ -S "$DPTY_HOME/dpty.sock" ]'
HTTP=$(grep -o 'http://[0-9.:]*' "$DPTY_HOME/daemon.out" | head -1)
TOKEN=$("$BIN" token)

echo "# sessions"
ID=$("$BIN" new -d -n echoer -l card=7 -- sh -c 'echo hello-world; read x; echo got:$x; sleep 300')
check "new returns id" '[ -n "$ID" ]'
check "wait sees output" '"$BIN" wait echoer hello-world --timeout 5'
"$BIN" send echoer "typed-line"
check "send reaches process" '"$BIN" wait echoer got:typed-line --timeout 5'
check "screen has text" '"$BIN" screen echoer | grep -q got:typed-line'
check "logs strip ansi" '"$BIN" logs echoer | grep -q hello-world'
check "history records command" '"$BIN" history echoer | grep -q typed-line'
check "label filter" '"$BIN" ls --json -l card=7 | grep -q echoer'
check "label filter excludes" '! "$BIN" ls --json -l card=8 | grep -q echoer'

echo "# agent resume after a daemon crash"
CID=$("$BIN" new -d -n agent -C /tmp -- claude "build the thing" --dangerously-skip-permissions)
sleep 0.6
AGENT_ID=$("$BIN" ls --json | python3 -c "import sys,json;print([s for s in json.load(sys.stdin) if s['name']=='agent'][0]['agent_session_id'])")
check "claude got a session id" '[ -n "$AGENT_ID" ] && grep -q -- "--session-id $AGENT_ID" "$DPTY_HOME/claude-args.log"'
kill -9 $DAEMON; wait $DAEMON 2>/dev/null
sleep 0.5
check "stale socket left behind" '[ -S "$DPTY_HOME/dpty.sock" ]'
"$BIN" serve --http 127.0.0.1:0 >"$DPTY_HOME/daemon2.out" 2>&1 &
DAEMON=$!
for _ in $(seq 80); do grep -q -- "--resume $AGENT_ID" "$DPTY_HOME/claude-args.log" 2>/dev/null && break; sleep 0.1; done
check "claude relaunched with --resume <same id>" 'grep -q -- "--resume $AGENT_ID" "$DPTY_HOME/claude-args.log"'
check "resume drops the original prompt" '! tail -1 "$DPTY_HOME/claude-args.log" | grep -q "build the thing"'
check "resume keeps flags" 'tail -1 "$DPTY_HOME/claude-args.log" | grep -q dangerously-skip-permissions'
check "same session id kept" '"$BIN" ls | grep -q "$CID"'
check "shell session is lost, not restarted" '"$BIN" ls | grep echoer | grep -q lost'
check "history survives the crash" '"$BIN" logs echoer | grep -q hello-world'
check "commands survive the crash" '"$BIN" history echoer | grep -q typed-line'
"$BIN" restart echoer
check "manual restart brings shell session back" '"$BIN" wait echoer hello-world --timeout 5'

echo "# shell comes back where it was"
"$BIN" new -d -n walker -C /tmp -- sh >/dev/null
sleep 0.5
"$BIN" send walker "cd /usr && echo ARRIVED"; "$BIN" wait walker ARRIVED --timeout 5 >/dev/null
sleep 3
kill -9 $DAEMON; wait $DAEMON 2>/dev/null
"$BIN" serve --http 127.0.0.1:0 >"$DPTY_HOME/daemon3.out" 2>&1 &
DAEMON=$!
for _ in $(seq 50); do "$BIN" status >/dev/null 2>&1 && break; sleep 0.1; done
"$BIN" restart walker; sleep 0.8
"$BIN" send walker "pwd"; sleep 0.6
check "restored shell starts in its last directory" '"$BIN" screen walker | grep -q "^/usr"'

echo "# mcp (stdio)"
OUT=$(printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"pty_screen","arguments":{"id":"echoer"}}}' | "$BIN" mcp)
check "mcp initialize" 'echo "$OUT" | grep -q "durable-pty"'
check "mcp lists tools" 'echo "$OUT" | grep -q pty_send'
check "mcp pty_screen returns screen" 'echo "$OUT" | grep -q hello-world'

echo "# http"
HTTP=$(grep -o 'http://[0-9.:]*' "$DPTY_HOME/daemon3.out" | head -1)
check "health is open" 'curl -sf "$HTTP/health" | grep -q ok'
check "rpc needs token" '[ "$(curl -s -o /dev/null -w %{http_code} -X POST "$HTTP/rpc" -d "{}")" = 401 ]'
check "rpc list" 'curl -sf -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" -X POST "$HTTP/rpc" -d "{\"id\":1,\"method\":\"list\"}" | grep -q echoer'
check "mcp over http" 'curl -sf -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" -X POST "$HTTP/mcp" -d "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/list\"}" | grep -q pty_list'

echo "# stop keeps sessions for next start"
"$BIN" stop >/dev/null; sleep 0.5
check "daemon exited" '! kill -0 $DAEMON 2>/dev/null'
"$BIN" ls >/dev/null 2>&1 &  # autostarts the daemon again
sleep 1.5
check "autostart + recovery" '"$BIN" ls | grep -q agent'
"$BIN" stop >/dev/null 2>&1
pkill -f "$FAKE" 2>/dev/null
rm -rf "$DPTY_HOME"
[ $fail = 0 ] && echo "ALL OK" || { echo "FAILURES"; exit 1; }
