#!/bin/sh
# Drive the real Worktable app through worktable-cli and assert its JSON.
#
# Usage:
#   sh scripts/cli-smoke.sh
#
# On a host without Screen Recording permission (or with a locked display),
# the screenshot step is exercised but reported as blocked. Pass
# ALLOW_SCREENSHOT_BLOCKER=1 to keep the rest of the smoke run green in that
# environment; the exact screencapture error is always printed verbatim.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TARGET=${CARGO_TARGET_DIR:-"$ROOT/target"}
APP="$TARGET/debug/worktable-app"
CLI="$TARGET/debug/worktable-cli"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

# One Python expression per assertion, with the parsed reply as `d`.
assert_json() {
    description=$1
    expression=$2
    payload=$3
    printf '%s' "$payload" | python3 -c '
import json, sys
d = json.load(sys.stdin)
expression = sys.argv[1]
if not eval(expression, {"d": d}):
    raise SystemExit(f"assertion failed: {sys.argv[2]}")
' "$expression" "$description" || fail "$description"
}

json_field() {
    field=$1
    payload=$2
    printf '%s' "$payload" | python3 -c '
import json, sys
value = json.load(sys.stdin)
for part in sys.argv[1].split("."):
    value = value[part]
print(value)
' "$field"
}

# Mutation commands print the full response; assert against its state.
response_state() {
    printf '%s' "$1" | python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin)["state"]))'
}

echo "== build"
cargo build --manifest-path "$ROOT/Cargo.toml" -p worktable-app -p worktable-cli
[ -x "$APP" ] || fail "app binary missing at $APP"
[ -x "$CLI" ] || fail "CLI binary missing at $CLI"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/wt-cli-smoke.XXXXXX")
SOCKET="$TMP/control.sock"
DATA="$TMP/data"
STOPPED=0

cleanup() {
    if [ "$STOPPED" -eq 0 ]; then
        "$CLI" --socket "$SOCKET" stop >/dev/null 2>&1 || true
    fi
    rm -rf "$TMP"
}
trap cleanup EXIT

echo "== start"
START=$("$CLI" --socket "$SOCKET" start --data-dir "$DATA")
assert_json "start prints a socket" 'd["socket"] == "'"$SOCKET"'"' "$START"
assert_json "start prints the data dir" 'd["data_dir"] == "'"$DATA"'"' "$START"
assert_json "start prints a pid" 'isinstance(d["pid"], int) and d["pid"] > 0' "$START"
[ -S "$SOCKET" ] || fail "control socket was not created"
[ -f "$SOCKET.session.json" ] || fail "session file was not written"
SOCKET_MODE=$(stat -f '%Lp' "$SOCKET")
[ "$SOCKET_MODE" = "600" ] || fail "control socket mode is $SOCKET_MODE, expected 600"

echo "== state"
STATE=$("$CLI" --socket "$SOCKET" state)
assert_json "state starts on Entries" 'd["mode"] == "entries"' "$STATE"
assert_json "state starts empty" 'd["entries"] == []' "$STATE"
assert_json "state reports no tour" 'd["onboarding"]["step"] is None' "$STATE"
assert_json "state reports window bounds" 'd["window"]["bounds"]["width"] > 0 and d["window"]["bounds"]["height"] > 0' "$STATE"
[ "$(json_field mode "$STATE")" = "entries" ] || fail "mode helper"

echo "== add-note"
ADD=$("$CLI" --socket "$SOCKET" add-note "Smoke note alpha" --title "Smoke")
assert_json "add-note succeeds" 'd["ok"] is True' "$ADD"
ID=$(json_field state.result.id "$ADD")
[ -n "$ID" ] || fail "add-note returned no id"
SECOND=$("$CLI" --socket "$SOCKET" add-note "Second smoke note")
SECOND_ID=$(json_field state.result.id "$SECOND")
[ -n "$SECOND_ID" ] || fail "second add-note returned no id"

# The insert is async; wait until the same view reports it.
deadline=$((SECONDS + 10))
while :; do
    STATE=$("$CLI" --socket "$SOCKET" state)
    printf '%s' "$STATE" | python3 -c '
import json, sys
state = json.load(sys.stdin)
ids = {entry["id"] for entry in state["entries"]}
raise SystemExit(0 if sys.argv[1] in ids and sys.argv[2] in ids else 1)
' "$ID" "$SECOND_ID" && break
    [ "$SECONDS" -lt "$deadline" ] || fail "seeded notes did not appear"
    sleep 0.1
done
assert_json "the note keeps its title" 'any(e["id"] == "'"$ID"'" and e["title"] == "Smoke" for e in d["entries"])' "$STATE"

echo "== search"
SEARCH=$("$CLI" --socket "$SOCKET" search alpha)
SEARCH_STATE=$(response_state "$SEARCH")
assert_json "search sets the query" 'd["query"] == "alpha"' "$SEARCH_STATE"
assert_json "search filters the visible list" 'd["visible_entry_ids"] == ["'"$ID"'"]' "$SEARCH_STATE"
CLEAR=$("$CLI" --socket "$SOCKET" clear-search)
CLEAR_STATE=$(response_state "$CLEAR")
assert_json "clear-search empties the query" 'd["query"] == ""' "$CLEAR_STATE"
assert_json "clear-search restores both entries" 'len(d["visible_entry_ids"]) == 2' "$CLEAR_STATE"

