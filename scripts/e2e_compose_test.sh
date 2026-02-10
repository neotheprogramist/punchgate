#!/usr/bin/env bash
#
# E2E test: NAT traversal tunnel via relay + DCUtR in containers.
#
# Topology (single bridge network, mDNS disabled at compile time):
#
#   echo (python:3-alpine)         — TCP echo server on 172.28.0.20:7777
#   bootstrap (punchgate:local)    — public relay node on 172.28.0.10
#   workhorse (punchgate:local)    — NAT'd peer exposing echo service
#   client    (punchgate:local)    — tunnels echo through bootstrap → workhorse
#
# Flow:
#   1. Bootstrap starts, logs peer ID
#   2. Workhorse connects to bootstrap → relay reservation → Kademlia publish
#   3. Client connects to bootstrap → DHT lookup → dial workhorse via relay →
#      DCUtR hole punch → tunnel spawned
#   4. Test sends payload through tunnel on host port 2222
#
# Requires: podman compose
#
set -euo pipefail

# ── Compose command detection ──────────────────────────────────────────────

if ! command -v podman &>/dev/null || ! podman compose version &>/dev/null 2>&1; then
    echo "ERROR: 'podman compose' not found" >&2
    exit 1
fi
COMPOSE="podman compose"

# ── Project root ───────────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

# ── Configurable knobs ─────────────────────────────────────────────────────

READY_TIMEOUT=60
TEST_PAYLOAD="hello punchgate nat"
TUNNEL_PORT=2222

# Compose reads ${VAR} references from the process environment.
# Export placeholders so early `up` calls don't fail on unset variables.
export BOOTSTRAP_ID=""

# ── Colour helpers ─────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
RESET='\033[0m'

log()  { printf "${CYAN}[test]${RESET} %s\n" "$*"; }
pass() { printf "${GREEN}${BOLD}[PASS]${RESET} %s\n" "$*"; }
fail() { printf "${RED}${BOLD}[FAIL]${RESET} %s\n" "$*"; exit 1; }
warn() { printf "${YELLOW}[warn]${RESET} %s\n" "$*"; }

# ── Cleanup ────────────────────────────────────────────────────────────────

cleanup() {
    log "tearing down containers..."
    $COMPOSE down --volumes --remove-orphans 2>/dev/null || true
}
trap cleanup EXIT

# ── Helpers ────────────────────────────────────────────────────────────────

wait_for_container_log() {
    local service="$1"
    local pattern="$2"
    local label="$3"
    local elapsed=0

    while ! $COMPOSE logs "$service" 2>&1 | grep -q "$pattern"; do
        sleep 0.5
        elapsed=$((elapsed + 1))
        if [[ $elapsed -ge $((READY_TIMEOUT * 2)) ]]; then
            warn "timed out waiting for $label"
            warn "last 30 lines of $service:"
            $COMPOSE logs --tail 30 "$service" 2>&1 || true
            fail "timeout waiting for: $label"
        fi
    done
}

parse_peer_id() {
    local service="$1"
    $COMPOSE logs "$service" 2>&1 \
        | sed 's/\x1b\[[0-9;]*m//g' \
        | grep -o 'local_peer_id=[^ ]*' \
        | head -1 \
        | cut -d= -f2
}

# ── Step 1: Build image ───────────────────────────────────────────────────

log "building punchgate:local image (mDNS disabled)..."
$COMPOSE build bootstrap

# ── Step 2: Start echo + bootstrap ─────────────────────────────────────────

log "starting echo server and bootstrap node..."
$COMPOSE up -d echo bootstrap

wait_for_container_log echo "listening on" "echo server"
log "echo server ready"

wait_for_container_log bootstrap "listening on /ip4/" "bootstrap listening"
export BOOTSTRAP_ID=$(parse_peer_id bootstrap)

if [[ -z "$BOOTSTRAP_ID" ]]; then
    fail "could not parse bootstrap peer ID from logs"
fi

log "bootstrap peer ID = $BOOTSTRAP_ID"

# ── Step 3: Start workhorse ───────────────────────────────────────────────

log "starting workhorse (--nat-status private, --expose echo)..."
$COMPOSE up -d --no-recreate workhorse

wait_for_container_log workhorse "listening on /ip4/" "workhorse listening"

log "waiting for workhorse relay reservation..."
wait_for_container_log workhorse "relay reservation accepted" "workhorse relay reservation"

WORKHORSE_ID=$(parse_peer_id workhorse)

if [[ -z "$WORKHORSE_ID" ]]; then
    fail "could not parse workhorse peer ID from logs"
fi

log "workhorse peer ID = $WORKHORSE_ID"

# ── Step 4: Start client ──────────────────────────────────────────────────

log "starting client (--tunnel-by-name echo@0.0.0.0:$TUNNEL_PORT)..."
$COMPOSE up -d --no-recreate client

wait_for_container_log client "listening on /ip4/" "client listening"

log "waiting for client tunnel listener..."
wait_for_container_log client "tunnel listening" "client tunnel on :$TUNNEL_PORT"
log "tunnel ready"

# Extra settle time for relay/DCUtR negotiation
sleep 2

# ── Step 5: Send test payload ─────────────────────────────────────────────

log "sending test payload through tunnel (127.0.0.1:$TUNNEL_PORT)..."
# The { echo; sleep } subshell keeps stdin open so nc doesn't half-close
# the TCP connection before the multi-hop response arrives.
RESULT=$( { echo "$TEST_PAYLOAD"; sleep 2; } | nc -w 5 127.0.0.1 "$TUNNEL_PORT" 2>/dev/null || true)

# ── Step 6: Verify ────────────────────────────────────────────────────────

if [[ "$RESULT" == "$TEST_PAYLOAD" ]]; then
    pass "echo through NAT tunnel matches: \"$RESULT\""
else
    warn "expected: \"$TEST_PAYLOAD\""
    warn "got:      \"$RESULT\""
    warn ""
    warn "── echo server log ──"
    $COMPOSE logs echo 2>&1 || true
    warn ""
    warn "── bootstrap log ──"
    $COMPOSE logs --tail 30 bootstrap 2>&1 || true
    warn ""
    warn "── workhorse log ──"
    $COMPOSE logs --tail 30 workhorse 2>&1 || true
    warn ""
    warn "── client log ──"
    $COMPOSE logs --tail 30 client 2>&1 || true
    fail "tunnel echo mismatch"
fi

log "data path: nc → :$TUNNEL_PORT → client (--tunnel-by-name) → bootstrap relay → workhorse → echo → back"
log "done."
