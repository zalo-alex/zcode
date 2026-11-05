#!/usr/bin/env bash
# End-to-end test of the REPL driven through tmux: a detached session, a message typed with `send-keys -l` + Enter,
# the pane read with capture-pane, idle detection by an unchanged pane, then --continue in a fresh pane.
# usage: scripts/tmux-e2e.sh [path/to/zcode]      (also used inside the bookworm container)
set -euo pipefail
ZCODE=${1:-$(dirname "$0")/../target/release/zcode}
ZCODE=$(readlink -f "$ZCODE")
PY=${PYTHON:-python3}
[ -x "$(dirname "$0")/../bench/venv/bin/python" ] && PY=$(readlink -f "$(dirname "$0")/../bench/venv/bin/python")
T=$(mktemp -d); trap 'tmux -L zce kill-server 2>/dev/null || true; kill $SRV 2>/dev/null || true; rm -rf "$T"' EXIT
PORT=$((20000 + RANDOM % 20000))
"$PY" "$(dirname "$0")/fake_llm.py" $PORT "$T/requests.log" & SRV=$!
for _ in $(seq 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" 2>/dev/null && break; sleep 0.1; done
mkdir -p "$T/work" "$T/home"
CFG='{"provider":{"fake":{"options":{"baseURL":"http://127.0.0.1:'$PORT'/v1","apiKey":"unused"},"models":{"m":{"limit":{"context":128000,"output":1000}}}}},"model":"fake/m","permission":{"*":"allow"}}'
ok() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; echo "--- pane ---"; tmux -L zce capture-pane -p -t zc -S -60 || true; exit 1; }
pane() { tmux -L zce capture-pane -p -t zc -S -200; }
wait_for() { # pattern timeout
  for _ in $(seq $(( $2 * 5 ))); do pane | grep -qF -- "$1" && return 0; sleep 0.2; done; return 1; }
start() { # extra args
  tmux -L zce kill-server 2>/dev/null || true
  tmux -L zce new-session -d -s zc -x 200 -y 50 -c "$T/work" \
    -e HOME="$T/home" -e XDG_DATA_HOME="$T/data" -e XDG_CACHE_HOME="$T/cache" -e XDG_CONFIG_HOME="$T/config" \
    -e OPENCODE_CONFIG_CONTENT="$CFG" -e TERM=xterm-256color "$ZCODE" --auto "$@"
}
ensure_ready() { # ready = pane longer than 20 chars and unchanged twice in a row
  local last="" stable=0 cur
  for _ in $(seq 14); do sleep 0.7; cur=$(pane | head -20); 
    if [ ${#cur} -gt 20 ]; then [ "$cur" = "$last" ] && stable=$((stable+1)) || stable=0; [ $stable -ge 2 ] && return 0; fi; last=$cur; done; return 1; }
type_line() { tmux -L zce send-keys -t zc -l -- "$1"; sleep 0.4; tmux -L zce send-keys -t zc Enter; }

start
ensure_ready && ok "banner drawn and stable within 10s" || fail "banner never stabilised"
type_line "hello there"
wait_for "FAKE-REPLY: hello there" 15 && ok "typed line submitted, reply shown" || fail "no reply to typed line"

LONG=$(printf 'abcdefghij%.0s' $(seq 400))   # 4000 chars in one line
type_line "$LONG"
wait_for "FAKE-REPLY: abcdefghij" 15 && ok "4000-char line accepted" || fail "long line lost"
"$PY" - "$T/requests.log" "$LONG" <<'PY' || fail "long line was truncated on its way to the model"
import json, sys
last = [json.loads(l) for l in open(sys.argv[1])][-1]
u = [m for m in last["messages"] if m["role"] == "user"][-1]["content"]
assert u == sys.argv[2], (len(u), len(sys.argv[2]))
PY
ok "long line reached the model intact"

type_line "slow first"
sleep 0.5
type_line "typed ahead"   # typed while the previous turn is still running
wait_for "FAKE-REPLY: typed ahead" 20 && ok "type-ahead executed after the running turn" || fail "type-ahead lost"
P=$(pane); [ "$(echo "$P" | grep -n 'FAKE-REPLY: slow first' | head -1 | cut -d: -f1)" -lt "$(echo "$P" | grep -n 'FAKE-REPLY: typed ahead' | head -1 | cut -d: -f1)" ] \
  && ok "type-ahead ran in order" || fail "wrong order"

h1=$(pane | md5sum); sleep 2.5; h2=$(pane | md5sum)
[ "$h1" = "$h2" ] && ok "pane is stable when idle (idle = unchanged pane)" || fail "pane keeps changing while idle"

type_line "fail429 please"
wait_for "LLM HTTP 429" 40 && ok "API error printed verbatim in the pane" || fail "429 not visible"
pane | grep -qF "rate limit exceeded" && ok "error body visible (visible to anything reading the pane)" || fail "no error body"
type_line "after the error"
wait_for "FAKE-REPLY: after the error" 15 && ok "REPL survived the error; next message works" || fail "REPL dead after error"

tmux -L zce kill-server; sleep 0.5
start --continue
ensure_ready && wait_for "resumed session" 10 && ok "--continue resumed in a new pane" || fail "no resume banner"
wait_for "FAKE-REPLY: after the error" 5 && ok "resume banner shows the last reply" || fail "last reply not shown"
type_line "and now"
wait_for "FAKE-REPLY: and now" 15 && ok "resumed session answers" || fail "resumed session dead"
"$PY" - "$T/requests.log" <<'PY' || fail "history missing after resume"
import json, sys
last = [json.loads(l) for l in open(sys.argv[1])][-1]
s = json.dumps(last["messages"])
assert "hello there" in s and "typed ahead" in s and "and now" in s
PY
ok "model received the whole earlier conversation after --continue"
echo "ALL TMUX E2E CHECKS PASSED"
