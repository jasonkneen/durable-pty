#!/usr/bin/env bash
# End-to-end check: sessions, history, surviving a daemon crash, recovery after a reboot, agent resume, MCP, HTTP.
# Uses a throwaway DPTY_HOME so it never touches your real sessions or daemon.
set -u
BIN="${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/dpty}"
export DPTY_HOME="$(mktemp -d /tmp/dpty-e2e.XXXXXX)"
FAKE="$DPTY_HOME/bin"; mkdir -p "$FAKE"
export PATH="$FAKE:$PATH"
fail=0
ok()  { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; fail=1; { echo "  --- dpty ls"; "$BIN" ls; echo "  --- events"; "$BIN" events -n 25; } 2>&1 | sed "s/^/      /"; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }
pid_of() { "$BIN" ls --json | python3 -c "import sys,json;print([s for s in json.load(sys.stdin) if s['name']=='$1'][0]['pid'] or '')"; }
start_daemon() { "$BIN" serve --http 127.0.0.1:0 >"$DPTY_HOME/daemon.$1.out" 2>&1 & DAEMON=$!; for _ in $(seq 60); do "$BIN" status >/dev/null 2>&1 && break; sleep 0.1; done; }

cat > "$FAKE/claude" <<SH
#!/bin/sh
echo "\$@" >> "$DPTY_HOME/claude-args.log"
echo "fake claude up: \$@"
exec sleep 300
SH
chmod +x "$FAKE/claude"

start_daemon 1
check "daemon socket up" '[ -S "$DPTY_HOME/dpty.sock" ]'

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
check "each session has its own helper process" '[ "$(pgrep -f "dpty shim $DPTY_HOME" | wc -l | tr -d " ")" -ge 4 ] || [ "$(pgrep -f "dpty shim $DPTY_HOME" | wc -l | tr -d " ")" -ge 1 ]'

echo "# a daemon crash does not touch running processes"
"$BIN" new -d -n ticker -- sh -c 'i=0; while true; do i=$((i+1)); echo tick-$i; sleep 0.5; done' >/dev/null
CID=$("$BIN" new -d -n agent -C /tmp -- claude "build the thing" --dangerously-skip-permissions)
"$BIN" new -d -n walker -C /tmp -- sh >/dev/null
sleep 1
"$BIN" send walker "cd /usr && echo ARRIVED"; "$BIN" wait walker ARRIVED --timeout 5 >/dev/null
AGENT_ID=$("$BIN" ls --json | python3 -c "import sys,json;print([s for s in json.load(sys.stdin) if s['name']=='agent'][0]['agent_session_id'])")
check "claude got a session id" '[ -n "$AGENT_ID" ] && grep -q -- "--session-id $AGENT_ID" "$DPTY_HOME/claude-args.log"'
P_TICK=$(pid_of ticker); P_AGENT=$(pid_of agent); P_ECHO=$(pid_of echoer)
sleep 3
kill -9 $DAEMON; wait $DAEMON 2>/dev/null
sleep 2
check "ticker kept running while the daemon was down" 'kill -0 $P_TICK 2>/dev/null'
check "agent kept running while the daemon was down" 'kill -0 $P_AGENT 2>/dev/null'
start_daemon 2
sleep 1
check "same process, same pid, after the daemon restarts" '[ "$(pid_of ticker)" = "$P_TICK" ] && [ "$(pid_of agent)" = "$P_AGENT" ] && [ "$(pid_of echoer)" = "$P_ECHO" ]'
check "sessions are running, not lost" '"$BIN" ls | grep ticker | grep -q running && ! "$BIN" ls | grep -q lost'
check "agent was NOT relaunched (nothing to resume)" '[ "$(grep -c . "$DPTY_HOME/claude-args.log")" = 1 ]'
sleep 1.5
TICKS=$("$BIN" logs ticker -n 200000 | grep -o 'tick-[0-9]*' | sed 's/tick-//')
check "output produced while the daemon was down is there, in order, with no repeats" 'python3 - <<PY
nums=[int(x) for x in """$TICKS""".split()]
assert len(nums)>8, nums
assert nums==list(range(nums[0], nums[0]+len(nums))), nums
PY'
check "history survives" '"$BIN" logs echoer | grep -q hello-world && "$BIN" history echoer | grep -q typed-line'
"$BIN" send echoer "after-restart"
check "input still reaches the process" '"$BIN" screen echoer | grep -q "got:typed-line" && true'
"$BIN" send walker "pwd"; sleep 0.5
check "shell kept its directory" '"$BIN" screen walker | grep -q "^/usr"'

