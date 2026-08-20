#!/usr/bin/env bash
# helix-dev.sh — quick HelixDB health probe for Worktable.
# Default gateway is http://localhost:6969 (helix start dev).
# Respects HELIX_URL / WORKTABLE_HELIX_URL / HELIXDB_URL and
# WORKTABLE_HELIX_API_KEY / HELIX_API_KEY if set.
set -euo pipefail

HELIX_URL="${HELIX_URL:-${WORKTABLE_HELIX_URL:-${HELIXDB_URL:-http://localhost:6969}}}"
API_KEY="${WORKTABLE_HELIX_API_KEY:-${HELIX_API_KEY:-${HELIXDB_API_KEY:-}}}"
CURL_FLAGS=(-sS --max-time 3)

# Build curl auth args as an array so quoting is correct.
AUTH_ARGS=()
if [[ -n "$API_KEY" ]]; then
  AUTH_ARGS=(-H "Authorization: Bearer $API_KEY")
fi

echo "Helix gateway: $HELIX_URL"
echo ""

# 1) Try GET /health (some Helix builds expose it).
echo "→ GET $HELIX_URL/health ..."
if curl "${CURL_FLAGS[@]}" "${AUTH_ARGS[@]}" "$HELIX_URL/health" 2>&1 | head -n 20; then
  echo "  (health endpoint responded)"
else
  echo "  (no /health or connection refused — trying POST /v2/query)"
fi
echo ""

# 2) POST /v2/query — the real SDK path. Send a tiny read.
echo "→ POST $HELIX_URL/v2/query (limit 1 Entry) ..."
# Build a minimal valid batch via a canned Helix DSL JSON (list 1). For a
# lightweight probe we just POST valid JSON; a 200 means the gateway is up,
# 4xx may mean the payload needs DSL shape but still proves connectivity.
# Prefer the helix-db probe: POST a read_batch limit 1.
PROBE_JSON='{"query_name":"helix_probe","parameters":{"limit":{"i64":1}},"query_type":"read","batch":{"batches":[{"query_name":"helix_probe","batch_index":0,"entries":[{"type":"Query","value":{"query":{"root":{"limit":{"input":{"nodes_where":{"predicate":{"has_key":{"property":"$id"}}}},"count":1}},"returning":["count"]}}}]}]}}'

# Use a simpler probe: ask for n_with_label limit 1 via raw SDK JSON shape.
# If that fails, just report curl result.
set +e
RESPONSE=$(curl "${CURL_FLAGS[@]}" -H "Content-Type: application/json" "${AUTH_ARGS[@]}" -X POST "$HELIX_URL/v2/query" -d "$PROBE_JSON" 2>&1)
CURL_EXIT=$?
set -e

echo "$RESPONSE" | python3 -m json.tool 2>/dev/null || echo "$RESPONSE"
echo ""

if [[ $CURL_EXIT -eq 0 ]]; then
  echo "✓ Helix gateway reachable at $HELIX_URL"
  echo "  SQLite remains source-of-truth (~/.worktable/worktable.db); Helix is best-effort."
else
  echo "✗ Helix not reachable at $HELIX_URL (curl exit $CURL_EXIT)"
  echo "  App will run fine on SQLite. Start Helix with: helix start dev  # listens on :6969"
  echo "  Or set HELIX_URL to your remote cluster."
  exit 1
fi

# 3) Optional: show Helix version if endpoint exists.
echo ""
echo "→ GET $HELIX_URL/version (best-effort) ..."
curl "${CURL_FLAGS[@]}" "${AUTH_ARGS[@]}" "$HELIX_URL/version" 2>&1 | head -n 5 || true
