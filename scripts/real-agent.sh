#!/usr/bin/env bash
# Drives a REAL agent CLI (claude or codex) through dpty: start it, give it a fact, crash the daemon, check the same live
# process is still there, then simulate a reboot (kill the processes) and check the agent resumes its conversation.
# Usage: scripts/real-agent.sh claude|codex|grok [-- extra agent args]. Costs a few cheap model calls. Uses a throwaway DPTY_HOME.
set -u
AGENT="${1:?claude, codex or grok}"; shift || true
[ "${1:-}" = "--" ] && shift
EXTRA=("$@")
BIN="${BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/dpty}"
export DPTY_HOME="$(mktemp -d /tmp/dpty-real.XXXXXX)"
# Use a folder the agent already trusts: the test never answers a trust prompt for you (its default answer is "No, exit").
WORK="${WORK:-$HOME/Documents/GitHub/ideation-canvas}"
CODE="PELICAN-$RANDOM"
fail=0
ok()  { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; fail=1; }
cleanup() { "$BIN" stop --sessions >/dev/null 2>&1; pkill -f "$DPTY_HOME" 2>/dev/null; }
trap cleanup EXIT

screen() { "$BIN" screen "$AGENT-run" 2>/dev/null; }
# Waits for text on screen, pressing Enter through trust / confirmation prompts that appear on the way.
wait_for() {
  local pat="$1" secs="${2:-90}" i=0
  while [ $i -lt $((secs*2)) ]; do
    local s; s="$(screen)"
    if echo "$s" | grep -qE "$pat"; then return 0; fi
    if echo "$s" | grep -qiE "trust this folder|Do you trust|trust the (files|contents)"; then echo "  $AGENT is asking to trust $WORK: run it there once yourself, or set WORK to a trusted folder" >&2; return 1; fi
    if echo "$s" | grep -qiE "Press enter to continue"; then "$BIN" send "$AGENT-run" --no-enter -k enter; sleep 1; fi
    sleep 0.5; i=$((i+1))
  done
  echo "--- last screen:" >&2; screen | tail -25 >&2
  return 1
}
start_daemon() { "$BIN" serve --no-http >"$DPTY_HOME/daemon.$1.out" 2>&1 & DAEMON=$!; for _ in $(seq 60); do "$BIN" status >/dev/null 2>&1 && break; sleep 0.1; done; }
agent_id() { "$BIN" ls --json | python3 -c "import sys,json;s=[x for x in json.load(sys.stdin) if x['name']=='$AGENT-run'][0];print(s['agent_session_id'] or '')"; }
pid_of() { "$BIN" ls --json | python3 -c "import sys,json;s=[x for x in json.load(sys.stdin) if x['name']=='$AGENT-run'][0];print(s['pid'] or '')"; }
ask() { "$BIN" send "$AGENT-run" "$1"; }  # types, pauses, then presses Enter on its own

start_daemon 1
echo "# $AGENT: start the real CLI under dpty (cwd $WORK)"
"$BIN" new -d -n "$AGENT-run" -C "$WORK" -- "$AGENT" "${EXTRA[@]}" >/dev/null || { bad "could not start $AGENT"; exit 1; }
if wait_for "(Try|›|❯|> |\\?.*shortcuts|ctrl\\+|Ctrl\\+|esc to|Context|Welcome|Shift\\+Tab)" 90; then ok "$AGENT interface is up"; else bad "$AGENT interface did not come up"; exit 1; fi
sleep 2

A=$((RANDOM % 900 + 100)); B=$((RANDOM % 900 + 100)); SUM=$((A + B))
C=$((RANDOM % 900 + 1000)); D=$((RANDOM % 900 + 1000)); SUM2=$((C + D))
LOWER="$(echo "$CODE" | tr 'A-Z' 'a-z')"
# The checks look for text that only the agent's ANSWER can contain, never for words from our own prompt.

echo "# give it a fact"
ask "Remember the code word $CODE. Also, what is $A plus $B? Reply with just the number."
if wait_for "$SUM" 150; then ok "it answered ($A + $B = $SUM)"; else bad "no answer from $AGENT"; exit 1; fi
sleep 3
ID1="$(agent_id)"
if [ -z "$ID1" ]; then for _ in $(seq 60); do sleep 1; ID1="$(agent_id)"; [ -n "$ID1" ] && break; done; fi
[ -n "$ID1" ] && ok "agent session id known to dpty: $ID1" || bad "agent session id not found"
case "$AGENT" in
  claude)
    F="$(ls "$HOME"/.claude/projects/*/"$ID1".jsonl 2>/dev/null | head -1)"
    [ -n "$F" ] && ok "claude saved the transcript: $(basename "$F")" || bad "no claude transcript for $ID1" ;;
  grok)
    F="$(ls -d "$HOME"/.grok/sessions/*/"$ID1" 2>/dev/null | head -1)"
    [ -n "$F" ] && ok "grok saved the session: $ID1" || bad "no grok session folder for $ID1" ;;
  *)
    F="$(find "$HOME/.codex/sessions" -name "*$ID1*.jsonl" 2>/dev/null | head -1)"
    [ -n "$F" ] && ok "codex saved the rollout: $(basename "$F")" || bad "no codex rollout for $ID1" ;;
esac
P1="$(pid_of)"

echo "# crash the daemon: the SAME live $AGENT process must still be there"
kill -9 $DAEMON; wait $DAEMON 2>/dev/null; sleep 2
kill -0 "$P1" 2>/dev/null && ok "$AGENT process $P1 survived the daemon" || bad "process died with the daemon"
start_daemon 2; sleep 1.5
[ "$(pid_of)" = "$P1" ] && ok "daemon reattached to the same pid" || bad "pid changed"
screen | grep -q "$SUM" && ok "the conversation is still on the screen" || bad "screen lost the conversation"
ask "What is $C plus $D? Reply with just the number."
if wait_for "$SUM2" 150; then ok "the live process kept working through the crash ($C + $D = $SUM2)"; else bad "live process did not answer after the crash"; fi
sleep 3

echo "# simulate a reboot: kill the helper processes (and with them the agent)"
pkill -9 -f "dpty shim $DPTY_HOME"; kill -9 $DAEMON; wait $DAEMON 2>/dev/null; sleep 2
kill -0 "$P1" 2>/dev/null && bad "agent still alive after the simulated reboot" || ok "agent process is gone"
start_daemon 3
echo "  waiting for dpty to relaunch it with its resume command..."
for _ in $(seq 40); do "$BIN" events "$AGENT-run" -n 20 | grep -q " resume " && break; sleep 0.5; done
RESUME_LINE="$("$BIN" events "$AGENT-run" -n 20 | grep " resume " | tail -1 | sed 's/.* resume *//')"
[ -n "$RESUME_LINE" ] && ok "dpty relaunched it as: $RESUME_LINE" || bad "no resume event"
if wait_for "(Try|›|> |ctrl\\+|esc to|Context|Welcome|shortcuts|ctx:|shift\\+tab|bypass)" 90 >/dev/null; then ok "$AGENT is back up"; else bad "$AGENT did not come back"; fi
"$BIN" events "$AGENT-run" -n 20 | grep -q "resume-failed" && bad "the resume failed and dpty had to start a new conversation"
sleep 3
ask "What was the code word I gave you earlier? Reply with it in lowercase letters only."
if wait_for "$LOWER" 150; then ok "resumed $AGENT remembered the code word from before the reboot: $LOWER"; else bad "resumed $AGENT does not remember"; fi
[ $fail = 0 ] && echo "RESULT: $AGENT OK" || { echo "RESULT: $AGENT FAILED"; exit 1; }
