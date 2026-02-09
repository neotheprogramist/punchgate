#!/usr/bin/env python3
"""Quick one-page summary of a single punchgate peer log.

Parses all structured events and produces a compact overview:
- Peer ID + listen addresses
- Phase progression timeline
- Unique peers seen
- Ping measurements + overall RTT mean
- Tunnel sessions + total bytes
- Errors and warnings count

Usage:
    python scripts/log_summary.py              # reads logs/ directory
    python scripts/log_summary.py logfile.txt  # reads specific file
    punchgate 2>&1 | python scripts/log_summary.py  # reads stdin
"""

from __future__ import annotations

import re
import sys
from collections import defaultdict

from log_parse import parse_log_lines, resolve_input, tee_to_logs

DURATION_RE = re.compile(r"([\d.]+)(µs|us|ms|s)")


def parse_rtt_ms(rtt_str: str) -> float | None:
    m = DURATION_RE.search(rtt_str)
    if not m:
        return None
    value = float(m.group(1))
    unit = m.group(2)
    if unit in ("µs", "us"):
        return value / 1000.0
    if unit == "ms":
        return value
    if unit == "s":
        return value * 1000.0
    return None


def fmt_bytes(n: int) -> str:
    if n < 1024:
        return f"{n}B"
    if n < 1024 * 1024:
        return f"{n / 1024:.1f}KB"
    return f"{n / (1024 * 1024):.1f}MB"


def fmt_ms(ms: float) -> str:
    if ms < 1.0:
        return f"{ms * 1000:.0f}µs"
    if ms < 1000.0:
        return f"{ms:.1f}ms"
    return f"{ms / 1000:.2f}s"


def main() -> None:
    lines = resolve_input(sys.argv)
    entries = parse_log_lines(lines)

    if not entries:
        print("No parseable log entries found.")
        return

    with tee_to_logs("summary"):
        _print_summary(entries)


def _print_summary(entries: list) -> None:
    # Extract data
    peer_id = None
    listen_addrs: list[str] = []
    phase_timeline: list[str] = []
    peers_seen: set[str] = set()
    rtts: list[float] = []
    ping_failures = 0
    tunnel_sessions = 0
    tunnel_rejected = 0
    tunnel_errors = 0
    total_bytes = 0
    warnings = 0
    errors = 0

    for entry in entries:
        if entry.level == "WARN":
            warnings += 1
        if entry.level == "ERROR":
            errors += 1

        # Peer ID from startup
        if entry.message == "starting punchgate node":
            peer_id = entry.fields.get("local_peer_id")

        # Listen addresses from state machine logs
        if "listening on" in entry.message and not entry.fields:
            addr = entry.message.replace("listening on ", "").strip()
            if addr:
                listen_addrs.append(addr)

        # Connection tracking
        if entry.message == "connection opened":
            peer = entry.fields.get("peer")
            if peer:
                peers_seen.add(peer)
        if entry.message == "connection closed":
            peer = entry.fields.get("peer")
            if peer:
                peers_seen.add(peer)

        # Phase transitions
        if entry.message == "phase transition":
            from_p = entry.fields.get("from", "?")
            to_p = entry.fields.get("to", "?")
            event = entry.fields.get("event", "?")
            ts = entry.timestamp.strftime("%H:%M:%S")
            phase_timeline.append(f"{ts} {from_p} -> {to_p} ({event})")

        # Ping
        if entry.message == "ping":
            rtt_str = entry.fields.get("rtt", "")
            rtt_ms = parse_rtt_ms(rtt_str)
            if rtt_ms is not None:
                rtts.append(rtt_ms)
        if entry.message == "ping failed":
            ping_failures += 1

        # Tunnels
        if entry.message == "tunnel accepted":
            tunnel_sessions += 1
        if entry.message in ("tunnel rejected", "client tunnel rejected"):
            tunnel_rejected += 1
        if entry.message in ("tunnel error", "client tunnel error"):
            tunnel_errors += 1
        if entry.message == "tunnel closed":
            b2s = entry.fields.get("bytes_to_service", "0")
            bfs = entry.fields.get("bytes_from_service", "0")
            try:
                total_bytes += int(b2s) + int(bfs)
            except ValueError:
                pass
        if entry.message == "client tunnel closed":
            b2r = entry.fields.get("bytes_to_remote", "0")
            bfr = entry.fields.get("bytes_from_remote", "0")
            try:
                total_bytes += int(b2r) + int(bfr)
            except ValueError:
                pass

    # Print summary
    print("=" * 60)
    print("PUNCHGATE PEER SUMMARY")
    print("=" * 60)

    if peer_id:
        print(f"\n  Peer ID: {peer_id}")
    if listen_addrs:
        print(f"  Listen addresses:")
        for addr in listen_addrs:
            print(f"    {addr}")

    # Time range
    first_ts = entries[0].timestamp.strftime("%H:%M:%S")
    last_ts = entries[-1].timestamp.strftime("%H:%M:%S")
    print(f"  Log span: {first_ts} - {last_ts} ({len(entries)} entries)")

    # Phase progression
    if phase_timeline:
        print(f"\n  Phase progression:")
        for line in phase_timeline:
            print(f"    {line}")
    else:
        print(f"\n  Phase progression: (no transitions)")

    # Peers
    print(f"\n  Unique peers seen: {len(peers_seen)}")
    if peers_seen and len(peers_seen) <= 5:
        for p in sorted(peers_seen):
            short = p[:16] + "..." if len(p) > 19 else p
            print(f"    {short}")

    # Ping
    if rtts:
        mean_rtt = sum(rtts) / len(rtts)
        print(f"\n  Ping measurements: {len(rtts)} (failures: {ping_failures})")
        print(f"  RTT mean: {fmt_ms(mean_rtt)}")
    elif ping_failures:
        print(f"\n  Ping measurements: 0 (failures: {ping_failures})")
    else:
        print(f"\n  Ping measurements: none")

    # Tunnels
    if tunnel_sessions or tunnel_rejected or tunnel_errors:
        print(f"\n  Tunnel sessions: {tunnel_sessions} accepted, {tunnel_rejected} rejected, {tunnel_errors} errors")
        print(f"  Total bytes transferred: {fmt_bytes(total_bytes)}")
    else:
        print(f"\n  Tunnel sessions: none")

    # Warnings/Errors
    print(f"\n  Warnings: {warnings}")
    print(f"  Errors:   {errors}")
    print()


if __name__ == "__main__":
    main()
