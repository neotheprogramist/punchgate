#!/usr/bin/env bash
#
# E2E test: tunnel an echo server between two punchgate peers.
#
# Starts an echo server, two peers (B exposes it, A tunnels to it),
# sends a message through the tunnel, and verifies the echo.
#
# This test exercises the DIRECT LAN scenario (mDNS discovery, no NAT).
#
# ── NAT Traversal Scenario (manual test) ──────────────────────────────────────
#
# The full NAT traversal flow requires real network separation (separate
# subnets, Docker containers, or different machines). It cannot be tested
# on localhost because both peers share the same network, so there is no
# actual NAT to punch through.
#
# 3-node setup:
#   Terminal 1 (bootstrap, public IP):
#     ./punchgate --listen /ip4/0.0.0.0/tcp/4001
#
#   Terminal 2 (workhorse, behind NAT — node detects NAT status via AutoNAT):
#     ./punchgate \
#       --bootstrap /ip4/<BOOTSTRAP_IP>/tcp/4001/p2p/<BOOTSTRAP_ID> \
#       --expose ssh=127.0.0.1:22
#
#   Terminal 3 (client, behind NAT or public):
#     ./punchgate \
#       --bootstrap /ip4/<BOOTSTRAP_IP>/tcp/4001/p2p/<BOOTSTRAP_ID> \
#       --tunnel <WORKHORSE_ID>:ssh@127.0.0.1:2222
#
# Flow:
#   1. Workhorse connects to bootstrap → gets relay reservation → advertises
#      relay circuit address as external address → publishes services to DHT
#   2. Client enters Participating → Kademlia lookup for workhorse peer ID →
#      discovers workhorse's relay circuit address → dials via relay
#   3. DCUtR auto-triggers hole punch over relay circuit
#   4. On hole punch success → client spawns tunnel task → SSH works via
#      direct connection at 127.0.0.1:2222
#
# Verify: ssh -p 2222 user@127.0.0.1
# ──────────────────────────────────────────────────────────────────────────────
#
set -euo pipefail

# ── Configurable knobs ───────────────────────────────────────────────────────

ECHO_PORT=7777
TUNNEL_BIND_PORT=9000
READY_TIMEOUT=30          # seconds to wait for each component
TEST_PAYLOAD="hello punchgate"

# ── Colour helpers ───────────────────────────────────────────────────────────

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

# ── Resolve project root ────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Temp directory for keys, logs/ for persistent output ─────────────────────

TMPDIR="$(mktemp -d)"
LOGS_DIR="$PROJECT_ROOT/logs"
mkdir -p "$LOGS_DIR"

ECHO_LOG="$TMPDIR/echo.log"
PEER_B_LOG="$TMPDIR/peer-b.log"
PEER_A_LOG="$TMPDIR/peer-a.log"
PEER_B_KEY="$TMPDIR/peer-b.key"
PEER_A_KEY="$TMPDIR/peer-a.key"

# PIDs to clean up
PIDS=()

# ── Cleanup on exit ─────────────────────────────────────────────────────────

cleanup() {
    log "cleaning up..."
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done

    # Save ANSI-stripped logs to logs/ for analysis scripts
    log "saving logs to $LOGS_DIR/"
    for logfile in "$PEER_A_LOG" "$PEER_B_LOG" "$ECHO_LOG"; do
        if [[ -f "$logfile" ]]; then
            name="$(basename "$logfile")"
            perl -pe 's/\e\[[0-9;]*m//g' "$logfile" > "$LOGS_DIR/$name"
        fi
    done

    if [[ "${TEST_FAILED:-0}" == "1" ]]; then
        warn "raw logs also preserved in $TMPDIR"
        echo "  echo server : $ECHO_LOG"
        echo "  peer B      : $PEER_B_LOG"
        echo "  peer A      : $PEER_A_LOG"
    else
        rm -rf "$TMPDIR"
    fi
}
trap cleanup EXIT

# ── Helpers ──────────────────────────────────────────────────────────────────

# Wait until a pattern appears in a file, or timeout.
wait_for_log() {
    local file="$1"
    local pattern="$2"
    local label="$3"
    local elapsed=0

    while ! grep -q "$pattern" "$file" 2>/dev/null; do
        sleep 0.5
        elapsed=$((elapsed + 1))
        if [[ $elapsed -ge $((READY_TIMEOUT * 2)) ]]; then
            warn "timed out waiting for $label"
            warn "last 20 lines of $file:"
            tail -20 "$file" 2>/dev/null || true
            TEST_FAILED=1
            fail "timeout waiting for: $label"
        fi
    done
}

# ── Step 1: Build ────────────────────────────────────────────────────────────

log "building punchgate..."
cargo build -p cli --manifest-path "$PROJECT_ROOT/Cargo.toml" 2>&1
BINARY="$PROJECT_ROOT/target/debug/cli"

