#!/usr/bin/env bash
# Load/fuzz/leak-check pass for the localhost HTTP server. Runs the checks
# described in the README's "Stress testing" section as one command.
#
# Usage: scripts/stress.sh
# Env overrides: HOST, PORT, SIEGE_CONCURRENCY, SIEGE_TIME

set -uo pipefail

HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-8080}"
SIEGE_CONCURRENCY="${SIEGE_CONCURRENCY:-25}"
SIEGE_TIME="${SIEGE_TIME:-30S}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PASS=0
FAIL=0
SERVER_PID=""
URLS_FILE="$(mktemp)"

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }
section() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

cleanup() {
    rm -f "$URLS_FILE"
    if [ -n "$SERVER_PID" ] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null
        wait "$SERVER_PID" 2>/dev/null
    fi
}
trap cleanup EXIT

wait_for_server() {
    for _ in $(seq 1 50); do
        curl -s -o /dev/null "http://$HOST:$PORT/" && return 0
        sleep 0.1
    done
    return 1
}

# pid of whatever process currently holds $1 in LISTEN state, or empty.
port_owner_pid() {
    ss -ltnp 2>/dev/null | awk -v p=":$1 " '$0 ~ p' | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2
}

section "Build"
if ! cargo build --quiet; then
    echo "cargo build failed" >&2
    exit 1
fi
echo "  built target/debug/localhost"

section "Start server"
OUR_EXE="$(readlink -f target/debug/localhost)"
EXISTING_PID="$(port_owner_pid "$PORT")"
if [ -n "$EXISTING_PID" ]; then
    EXISTING_EXE="$(readlink -f "/proc/$EXISTING_PID/exe" 2>/dev/null || true)"
    if [ "$EXISTING_EXE" = "$OUR_EXE" ]; then
        echo "  reusing already-running target/debug/localhost (pid $EXISTING_PID, won't be killed on exit)"
        SERVER_PID_FOR_FD_CHECK="$EXISTING_PID"
    else
        echo "port $PORT is already in use by a different process (pid $EXISTING_PID, ${EXISTING_EXE:-unknown}) — refusing to run the stress test against it." >&2
        echo "Stop that process or point HOST/PORT (and config/config.json) at a free port." >&2
        exit 1
    fi
else
    ./target/debug/localhost >/tmp/localhost-stress.log 2>&1 &
    SERVER_PID=$!
    if wait_for_server; then
        echo "  started target/debug/localhost (pid $SERVER_PID)"
        SERVER_PID_FOR_FD_CHECK="$SERVER_PID"
    else
        fail "server did not come up within 5s"
        cat /tmp/localhost-stress.log >&2
        exit 1
    fi
fi

section "Load test (siege)"
if command -v siege >/dev/null 2>&1; then
    printf '%s\n' \
        "http://$HOST:$PORT/" \
        "http://$HOST:$PORT/about" \
        "http://$HOST:$PORT/cgi-bin/hello.sh?x=1" > "$URLS_FILE"

    FD_BEFORE=0
    [ -n "$SERVER_PID_FOR_FD_CHECK" ] && FD_BEFORE=$(ls "/proc/$SERVER_PID_FOR_FD_CHECK/fd" 2>/dev/null | wc -l)

    SIEGE_LOG="$(mktemp)"
    siege -c "$SIEGE_CONCURRENCY" -t "$SIEGE_TIME" -f "$URLS_FILE" --log="$SIEGE_LOG" 2>&1 | tee /tmp/localhost-siege.log
    if grep -q "Availability:[[:space:]]*100\.00" /tmp/localhost-siege.log; then
        pass "100% availability under $SIEGE_CONCURRENCY concurrent clients for $SIEGE_TIME"
    else
        avail=$(grep -o "Availability:[[:space:]]*[0-9.]*" /tmp/localhost-siege.log || echo "unknown")
        fail "availability was not 100% ($avail) — see /tmp/localhost-siege.log"
    fi
    rm -f "$SIEGE_LOG"

    sleep 0.5
    FD_AFTER=0
    [ -n "$SERVER_PID_FOR_FD_CHECK" ] && FD_AFTER=$(ls "/proc/$SERVER_PID_FOR_FD_CHECK/fd" 2>/dev/null | wc -l)
    echo "  fd count: before=$FD_BEFORE after=$FD_AFTER"
    if [ "$FD_BEFORE" -gt 0 ] && [ "$((FD_AFTER - FD_BEFORE))" -le 2 ]; then
        pass "no fd leak across the run (delta $((FD_AFTER - FD_BEFORE)))"
    else
        fail "fd count grew by $((FD_AFTER - FD_BEFORE)) — possible leak"
    fi
else
    echo "  siege not found, falling back to a burst of parallel curls"
    ok=0
    total=40
    for i in $(seq 1 $total); do
        curl -s -o /dev/null -w '%{http_code}\n' "http://$HOST:$PORT/" &
    done | grep -c '^200$' > /tmp/localhost-curl-ok.txt
    ok=$(cat /tmp/localhost-curl-ok.txt)
    echo "  $ok/$total parallel requests returned 200"
    [ "$ok" -eq "$total" ] && pass "all parallel requests succeeded" || fail "$((total - ok)) requests failed"
fi

section "Zombie CGI children"
zombies=$(ps aux | grep '[d]efunct' | wc -l)
if [ "$zombies" -eq 0 ]; then
    pass "no zombie processes"
else
    fail "$zombies zombie process(es) found"
    ps aux | grep '[d]efunct'
fi

section "Concurrency proof (idle connection must not block others)"
(exec 3<>"/dev/tcp/$HOST/$PORT"; sleep 8) &
IDLE_PID=$!
sleep 0.3
TIME_TOTAL=$(curl -s -o /dev/null -w '%{time_total}' "http://$HOST:$PORT/")
echo "  concurrent request while a connection sits idle: ${TIME_TOTAL}s"
if awk -v t="$TIME_TOTAL" 'BEGIN{exit !(t < 2.0)}'; then
    pass "second client served immediately (${TIME_TOTAL}s), not starved by the idle one"
else
    fail "second client took ${TIME_TOTAL}s — looks starved"
fi
wait "$IDLE_PID" 2>/dev/null

section "Malformed input handling"
(exec 3<>"/dev/tcp/$HOST/$PORT"; printf 'GARBAGE NOT HTTP AT ALL\r\n\r\n' >&3; timeout 2 cat <&3 >/dev/null) 2>/dev/null
OVERSIZED_HEADER="GET / HTTP/1.1\r\nHost: $HOST\r\nX-Pad: $(head -c 9000 < /dev/zero | tr '\0' 'a')\r\n\r\n"
(exec 3<>"/dev/tcp/$HOST/$PORT"; printf "%b" "$OVERSIZED_HEADER" >&3; timeout 2 cat <&3 >/dev/null) 2>/dev/null
(exec 3<>"/dev/tcp/$HOST/$PORT"; printf 'POST / HTTP/1.1\r\nHost: %s\r\nContent-Length: 999999999\r\n\r\nshort body' "$HOST" >&3; timeout 2 cat <&3 >/dev/null) 2>/dev/null

CODE=$(curl -s -o /dev/null -w '%{http_code}' "http://$HOST:$PORT/")
if [ "$CODE" = "200" ]; then
    pass "server still answers 200 after garbage bytes, oversized header, and a false Content-Length"
else
    fail "server returned $CODE after malformed input (expected 200 — server may have crashed or wedged)"
fi

section "Summary"
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
