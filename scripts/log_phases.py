#!/usr/bin/env python3
"""Analyze state machine lifecycle from punchgate structured logs.

Parses 'phase transition' and 'event processed' events to compute:
- Phase timeline (when each transition happened, triggered by what event)
- Time spent in each phase
- Event type frequency distribution
- Commands emitted per event type (average)
- Anomalies: regressions (Participating -> Discovering), repeated transitions

Usage:
    python scripts/log_phases.py              # reads logs/ directory
    python scripts/log_phases.py logfile.txt  # reads specific file
    python scripts/log_phases.py --json       # JSON output from logs/
    punchgate 2>&1 | python scripts/log_phases.py  # reads stdin
"""

from __future__ import annotations

import json
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from datetime import datetime, timedelta

from log_parse import parse_log_lines, resolve_input, tee_to_logs


@dataclass
class PhaseTransition:
    timestamp: datetime
    event: str
    from_phase: str
    to_phase: str
    commands: int


@dataclass
class EventStats:
    count: int = 0
    total_commands: int = 0

    @property
    def avg_commands(self) -> float:
        return self.total_commands / max(self.count, 1)


@dataclass
class PhaseAnalysis:
    transitions: list[PhaseTransition] = field(default_factory=list)
    event_stats: dict[str, EventStats] = field(default_factory=lambda: defaultdict(EventStats))
    phase_durations: dict[str, timedelta] = field(default_factory=lambda: defaultdict(timedelta))
    anomalies: list[str] = field(default_factory=list)

    def add_transition(self, t: PhaseTransition) -> None:
        self.transitions.append(t)

        # Check for regressions
        regression_pairs = {("Participating", "Discovering")}
        if (t.from_phase, t.to_phase) in regression_pairs:
            self.anomalies.append(
                f"{t.timestamp.strftime('%H:%M:%S')} REGRESSION: {t.from_phase} -> {t.to_phase} "
                f"(triggered by {t.event})"
            )

    def compute_durations(self) -> None:
        if len(self.transitions) < 2:
            return
        for i in range(len(self.transitions) - 1):
            phase = self.transitions[i].to_phase
            duration = self.transitions[i + 1].timestamp - self.transitions[i].timestamp
            self.phase_durations[phase] += duration


REGRESSIONS = {("Participating", "Discovering")}


def analyze(lines: list[str]) -> PhaseAnalysis:
    analysis = PhaseAnalysis()

    for entry in parse_log_lines(lines):
        if entry.message == "phase transition":
            event = entry.fields.get("event", "unknown")
            from_phase = entry.fields.get("from", "?")
            to_phase = entry.fields.get("to", "?")
            commands = int(entry.fields.get("commands", "0"))
            analysis.add_transition(PhaseTransition(
                timestamp=entry.timestamp,
                event=event,
                from_phase=from_phase,
                to_phase=to_phase,
                commands=commands,
            ))
        elif entry.message == "event processed":
            event = entry.fields.get("event", "unknown")
            commands = int(entry.fields.get("commands", "0"))
            stats = analysis.event_stats[event]
            stats.count += 1
            stats.total_commands += commands

    analysis.compute_durations()
    return analysis


def format_duration(td: timedelta) -> str:
    total_secs = td.total_seconds()
    if total_secs < 1.0:
        return f"{total_secs * 1000:.0f}ms"
    if total_secs < 60:
        return f"{total_secs:.1f}s"
    if total_secs < 3600:
        return f"{int(total_secs) // 60}m {int(total_secs) % 60}s"
    return f"{int(total_secs) // 3600}h {(int(total_secs) % 3600) // 60}m"


def print_text(analysis: PhaseAnalysis) -> None:
    print("=" * 70)
    print("STATE MACHINE LIFECYCLE")
    print("=" * 70)

    # Phase timeline
    if analysis.transitions:
        print("\nPHASE TIMELINE")
        print("-" * 60)
        for t in analysis.transitions:
            print(
                f"  {t.timestamp.strftime('%H:%M:%S.%f')[:15]}  "
                f"{t.from_phase:<16} -> {t.to_phase:<16}  "
                f"({t.event}, {t.commands} cmds)"
            )
    else:
        print("\nNo phase transitions found.")

    # Phase durations
    if analysis.phase_durations:
        print(f"\nPHASE DURATIONS")
        print("-" * 40)
        for phase, duration in sorted(analysis.phase_durations.items()):
            print(f"  {phase:<20} {format_duration(duration):>12}")

    # Event frequency
    if analysis.event_stats:
        print(f"\nEVENT FREQUENCY")
        print(f"{'Event':<30} {'Count':>7} {'Avg Cmds':>10}")
        print("-" * 50)
        for event in sorted(analysis.event_stats, key=lambda e: analysis.event_stats[e].count, reverse=True):
            stats = analysis.event_stats[event]
            print(f"  {event:<28} {stats.count:>7} {stats.avg_commands:>10.1f}")

    # Anomalies
    if analysis.anomalies:
        print(f"\nANOMALIES ({len(analysis.anomalies)})")
        print("-" * 60)
        for anomaly in analysis.anomalies:
            print(f"  {anomaly}")

    print()


def print_json(analysis: PhaseAnalysis) -> None:
    result = {
        "transitions": [
            {
                "timestamp": t.timestamp.isoformat(),
                "event": t.event,
                "from": t.from_phase,
                "to": t.to_phase,
                "commands": t.commands,
            }
            for t in analysis.transitions
        ],
        "phase_durations": {
            phase: dur.total_seconds()
            for phase, dur in analysis.phase_durations.items()
        },
        "event_stats": {
            event: {"count": s.count, "avg_commands": s.avg_commands}
            for event, s in analysis.event_stats.items()
        },
        "anomalies": analysis.anomalies,
    }
    print(json.dumps(result, indent=2))


def main() -> None:
    lines = resolve_input(sys.argv, flag_args=("--json",))
    analysis = analyze(lines)
    use_json = "--json" in sys.argv

    with tee_to_logs("phases", "json" if use_json else "txt"):
        if use_json:
            print_json(analysis)
        else:
            print_text(analysis)


if __name__ == "__main__":
    main()
