#!/usr/bin/env bash
set -euo pipefail

export SOAK_MINUTES=${SOAK_MINUTES:-30}
if [[ ! $SOAK_MINUTES =~ ^[1-9][0-9]*$ ]]; then
    echo "SOAK_MINUTES must be a positive integer" >&2
    exit 1
fi
export RUSTTALK_SOAK_PROJECT=rusttalk-soak-$$
export RUSTTALK_DEV_SESSION_TTL_SECS=90
export RUSTTALK_DEV_DATA
RUSTTALK_DEV_DATA=$(mktemp -d)
compose() {
    docker compose -f deploy/compose.dev.yml --profile index -p "$RUSTTALK_SOAK_PROJECT" "$@"
}

cleanup() {
    local status=$?
    if compose logs --no-color api caddy livekit index > "$RUSTTALK_DEV_DATA/soak.log" 2>&1; then
        python3 - "$RUSTTALK_DEV_DATA/soak.log" <<'PY' || status=1
import json
from pathlib import Path
import re
import sys

issues = 0
upgrades = 0
reported_admin_warning = False
log_path = Path(sys.argv[1])
finish_path = log_path.with_name("soak-finished")
finished = float(finish_path.read_text()) if finish_path.exists() else float("inf")
with log_path.open(encoding="utf-8") as logs:
    for line in logs:
        service, _, message = line.partition("|")
        service = service.strip()
        try:
            entry = json.loads(message)
        except json.JSONDecodeError:
            entry = {}
        if not isinstance(entry, dict):
            continue
        level = str(entry.get("level", "")).lower()
        # The API's only HTTP 101 is the event WebSocket upgrade.
        if service == "api-1" and entry.get("fields", {}).get("status") == 101:
            upgrades += 1
        # Plain-text lines: LiveKit's console format and Rust panics.
        if not level:
            match = re.match(r"\s*(?:\d{4}-\d\d-\d\dT\S+\s+)?(WARN(?:ING)?|ERROR|DPANIC|PANIC|FATAL)\b", message, re.I)
            level = match[1].lower() if match else "panic" if " panicked at " in message else ""
        # Caddy reports the dev Caddyfile's intentional `admin off` once at startup.
        if (not reported_admin_warning and service == "caddy-1" and level == "warn"
                and entry.get("logger") == "admin" and entry.get("msg") == "admin endpoint disabled"):
            print(f"Excluded expected Caddy startup warning: {line.rstrip()}")
            reported_admin_warning = True
            continue
        # LiveKit logs Pion's local data-channel abort when the bots deliberately leave.
        if (service == "livekit-1" and level == "warn" and entry.get("ts", 0) >= finished
                and entry.get("logger") == "livekit.transport"
                and entry.get("msg") == "error reading data channel"
                and entry.get("label") in {"_reliable", "_lossy", "_data_track"}
                and entry.get("error", "").endswith(
                    "abort chunk, with following errors: user initiated abort: (User Initiated Abort: Close called)"
                )):
            print(f"Excluded expected LiveKit room exit warning: {line.rstrip()}")
            continue
        # LiveKit can race a signal write against the leaving bot's close frame.
        if (service == "livekit-1" and level == "warn" and entry.get("ts", 0) >= finished
                and entry.get("msg") == "error writing to websocket"
                and entry.get("error") == "websocket: close sent"):
            print(f"Excluded expected LiveKit room exit warning: {line.rstrip()}")
            continue
        if level in {"warn", "warning", "error", "dpanic", "panic", "fatal"}:
            print(line.rstrip(), file=sys.stderr)
            issues += 1
print(f"Soak service logs: event sockets={upgrades} warn/error lines={issues}")
if upgrades != 2:
    print(f"Expected exactly two event WebSocket upgrades, got {upgrades}", file=sys.stderr)
sys.exit(bool(issues or upgrades != 2))
PY
    else
        cat "$RUSTTALK_DEV_DATA/soak.log" >&2
        status=1
    fi
    compose down --volumes --rmi local || status=1
    rm -rf "$RUSTTALK_DEV_DATA"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

compose up --build --wait --wait-timeout 60
cargo test --locked -p rusttalk-bot --test membership -- --ignored --nocapture soak::docker_soak