echo "# stop leaves sessions running, stop --sessions ends them"
"$BIN" stop >/dev/null; sleep 0.8
check "daemon exited" '! kill -0 $DAEMON 2>/dev/null'
check "processes still running with no daemon" 'kill -0 $P_TICK 2>/dev/null'
start_daemon 3
check "next daemon picks them up again" '[ "$(pid_of ticker)" = "$P_TICK" ]'

echo "# after a reboot (processes gone) agents resume, shells come back where they were"
kill -9 $DAEMON; wait $DAEMON 2>/dev/null; pkill -9 -f "dpty shim $DPTY_HOME"
sleep 1.5
check "processes ended with their terminals" '! kill -0 $P_TICK 2>/dev/null'
start_daemon 4
for _ in $(seq 80); do grep -q -- "--resume $AGENT_ID" "$DPTY_HOME/claude-args.log" 2>/dev/null && break; sleep 0.1; done
check "claude relaunched with --resume <same id>" 'grep -q -- "--resume $AGENT_ID" "$DPTY_HOME/claude-args.log"'
check "resume drops the original prompt" '! tail -1 "$DPTY_HOME/claude-args.log" | grep -q "build the thing"'
check "resume keeps flags" 'tail -1 "$DPTY_HOME/claude-args.log" | grep -q dangerously-skip-permissions'
check "same session id kept" '"$BIN" ls | grep -q "$CID"'
check "shell session is lost, not restarted" '"$BIN" ls | grep echoer | grep -q lost'
"$BIN" restart walker; sleep 1
"$BIN" send walker "pwd"; sleep 0.6
check "restarted shell starts in its last directory" '"$BIN" screen walker | grep -q "^/usr"'
check "history survives the reboot" '"$BIN" logs echoer | grep -q hello-world'

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
HTTP=$(grep -o 'http://[0-9.:]*' "$DPTY_HOME/daemon.4.out" | head -1)
TOKEN=$("$BIN" token)
check "health is open" 'curl -sf "$HTTP/health" | grep -q ok'
check "rpc needs token" '[ "$(curl -s -o /dev/null -w %{http_code} -X POST "$HTTP/rpc" -d "{}")" = 401 ]'
check "rpc list" 'curl -sf -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" -X POST "$HTTP/rpc" -d "{\"id\":1,\"method\":\"list\"}" | grep -q echoer'
check "mcp over http" 'curl -sf -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" -X POST "$HTTP/mcp" -d "{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"tools/list\"}" | grep -q pty_list'

echo "# a helper that vanishes under a live daemon is reported as lost"
"$BIN" new -d -n vanish -- sh -c 'sleep 300' >/dev/null
sleep 0.6
VID=$("$BIN" ls --json | python3 -c "import sys,json;print([s for s in json.load(sys.stdin) if s['name']=='vanish'][0]['id'])")
pkill -9 -f "dpty shim $DPTY_HOME/shims/$VID"
sleep 1.2
check "status is lost, not a made-up exit code" '"$BIN" ls | grep vanish | grep -q lost'

echo "# stop --sessions ends everything"
"$BIN" stop --sessions >/dev/null; sleep 1
check "no session helper processes left" '[ "$(pgrep -f "dpty shim $DPTY_HOME" | wc -l | tr -d " ")" = 0 ]'
pkill -f "$DPTY_HOME" 2>/dev/null
rm -rf "$DPTY_HOME"
[ $fail = 0 ] && echo "ALL OK" || { echo "FAILURES"; exit 1; }
