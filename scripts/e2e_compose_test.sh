#!/usr/bin/env bash
#
# E2E test: tunnel an echo server through full-cone NAT gateways.
#
# Topology (internal bridge networks with NAT gateway containers):
#
#   public    (10.100.0.0/24) — bootstrap, nat-a, nat-b
#   lan-a     (10.0.1.0/24, internal) — nat-a, echo, workhorse
#   lan-b     (10.0.2.0/24, internal) — nat-b, client, test-probe
#
#   nat-a/nat-b simulate full-cone NAT:
#     - SNAT with fixed port for QUIC outbound (preserves port 4001)
#     - DNAT for inbound UDP 4001 (forwards to LAN peer)
#     - MASQUERADE for other outbound traffic
#     - Bridge gateway blocking prevents cross-bridge bypass
#
#   Full-cone NAT allows direct connections through the NAT, so DCUtR
#   hole-punching is not exercised. Linux conntrack cannot properly handle
#   UDP simultaneous open in a same-host container environment (the dialer's
#   packet creates a conntrack INPUT entry that collides with the listener's
#   SNAT reply tuple). Full-cone DNAT sidesteps this limitation.
#
# Data path verified:
#   test-probe → client:2222 → nat-b → public → nat-a → workhorse → echo
#                echo → workhorse → nat-a → public → nat-b → client → test-probe
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

READY_TIMEOUT=90
TEST_PAYLOAD="hello punchgate nat"

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

log "building punchgate image (mDNS disabled)..."
$COMPOSE build

# ── Step 2: Start NAT gateways + echo + bootstrap ────────────────────────

log "starting NAT gateways, echo server, and bootstrap node..."
$COMPOSE up -d nat-a nat-b echo bootstrap

wait_for_container_log nat-a "NAT gateway ready" "nat-a gateway"
log "nat-a gateway ready"
wait_for_container_log nat-b "NAT gateway ready" "nat-b gateway"
log "nat-b gateway ready"

wait_for_container_log echo "listening on" "echo server"
log "echo server ready"

wait_for_container_log bootstrap "listening on /ip4/" "bootstrap listening"
export BOOTSTRAP_ID=$(parse_peer_id bootstrap)

if [[ -z "$BOOTSTRAP_ID" ]]; then
    fail "could not parse bootstrap peer ID from logs"
fi

log "bootstrap peer ID = $BOOTSTRAP_ID"

# ── Step 3: Start workhorse ───────────────────────────────────────────────

log "starting workhorse (--expose echo, routes via nat-a)..."
$COMPOSE up -d --no-recreate workhorse

wait_for_container_log workhorse "listening on /ip4/" "workhorse listening"

log "waiting for workhorse relay reservation..."
wait_for_container_log workhorse "relay reservation accepted" "workhorse relay reservation"

WORKHORSE_ID=$(parse_peer_id workhorse)

if [[ -z "$WORKHORSE_ID" ]]; then
    fail "could not parse workhorse peer ID from logs"
fi

log "workhorse peer ID = $WORKHORSE_ID"

# ── Step 4: Start client + test probe ────────────────────────────────────

log "starting client (--tunnel-by-name echo, routes via nat-b) and test probe..."
$COMPOSE up -d --no-recreate client test-probe

wait_for_container_log client "listening on /ip4/" "client listening"

log "waiting for client tunnel listener..."
wait_for_container_log client "tunnel listening" "client tunnel on :2222"
log "tunnel ready"

# Extra settle time for connection establishment
sleep 3

# ── Step 5: Send test payload via test-probe ─────────────────────────────

log "sending test payload through tunnel (test-probe → client:2222 → workhorse → echo)..."
RESULT=$($COMPOSE exec -T test-probe sh -c "echo '${TEST_PAYLOAD}' | nc -w 5 10.0.2.40 2222" 2>/dev/null || true)

# ── Step 6: Verify tunnel data ───────────────────────────────────────────

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

# ── Step 7: Verify NAT translation ──────────────────────────────────────

# Confirm the workhorse sees the client's NAT public IP (not internal IP),
# proving traffic traverses the NAT gateways. Strip ANSI codes first.
NAT_CONN=$($COMPOSE logs workhorse 2>&1 | sed 's/\x1b\[[0-9;]*m//g' | grep -c "endpoint=/ip4/10.100.0.12/" || true)

if [[ "$NAT_CONN" -gt 0 ]]; then
    pass "NAT translation verified: workhorse sees client via nat-b (10.100.0.12)"
else
    warn "── workhorse log (last 50 lines) ──"
    $COMPOSE logs --tail 50 workhorse 2>&1 || true
    fail "NAT translation not detected — expected connections from 10.100.0.12"
fi

# ── Step 8: Verify DCUtR hole punch ──────────────────────────────────

# Query each service separately — podman compose may not reliably combine
# multi-service log output in a single invocation.
WH_LOGS=$($COMPOSE logs workhorse 2>&1 | sed 's/\x1b\[[0-9;]*m//g')
CL_LOGS=$($COMPOSE logs client 2>&1 | sed 's/\x1b\[[0-9;]*m//g')
CLEAN_LOGS="${WH_LOGS}"$'\n'"${CL_LOGS}"
HP_SUCCESS=$(echo "$CLEAN_LOGS" | grep -c "hole punch succeeded" || true)
HP_FAILURE=$(echo "$CLEAN_LOGS" | grep -c "hole punch failed" || true)

if [[ "$HP_SUCCESS" -gt 0 ]]; then
    pass "DCUtR hole punch succeeded ($HP_SUCCESS event(s))"
elif [[ "$HP_FAILURE" -gt 0 ]]; then
    warn "DCUtR hole punch failed ($HP_FAILURE event(s))"
    warn "── client log (last 50 lines) ──"
    $COMPOSE logs --tail 50 client 2>&1 || true
    warn "── workhorse log (last 50 lines) ──"
    $COMPOSE logs --tail 50 workhorse 2>&1 || true
    fail "DCUtR hole punch failed — expected success with full-cone NAT"
else
    warn "no DCUtR events detected"
    warn "── client log (last 50 lines) ──"
    $COMPOSE logs --tail 50 client 2>&1 || true
    warn "── workhorse log (last 50 lines) ──"
    $COMPOSE logs --tail 50 workhorse 2>&1 || true
    fail "no DCUtR events — hole punching not exercised"
fi

# ── Step 9: Verify tunnel uses direct connection ─────────────────────

TUNNEL_DIRECT=$(echo "$CLEAN_LOGS" | grep "spawning tunnel" | grep -c "relayed=false" || true)

if [[ "$TUNNEL_DIRECT" -gt 0 ]]; then
    pass "tunnel spawned on direct connection (relayed=false)"
else
    TUNNEL_RELAY=$(echo "$CLEAN_LOGS" | grep "spawning tunnel" | grep -c "relayed=true" || true)
    if [[ "$TUNNEL_RELAY" -gt 0 ]]; then
        warn "tunnel spawned on relay (relayed=true) — DCUtR may have completed after tunnel setup"
    fi
    warn "── client log (last 50 lines) ──"
    $COMPOSE logs --tail 50 client 2>&1 || true
    fail "tunnel not spawned on direct connection"
fi

log "done."