if [[ ! -x "$BINARY" ]]; then
    fail "binary not found at $BINARY"
fi

# ── Step 2: Start echo server ────────────────────────────────────────────────

log "starting echo server on port $ECHO_PORT..."
python3 "$PROJECT_ROOT/scripts/echo_server.py" --port "$ECHO_PORT" \
    > "$ECHO_LOG" 2>&1 &
PIDS+=($!)

wait_for_log "$ECHO_LOG" "listening on" "echo server"
log "echo server ready (pid ${PIDS[-1]})"

# ── Step 3: Start Peer B (exposes echo service) ─────────────────────────────

log "starting peer B (expose echo=127.0.0.1:$ECHO_PORT)..."
RUST_LOG="${RUST_LOG:-cli=debug}" "$BINARY" \
    --identity "$PEER_B_KEY" \
    --listen /ip4/127.0.0.1/udp/0/quic-v1 \
    --expose "echo=127.0.0.1:$ECHO_PORT" \
    > "$PEER_B_LOG" 2>&1 &
PIDS+=($!)

wait_for_log "$PEER_B_LOG" "listening on /ip4/127.0.0.1/udp/" "peer B listening"
log "peer B ready (pid ${PIDS[-1]})"

# Parse Peer B's ID and listening address (strip ANSI codes first)
PEER_B_LOG_CLEAN="$TMPDIR/peer-b-clean.log"
perl -pe 's/\e\[[0-9;]*m//g' "$PEER_B_LOG" > "$PEER_B_LOG_CLEAN"
PEER_B_ID=$(grep -ao 'local_peer_id=[^ ]*' "$PEER_B_LOG_CLEAN" | head -1 | cut -d= -f2)
PEER_B_ADDR=$(grep -ao 'listening on /ip4/127.0.0.1/udp/[0-9]*/quic-v1' "$PEER_B_LOG_CLEAN" | head -1 | sed 's/listening on //')

if [[ -z "$PEER_B_ID" || -z "$PEER_B_ADDR" ]]; then
    TEST_FAILED=1
    fail "could not parse peer B identity or address from logs"
fi

log "peer B id   = $PEER_B_ID"
log "peer B addr = $PEER_B_ADDR"

# ── Step 4: Start Peer A (tunnel to B's echo service) ───────────────────────

BOOTSTRAP="$PEER_B_ADDR/p2p/$PEER_B_ID"
TUNNEL_SPEC="$PEER_B_ID:echo@127.0.0.1:$TUNNEL_BIND_PORT"

log "starting peer A (tunnel $TUNNEL_SPEC)..."
RUST_LOG="${RUST_LOG:-cli=debug}" "$BINARY" \
    --identity "$PEER_A_KEY" \
    --listen /ip4/127.0.0.1/udp/0/quic-v1 \
    --bootstrap "$BOOTSTRAP" \
    --tunnel "$TUNNEL_SPEC" \
    > "$PEER_A_LOG" 2>&1 &
PIDS+=($!)

wait_for_log "$PEER_A_LOG" "listening on /ip4/127.0.0.1/udp/" "peer A listening"
log "peer A ready (pid ${PIDS[-1]})"

# Wait for Peer A to establish the tunnel listener
log "waiting for tunnel listener..."
wait_for_log "$PEER_A_LOG" "tunnel listening" "tunnel bind on :$TUNNEL_BIND_PORT"
log "tunnel ready"

# Give mDNS a moment to fully propagate
sleep 1

# ── Step 5: Send test data through the tunnel ────────────────────────────────

log "sending test payload through tunnel (127.0.0.1:$TUNNEL_BIND_PORT)..."
RESULT=$(echo "$TEST_PAYLOAD" | nc -w 5 127.0.0.1 "$TUNNEL_BIND_PORT" 2>/dev/null || true)

# ── Step 6: Verify ───────────────────────────────────────────────────────────

if [[ "$RESULT" == "$TEST_PAYLOAD" ]]; then
    pass "echo through tunnel matches: \"$RESULT\""
else
    TEST_FAILED=1
    warn "expected: \"$TEST_PAYLOAD\""
    warn "got:      \"$RESULT\""
    warn ""
    warn "── echo server log ──"
    cat "$ECHO_LOG" 2>/dev/null || true
    warn ""
    warn "── peer B log ──"
    tail -30 "$PEER_B_LOG" 2>/dev/null || true
    warn ""
    warn "── peer A log ──"
    tail -30 "$PEER_A_LOG" 2>/dev/null || true
    fail "tunnel echo mismatch"
fi

log "data path: nc → :$TUNNEL_BIND_PORT → Peer A → libp2p → Peer B → :$ECHO_PORT → echo → back"

# Let pings and events accumulate briefly before shutdown
sleep 2

log "done. Analyze with: python scripts/log_summary.py"