echo "== label/select/open/close"
LABEL=$("$CLI" --socket "$SOCKET" label amber --id "$ID")
LABEL_STATE=$(response_state "$LABEL")
assert_json "label applies" 'next(e["label"] for e in d["entries"] if e["id"] == "'"$ID"'") == "amber"' "$LABEL_STATE"
SELECT=$("$CLI" --socket "$SOCKET" select "$ID")
SELECT_STATE=$(response_state "$SELECT")
assert_json "select applies" 'next(e["selected"] for e in d["entries"] if e["id"] == "'"$ID"'") is True' "$SELECT_STATE"
OPEN=$("$CLI" --socket "$SOCKET" open)
OPEN_STATE=$(response_state "$OPEN")
assert_json "open shows the detail" 'd["entry_modal"]["id"] == "'"$ID"'"' "$OPEN_STATE"
CLOSE=$("$CLI" --socket "$SOCKET" close)
assert_json "close starts the close morph" 'd["ok"] is True' "$CLOSE"
deadline=$((SECONDS + 5))
while :; do
    STATE=$("$CLI" --socket "$SOCKET" state)
    printf '%s' "$STATE" | python3 -c '
import json, sys
raise SystemExit(0 if json.load(sys.stdin)["entry_modal"] is None else 1)
' && break
    [ "$SECONDS" -lt "$deadline" ] || fail "entry detail did not unmount"
    sleep 0.1
done

echo "== pages/settings"
ASSISTANT=$("$CLI" --socket "$SOCKET" page assistant)
assert_json "assistant page" 'd["state"]["mode"] == "assistant"' "$ASSISTANT"
SETTINGS=$("$CLI" --socket "$SOCKET" page settings)
assert_json "settings page" 'd["state"]["mode"] == "settings" and d["state"]["settings_tab"] is None' "$SETTINGS"
PROVIDERS=$("$CLI" --socket "$SOCKET" settings providers)
assert_json "providers category" 'd["state"]["settings_tab"] == "providers" and d["state"]["providers"]["providers"]' "$PROVIDERS"
BACK=$("$CLI" --socket "$SOCKET" back)
assert_json "back leaves the category" 'd["state"]["mode"] == "settings" and d["state"]["settings_tab"] is None' "$BACK"
ENTRIES=$("$CLI" --socket "$SOCKET" back)
assert_json "back returns to entries" 'd["state"]["mode"] == "entries"' "$ENTRIES"

echo "== raw protocol"
RAW=$("$CLI" --socket "$SOCKET" cmd '{"action":"toggle-theme"}')
assert_json "cmd accepts raw protocol" 'd["ok"] is True and d["state"]["theme"] in ("light", "dark")' "$RAW"

BATCH_FILE="$TMP/batch.json"
cat >"$BATCH_FILE" <<'JSON'
[
  {"action":"page","page":"entries"},
  {"action":"sort","mode":"alpha","ascending":true},
  {"action":"search","query":"smoke"},
  {"action":"clear-search"}
]
JSON
BATCH=$("$CLI" --socket "$SOCKET" batch "$BATCH_FILE")
assert_json "batch runs every command" 'len(d) == 4 and all(r["ok"] for r in d)' "$BATCH"
assert_json "batch ends sorted and unfiltered" 'd[-1]["state"]["query"] == "" and d[-1]["state"]["sort"]["mode"] == "alpha"' "$BATCH"

echo "== window"
WINDOW=$("$CLI" --socket "$SOCKET" window show)
assert_json "window show" 'd["ok"] is True and d["state"]["window"]["bounds"]["width"] > 0' "$WINDOW"

echo "== screenshot"
SCREENSHOT_BLOCKED=0
if SHOT=$("$CLI" --socket "$SOCKET" screenshot 2>"$TMP/screenshot.err"); then
    assert_json "screenshot reports a path" 'd["path"].endswith(".png")' "$SHOT"
    SHOT_PATH=$(json_field path "$SHOT")
    [ -s "$SHOT_PATH" ] || fail "screenshot file is empty: $SHOT_PATH"
    echo "screenshot: $SHOT_PATH"
elif [ "${ALLOW_SCREENSHOT_BLOCKER:-0}" = "1" ]; then
    SCREENSHOT_BLOCKED=1
    echo "SCREENSHOT BLOCKED (ALLOW_SCREENSHOT_BLOCKER=1):"
    cat "$TMP/screenshot.err"
else
    cat "$TMP/screenshot.err" >&2
    fail "screenshot failed; grant Screen Recording permission and unlock the display, or rerun with ALLOW_SCREENSHOT_BLOCKER=1"
fi

echo "== stop"
STOP=$("$CLI" --socket "$SOCKET" stop)
STOPPED=1
assert_json "stop reports the running session" 'd["stopped"] is True' "$STOP"
[ ! -e "$SOCKET" ] || fail "stop left the control socket behind"

if [ "$SCREENSHOT_BLOCKED" -eq 1 ]; then
    echo "SMOKE PASSED except screenshot (environment blocker reported above)"
else
    echo "SMOKE PASSED"
fi
