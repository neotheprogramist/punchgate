#!/usr/bin/env python3
"""Analyze ping RTT latency from punchgate structured logs.

Parses 'ping' and 'ping failed' events to compute:
- Per-peer RTT stats (min, mean, median, p95, p99, max)
- Ping failure count per peer
- Unhealthy peers (high failure rate or high RTT)

Usage:
    python scripts/log_latency.py              # reads logs/ directory
    python scripts/log_latency.py logfile.txt  # reads specific file
    python scripts/log_latency.py --csv        # CSV output from logs/
    punchgate 2>&1 | python scripts/log_latency.py  # reads stdin
"""

from __future__ import annotations

import csv
import io
import re
import sys
from collections import defaultdict
from dataclasses import dataclass, field

from log_parse import parse_log_lines, resolve_input, tee_to_logs

# Parse durations like "150.123ms", "1.5s", "200µs", "200us"
DURATION_RE = re.compile(r"([\d.]+)(µs|us|ms|s)")

UNHEALTHY_FAILURE_RATE = 0.1  # 10% failure rate
UNHEALTHY_RTT_MS = 500.0  # 500ms mean RTT


def parse_rtt_ms(rtt_str: str) -> float | None:
    """Parse a tracing debug-formatted Duration into milliseconds."""
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


@dataclass
class PeerLatency:
    rtts_ms: list[float] = field(default_factory=list)
    failures: int = 0

    @property
    def count(self) -> int:
        return len(self.rtts_ms)

    @property
    def total(self) -> int:
        return self.count + self.failures

    @property
    def failure_rate(self) -> float:
        return self.failures / max(self.total, 1)

    def percentile(self, p: float) -> float:
        if not self.rtts_ms:
            return 0.0
        sorted_rtts = sorted(self.rtts_ms)
        idx = int(len(sorted_rtts) * p / 100.0)
        idx = min(idx, len(sorted_rtts) - 1)
        return sorted_rtts[idx]

    @property
    def mean(self) -> float:
        return sum(self.rtts_ms) / max(len(self.rtts_ms), 1)

    @property
    def median(self) -> float:
        return self.percentile(50)

    @property
    def min_rtt(self) -> float:
        return min(self.rtts_ms) if self.rtts_ms else 0.0

    @property
    def max_rtt(self) -> float:
        return max(self.rtts_ms) if self.rtts_ms else 0.0

    @property
    def is_unhealthy(self) -> bool:
        return (
            self.failure_rate > UNHEALTHY_FAILURE_RATE
            or (self.rtts_ms and self.mean > UNHEALTHY_RTT_MS)
        )


def analyze(lines: list[str]) -> dict[str, PeerLatency]:
    peers: dict[str, PeerLatency] = defaultdict(PeerLatency)
    for entry in parse_log_lines(lines):
        if entry.message == "ping":
            peer = entry.fields.get("peer", "unknown")
            rtt_str = entry.fields.get("rtt", "")
            rtt_ms = parse_rtt_ms(rtt_str)
            if rtt_ms is not None:
                peers[peer].rtts_ms.append(rtt_ms)
        elif entry.message == "ping failed":
            peer = entry.fields.get("peer", "unknown")
            peers[peer].failures += 1
    return dict(peers)


def short_peer(peer: str) -> str:
    return peer[:16] + "..." if len(peer) > 19 else peer


def fmt_ms(ms: float) -> str:
    if ms < 1.0:
        return f"{ms * 1000:.0f}µs"
    if ms < 1000.0:
        return f"{ms:.1f}ms"
    return f"{ms / 1000:.2f}s"


def print_text(peers: dict[str, PeerLatency]) -> None:
    print("=" * 80)
    print("PING LATENCY ANALYSIS")
    print("=" * 80)

    if not peers:
        print("No ping events found.")
        return

    total_pings = sum(p.count for p in peers.values())
    total_failures = sum(p.failures for p in peers.values())
    print(f"\n  Total pings:    {total_pings}")
    print(f"  Total failures: {total_failures}")
    print(f"  Peers measured: {len(peers)}")

    print(
        f"\n{'Peer':<22} {'Pings':>6} {'Fail':>5} {'Min':>8} "
        f"{'Mean':>8} {'Med':>8} {'P95':>8} {'P99':>8} {'Max':>8} {'Health':>8}"
    )
    print("-" * 100)

    for peer in sorted(peers, key=lambda p: peers[p].mean, reverse=True):
        lat = peers[peer]
        health = "BAD" if lat.is_unhealthy else "ok"
        print(
            f"  {short_peer(peer):<20} {lat.count:>6} {lat.failures:>5} "
            f"{fmt_ms(lat.min_rtt):>8} {fmt_ms(lat.mean):>8} {fmt_ms(lat.median):>8} "
            f"{fmt_ms(lat.percentile(95)):>8} {fmt_ms(lat.percentile(99)):>8} "
            f"{fmt_ms(lat.max_rtt):>8} {health:>8}"
        )

    unhealthy = [p for p, lat in peers.items() if lat.is_unhealthy]
    if unhealthy:
        print(f"\nUnhealthy peers ({len(unhealthy)}):")
        for peer in unhealthy:
            lat = peers[peer]
            reasons = []
            if lat.failure_rate > UNHEALTHY_FAILURE_RATE:
                reasons.append(f"failure rate {lat.failure_rate:.0%}")
            if lat.rtts_ms and lat.mean > UNHEALTHY_RTT_MS:
                reasons.append(f"mean RTT {fmt_ms(lat.mean)}")
            print(f"  {short_peer(peer)}: {', '.join(reasons)}")

    print()


def print_csv(peers: dict[str, PeerLatency]) -> None:
    output = io.StringIO()
    writer = csv.writer(output)
    writer.writerow(["peer", "pings", "failures", "min_ms", "mean_ms", "median_ms", "p95_ms", "p99_ms", "max_ms"])
    for peer in sorted(peers):
        lat = peers[peer]
        writer.writerow([
            peer, lat.count, lat.failures,
            f"{lat.min_rtt:.2f}", f"{lat.mean:.2f}", f"{lat.median:.2f}",
            f"{lat.percentile(95):.2f}", f"{lat.percentile(99):.2f}", f"{lat.max_rtt:.2f}",
        ])
    print(output.getvalue(), end="")


def main() -> None:
    lines = resolve_input(sys.argv, flag_args=("--csv",))
    peers = analyze(lines)
    use_csv = "--csv" in sys.argv

    with tee_to_logs("latency", "csv" if use_csv else "txt"):
        if use_csv:
            print_csv(peers)
        else:
            print_text(peers)


if __name__ == "__main__":
    main()
