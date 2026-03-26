#!/usr/bin/env bash
set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
readonly E2E_OUTPUT_DIR="${LTSEARCH_E2E_OUTPUT_DIR:-$REPO_ROOT/.e2e-tmp}"
readonly SAM_API_PID_FILE="$E2E_OUTPUT_DIR/sam-api.pid"

if [[ -f "$SAM_API_PID_FILE" ]]; then
  SAM_PID="$(cat "$SAM_API_PID_FILE")"
  if kill -0 "$SAM_PID" >/dev/null 2>&1; then
    kill "$SAM_PID" && wait "$SAM_PID" 2>/dev/null || true
    echo "SAM API (PID $SAM_PID) stopped." >&2
  fi
  rm -f "$SAM_API_PID_FILE"
fi

docker compose -f "$REPO_ROOT/docker-compose.moto.yml" down -v 2>/dev/null || true

echo "Moto stopped." >&2
