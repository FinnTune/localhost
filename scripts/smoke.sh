#!/usr/bin/env bash
# Fast (<5s) end-to-end check: boots the real server and drives it over real
# sockets with curl, asserting behavior cargo test can't see (unit tests
# exercise the modules directly; this exercises the actual event loop,
# actual routing, actual disk I/O). Meant for CI on every push — see
# scripts/stress.sh for the heavier load/leak/concurrency pass.
#
# Usage: scripts/smoke.sh
# Env overrides: HOST, PORT

set -uo pipefail

HOST="${HOST:-127.0.0.1}"
PORT="${PORT:-8080}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

PASS=0
FAIL=0
SERVER_PID=""
UPLOAD_FILE="$(mktemp)"

pass() { printf '  \033[32mPASS\033[0m %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$1"; FAIL=$((FAIL + 1)); }
section() { printf '\n\033[1m== %s ==\033[0m\n' "$1"; }

cleanup() {
    rm -f "$UPLOAD_FILE"
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

expect_status() {
    local description="$1" expected="$2"
    shift 2
    local actual
    actual="$(curl -s -o /dev/null -w '%{http_code}' "$@")"
    if [ "$actual" = "$expected" ]; then
        pass "$description ($actual)"
    else
        fail "$description (expected $expected, got $actual)"
    fi
}

expect_body_contains() {
    local description="$1" needle="$2"
    shift 2
    local body
    body="$(curl -s "$@")"
    if [ "${body#*"$needle"}" != "$body" ]; then
        pass "$description"
    else
        fail "$description (response didn't contain '$needle')"
    fi
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
    else
        echo "port $PORT is already in use by a different process (pid $EXISTING_PID, ${EXISTING_EXE:-unknown}) — refusing to run the smoke test against it." >&2
        echo "Stop that process or point HOST/PORT (and config/config.json) at a free port." >&2
        exit 1
    fi
else
    ./target/debug/localhost >/tmp/localhost-smoke.log 2>&1 &
    SERVER_PID=$!
    if wait_for_server; then
        echo "  started target/debug/localhost (pid $SERVER_PID)"
    else
        fail "server did not come up within 5s"
        cat /tmp/localhost-smoke.log >&2
        exit 1
    fi
fi

section "Static files"
expect_status "GET / returns 200" 200 "http://$HOST:$PORT/"
expect_status "GET /about returns 200" 200 "http://$HOST:$PORT/about"
expect_status "GET /nonexistent returns 404" 404 "http://$HOST:$PORT/nonexistent"
expect_status "PUT / on a GET-only location returns 405" 405 -X PUT "http://$HOST:$PORT/"

section "CGI"
expect_status "GET /cgi-bin/hello.sh returns 200" 200 "http://$HOST:$PORT/cgi-bin/hello.sh?x=1"
expect_body_contains "CGI output includes the query string" "Query: x=1" \
    "http://$HOST:$PORT/cgi-bin/hello.sh?x=1"

section "Upload / POST / DELETE round trip"
echo "smoke test payload $$" > "$UPLOAD_FILE"
UPLOAD_NAME="smoke-$$.txt"
expect_status "POST multipart upload succeeds" 201 \
    -F "file=@$UPLOAD_FILE;filename=$UPLOAD_NAME" "http://$HOST:$PORT/upload"
expect_body_contains "uploaded file is servable and matches what was sent" "smoke test payload $$" \
    "http://$HOST:$PORT/upload/$UPLOAD_NAME"
expect_status "DELETE removes the uploaded file" 204 \
    -X DELETE "http://$HOST:$PORT/upload/$UPLOAD_NAME"
expect_status "uploaded file is gone after DELETE" 404 "http://$HOST:$PORT/upload/$UPLOAD_NAME"

section "Malformed input doesn't wedge the server"
(exec 3<>"/dev/tcp/$HOST/$PORT"; printf 'GARBAGE NOT HTTP AT ALL\r\n\r\n' >&3; timeout 2 cat <&3 >/dev/null) 2>/dev/null
expect_status "server still answers after garbage input" 200 "http://$HOST:$PORT/"

section "Summary"
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
