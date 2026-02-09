#!/usr/bin/env python3
"""Analyze tunnel session metrics from punchgate structured logs.

Parses tunnel lifecycle events to compute:
- Total sessions (accepted vs rejected)
- Bytes transferred per service (aggregate and per-session)
- Error rate
- Per-service breakdown

Usage:
    python scripts/log_tunnels.py              # reads logs/ directory
    python scripts/log_tunnels.py logfile.txt  # reads specific file
    python scripts/log_tunnels.py --json       # JSON output from logs/
    punchgate 2>&1 | python scripts/log_tunnels.py  # reads stdin
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
from dataclasses import dataclass, field

from log_parse import parse_log_lines, resolve_input, tee_to_logs


@dataclass
class ServiceStats:
    accepted: int = 0
    rejected: int = 0
    errors: int = 0
    bytes_to_service: int = 0
    bytes_from_service: int = 0
    client_sessions: int = 0
    client_errors: int = 0
    client_rejected: int = 0
    bytes_to_remote: int = 0
    bytes_from_remote: int = 0

    @property
    def total_sessions(self) -> int:
        return self.accepted + self.rejected

    @property
    def total_bytes(self) -> int:
        return self.bytes_to_service + self.bytes_from_service + self.bytes_to_remote + self.bytes_from_remote

    @property
    def error_rate(self) -> float:
        total = self.accepted + self.client_sessions
        return (self.errors + self.client_errors) / max(total, 1)


def analyze(lines: list[str]) -> dict[str, ServiceStats]:
    services: dict[str, ServiceStats] = defaultdict(ServiceStats)

    for entry in parse_log_lines(lines):
        service = entry.fields.get("service", "unknown")

        match entry.message:
            case "tunnel accepted":
                services[service].accepted += 1
            case "tunnel rejected":
                services[service].rejected += 1
            case "tunnel closed":
                stats = services[service]
                b2s = entry.fields.get("bytes_to_service", "0")
                bfs = entry.fields.get("bytes_from_service", "0")
                try:
                    stats.bytes_to_service += int(b2s)
                    stats.bytes_from_service += int(bfs)
                except ValueError:
                    pass
            case "tunnel error":
                services[service].errors += 1
            case "client tunnel closed":
                stats = services[service]
                stats.client_sessions += 1
                b2r = entry.fields.get("bytes_to_remote", "0")
                bfr = entry.fields.get("bytes_from_remote", "0")
                try:
                    stats.bytes_to_remote += int(b2r)
                    stats.bytes_from_remote += int(bfr)
                except ValueError:
                    pass
            case "client tunnel rejected":
                services[service].client_rejected += 1
            case "client tunnel error":
                services[service].client_errors += 1

    return dict(services)


def fmt_bytes(n: int) -> str:
    if n < 1024:
        return f"{n}B"
    if n < 1024 * 1024:
        return f"{n / 1024:.1f}KB"
    if n < 1024 * 1024 * 1024:
        return f"{n / (1024 * 1024):.1f}MB"
    return f"{n / (1024 * 1024 * 1024):.1f}GB"


def print_text(services: dict[str, ServiceStats]) -> None:
    print("=" * 70)
    print("TUNNEL SESSION METRICS")
    print("=" * 70)

    if not services:
        print("No tunnel events found.")
        return

    total_accepted = sum(s.accepted for s in services.values())
    total_rejected = sum(s.rejected for s in services.values())
    total_errors = sum(s.errors + s.client_errors for s in services.values())
    total_bytes = sum(s.total_bytes for s in services.values())

    print(f"\n  Total accepted:  {total_accepted}")
    print(f"  Total rejected:  {total_rejected}")
    print(f"  Total errors:    {total_errors}")
    print(f"  Total bytes:     {fmt_bytes(total_bytes)}")

    # Server-side table
    print(f"\n{'Service':<20} {'Accept':>7} {'Reject':>7} {'Error':>6} "
          f"{'→ Service':>12} {'← Service':>12} {'Err Rate':>9}")
    print("-" * 78)

    for name in sorted(services):
        s = services[name]
        if s.accepted == 0 and s.rejected == 0 and s.errors == 0:
            continue
        print(
            f"  {name:<18} {s.accepted:>7} {s.rejected:>7} {s.errors:>6} "
            f"{fmt_bytes(s.bytes_to_service):>12} {fmt_bytes(s.bytes_from_service):>12} "
            f"{s.error_rate:>8.0%}"
        )

    # Client-side table
    has_client = any(s.client_sessions or s.client_rejected or s.client_errors for s in services.values())
    if has_client:
        print(f"\nClient-side:")
        print(f"{'Service':<20} {'Sessions':>9} {'Reject':>7} {'Error':>6} "
              f"{'→ Remote':>12} {'← Remote':>12}")
        print("-" * 70)
        for name in sorted(services):
            s = services[name]
            if s.client_sessions == 0 and s.client_rejected == 0 and s.client_errors == 0:
                continue
            print(
                f"  {name:<18} {s.client_sessions:>9} {s.client_rejected:>7} {s.client_errors:>6} "
                f"{fmt_bytes(s.bytes_to_remote):>12} {fmt_bytes(s.bytes_from_remote):>12}"
            )

    print()


def print_json(services: dict[str, ServiceStats]) -> None:
    result = {}
    for name, s in services.items():
        result[name] = {
            "server": {
                "accepted": s.accepted,
                "rejected": s.rejected,
                "errors": s.errors,
                "bytes_to_service": s.bytes_to_service,
                "bytes_from_service": s.bytes_from_service,
            },
            "client": {
                "sessions": s.client_sessions,
                "rejected": s.client_rejected,
                "errors": s.client_errors,
                "bytes_to_remote": s.bytes_to_remote,
                "bytes_from_remote": s.bytes_from_remote,
            },
            "total_bytes": s.total_bytes,
            "error_rate": s.error_rate,
        }
    print(json.dumps(result, indent=2))


def main() -> None:
    lines = resolve_input(sys.argv, flag_args=("--json",))
    services = analyze(lines)
    use_json = "--json" in sys.argv

    with tee_to_logs("tunnels", "json" if use_json else "txt"):
        if use_json:
            print_json(services)
        else:
            print_text(services)


if __name__ == "__main__":
    main()
