#!/usr/bin/env python3
"""Analyze peer connection topology from punchgate structured logs.

Parses 'connection opened' and 'connection closed' events to compute:
- Per-peer connection timeline
- Connection duration per peer
- Peak / minimum connection count
- Connection churn rate (opens + closes per minute)

Usage:
    python scripts/log_connections.py              # reads logs/ directory
    python scripts/log_connections.py logfile.txt  # reads specific file
    python scripts/log_connections.py --json       # JSON output from logs/
    punchgate 2>&1 | python scripts/log_connections.py  # reads stdin
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta

from log_parse import parse_log_lines, resolve_input, tee_to_logs


@dataclass
class PeerSession:
    opened_at: datetime
    closed_at: datetime | None = None

    @property
    def duration(self) -> timedelta | None:
        if self.closed_at is None:
            return None
        return self.closed_at - self.opened_at


@dataclass
class ConnectionState:
    sessions: dict[str, list[PeerSession]] = field(default_factory=lambda: defaultdict(list))
    current_count: int = 0
    peak_count: int = 0
    min_count: int = 0
    events_per_minute: dict[str, int] = field(default_factory=lambda: defaultdict(int))

    def open(self, peer: str, ts: datetime) -> None:
        self.sessions[peer].append(PeerSession(opened_at=ts))
        self.current_count += 1
        self.peak_count = max(self.peak_count, self.current_count)
        minute_key = ts.strftime("%Y-%m-%dT%H:%M")
        self.events_per_minute[minute_key] += 1

    def close(self, peer: str, ts: datetime) -> None:
        for session in reversed(self.sessions[peer]):
            if session.closed_at is None:
                session.closed_at = ts
                break
        self.current_count = max(0, self.current_count - 1)
        self.min_count = min(self.min_count, self.current_count)
        minute_key = ts.strftime("%Y-%m-%dT%H:%M")
        self.events_per_minute[minute_key] += 1


def analyze(lines: list[str]) -> ConnectionState:
    state = ConnectionState()
    for entry in parse_log_lines(lines):
        if entry.message == "connection opened":
            peer = entry.fields.get("peer", "unknown")
            state.open(peer, entry.timestamp)
        elif entry.message == "connection closed":
            peer = entry.fields.get("peer", "unknown")
            state.close(peer, entry.timestamp)
    return state


def format_duration(td: timedelta | None) -> str:
    if td is None:
        return "ongoing"
    total_secs = int(td.total_seconds())
    if total_secs < 60:
        return f"{total_secs}s"
    if total_secs < 3600:
        return f"{total_secs // 60}m {total_secs % 60}s"
    return f"{total_secs // 3600}h {(total_secs % 3600) // 60}m"


def short_peer(peer: str) -> str:
    return peer[:16] + "..." if len(peer) > 19 else peer


def print_text(state: ConnectionState) -> None:
    print("=" * 70)
    print("CONNECTION TOPOLOGY")
    print("=" * 70)

    if not state.sessions:
        print("No connection events found.")
        return

    print(f"\n  Peak connections:    {state.peak_count}")
    print(f"  Current connections: {state.current_count}")

    if state.events_per_minute:
        total_events = sum(state.events_per_minute.values())
        minutes = len(state.events_per_minute)
        churn = total_events / max(minutes, 1)
        print(f"  Churn rate:          {churn:.1f} events/min")

    print(f"\n{'Peer':<22} {'Sessions':>8} {'Total Duration':>16} {'Last Event':>14}")
    print("-" * 64)

    for peer, sessions in sorted(state.sessions.items()):
        total_dur = timedelta()
        for s in sessions:
            if s.duration is not None:
                total_dur += s.duration
        last_event = sessions[-1].closed_at or sessions[-1].opened_at
        print(
            f"  {short_peer(peer):<20} {len(sessions):>8} "
            f"{format_duration(total_dur if any(s.duration for s in sessions) else None):>16} "
            f"{last_event.strftime('%H:%M:%S'):>14}"
        )

    print()

    # Timeline
    print("TIMELINE")
    print("-" * 64)
    all_events: list[tuple[datetime, str, str]] = []
    for peer, sessions in state.sessions.items():
        for s in sessions:
            all_events.append((s.opened_at, "OPEN ", peer))
            if s.closed_at:
                all_events.append((s.closed_at, "CLOSE", peer))
    all_events.sort(key=lambda x: x[0])
    for ts, action, peer in all_events[:50]:
        print(f"  {ts.strftime('%H:%M:%S.%f')[:15]}  {action}  {short_peer(peer)}")
    if len(all_events) > 50:
        print(f"  ... and {len(all_events) - 50} more events")
    print()


def print_json(state: ConnectionState) -> None:
    result = {
        "peak_connections": state.peak_count,
        "current_connections": state.current_count,
        "peers": {},
    }
    for peer, sessions in state.sessions.items():
        result["peers"][peer] = {
            "sessions": len(sessions),
            "durations": [
                s.duration.total_seconds() if s.duration else None for s in sessions
            ],
        }
    print(json.dumps(result, indent=2))


def main() -> None:
    lines = resolve_input(sys.argv, flag_args=("--json",))
    state = analyze(lines)
    use_json = "--json" in sys.argv

    with tee_to_logs("connections", "json" if use_json else "txt"):
        if use_json:
            print_json(state)
        else:
            print_text(state)


if __name__ == "__main__":
    main()
