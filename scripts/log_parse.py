"""Shared parser for tracing-subscriber fmt output.

Parses lines like:
  2024-01-15T10:30:00.123456Z  INFO cli::node: connection opened peer=12D3KooW... endpoint=/ip4/...
  2024-01-15T10:30:00.123456Z  WARN cli::tunnel: tunnel error peer_id=12D3KooW... error=broken pipe

Extracts timestamp, level, target, message, and structured key=value fields.
"""

from __future__ import annotations

import contextlib
import os
import re
import sys
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

# Matches the tracing-subscriber default fmt output
# Groups: timestamp, level, target, rest (message + fields)
LINE_RE = re.compile(
    r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+Z?)\s+"
    r"(TRACE|DEBUG|INFO|WARN|ERROR)\s+"
    r"([\w:]+):\s+"
    r"(.+)"
)

# Matches key=value pairs where value may be quoted or unquoted
# Handles: peer=12D3..., rtt=150ms, error="broken pipe", bytes_to_service=1024
FIELD_RE = re.compile(
    r'(\w+)=(?:"([^"]*)"|([\S]+))'
)


@dataclass
class LogEntry:
    timestamp: datetime
    level: str
    target: str
    message: str
    fields: dict[str, str] = field(default_factory=dict)


def parse_fields(text: str) -> dict[str, str]:
    """Extract key=value pairs from the field portion of a log line."""
    fields: dict[str, str] = {}
    for match in FIELD_RE.finditer(text):
        key = match.group(1)
        value = match.group(2) if match.group(2) is not None else match.group(3)
        fields[key] = value
    return fields


def parse_line(line: str) -> LogEntry | None:
    """Parse a single tracing-subscriber fmt log line."""
    m = LINE_RE.match(line.strip())
    if not m:
        return None

    ts_str, level, target, rest = m.groups()

    # Parse timestamp — handle with or without trailing Z
    ts_str = ts_str.rstrip("Z")
    # Truncate microseconds to 6 digits if longer
    parts = ts_str.split(".")
    if len(parts) == 2 and len(parts[1]) > 6:
        ts_str = parts[0] + "." + parts[1][:6]
    try:
        timestamp = datetime.fromisoformat(ts_str)
    except ValueError:
        return None

    # Split rest into message and fields
    # The message comes first, then key=value pairs
    # Strategy: find the first key=value and split there
    field_match = FIELD_RE.search(rest)
    if field_match:
        message = rest[: field_match.start()].strip()
        fields = parse_fields(rest[field_match.start() :])
    else:
        message = rest.strip()
        fields = {}

    return LogEntry(
        timestamp=timestamp,
        level=level,
        target=target,
        message=message,
        fields=fields,
    )


def parse_log_lines(lines: list[str]) -> list[LogEntry]:
    """Parse multiple log lines, skipping unparseable ones."""
    entries: list[LogEntry] = []
    for line in lines:
        entry = parse_line(line)
        if entry is not None:
            entries.append(entry)
    return entries


def _find_logs_dir() -> Path | None:
    """Walk up from the script location to find the project logs/ directory."""
    script_dir = Path(__file__).resolve().parent
    project_root = script_dir.parent
    logs_dir = project_root / "logs"
    if logs_dir.is_dir():
        return logs_dir
    return None


def resolve_input(argv: list[str], flag_args: tuple[str, ...] = ()) -> list[str]:
    """Resolve log input from CLI args, logs/ directory, or stdin.

    Priority:
    1. Explicit file/directory path in argv (first non-flag argument)
    2. All *.log files in the project's logs/ directory
    3. stdin (when piped)

    flag_args: argument strings to skip (e.g. ("--json", "--csv"))
    """
    # Check for explicit file argument
    positional = [a for a in argv[1:] if a not in flag_args]
    if positional:
        path = Path(positional[0])
        if path.is_dir():
            log_files = sorted(path.glob("*.log"))
            if not log_files:
                print(f"No .log files found in {path}", file=sys.stderr)
                return []
            lines: list[str] = []
            for f in log_files:
                lines.extend(f.read_text().splitlines(keepends=True))
            return lines
        with open(path) as f:
            return f.readlines()

    # No explicit file — try logs/ directory first
    logs_dir = _find_logs_dir()
    if logs_dir:
        log_files = sorted(logs_dir.glob("*.log"))
        if log_files:
            names = [f.name for f in log_files]
            print(f"Reading from logs/: {', '.join(names)}", file=sys.stderr)
            lines: list[str] = []
            for f in log_files:
                lines.extend(f.read_text().splitlines(keepends=True))
            return lines

    # No logs/ — fall back to stdin if piped
    if not sys.stdin.isatty():
        return sys.stdin.readlines()

    print("No log files found. Run the e2e test or provide a log file.", file=sys.stderr)
    return []


class _TeeWriter:
    """Duplicates writes to both the original stream and a file."""

    def __init__(self, original: object, file: object) -> None:
        self._original = original
        self._file = file

    def write(self, data: str) -> int:
        self._original.write(data)
        self._file.write(data)
        return len(data)

    def flush(self) -> None:
        self._original.flush()
        self._file.flush()


@contextlib.contextmanager
def tee_to_logs(report_name: str, extension: str = "txt"):
    """Context manager that tees stdout to logs/<report_name>.<extension>.

    Output goes to both the terminal and the file. The file is written
    to the project's logs/ directory, creating it if needed.
    """
    logs_dir = _find_logs_dir()
    if not logs_dir:
        yield
        return

    logs_dir.mkdir(parents=True, exist_ok=True)
    out_path = logs_dir / f"{report_name}.{extension}"

    with open(out_path, "w") as f:
        tee = _TeeWriter(sys.stdout, f)
        old_stdout = sys.stdout
        sys.stdout = tee
        try:
            yield
        finally:
            sys.stdout = old_stdout

    print(f"Saved to {out_path.relative_to(logs_dir.parent)}", file=sys.stderr)
