#!/usr/bin/env python3
"""Correlate probe_seek JSON lines with Kodi logs; never claim AV verification.

Usage: python3 scripts/kodi_seek_report.py PROBE_LOG KODI_LOG
Kodi timestamps are interpreted in the machine's local timezone. Run this on
the Kodi host, or set TZ to that host's timezone before running.
"""
import datetime as dt
import json
import math
import re
import sys


def report(probe_text, kodi_text):
    events = []
    for line in probe_text.splitlines():
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    lines = []
    for line in kodi_text.splitlines():
        match = re.match(r"^(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d+) .*? (.*)$", line)
        if match:
            timestamp = dt.datetime.fromisoformat(match[1]).astimezone()
            lines.append((timestamp, match[2]))
    commands = [(i, event) for i, event in enumerate(events) if event.get("event") == "seek_command"]
    rows = []
    for n, (index, command) in enumerate(commands):
        returned = dt.datetime.fromisoformat(command["wall_time"])
        started = returned - dt.timedelta(milliseconds=command["elapsed_ms"])
        end_index = commands[n + 1][0] if n + 1 < len(commands) else len(events)
        end = (dt.datetime.fromisoformat(commands[n + 1][1]["wall_time"])
               - dt.timedelta(milliseconds=commands[n + 1][1]["elapsed_ms"])) if n + 1 < len(commands) else started + dt.timedelta(seconds=15)
        observation = next((e for e in events[index + 1:end_index] if e.get("event") == "seek_observation"), {})
        row = {"target": command["target"], "command_success": command["success"],
               "source_requests": observation.get("source_requests"), "av_recovery_verified": False}
        if not command["success"]:
            rows.append(row)
            continue
        # Do not associate a stale playback resume with a seek the device did
        # not process. Also exclude the next command's recovery from this row.
        seeking = False
        for timestamp, line in lines:
            if not started <= timestamp < end:
                continue
            target = re.search(r"demuxer seek to: ([0-9.]+)", line)
            if target:
                seeking = abs(float(target[1]) / 1000 - command["target"]) < 0.01
            if not seeking:
                continue
            for marker, field in [
                ("demuxer seek to:", "demux_ready_ms"),
                ("CVideoPlayerVideo - CDVDMsg::GENERAL_RESYNC", "video_resync_log_ms"),
                ("CDVDAudio::Resume - resume audio stream", "audio_resume_log_ms"),
            ]:
                if marker in line and (field != "demux_ready_ms" or ", success" in line):
                    row.setdefault(field, round((timestamp - started).total_seconds() * 1000, 3))
        rows.append(row)
    values = sorted(row["audio_resume_log_ms"] for row in rows if "audio_resume_log_ms" in row)
    return {"measurement": "Kodi internal log milestones, NOT visible/audible recovery",
            "rows": rows, "audio_resume_log_summary": {
                "n": len(values), "missing": len(rows) - len(values),
                "p50_ms": values[math.ceil(len(values) * .50) - 1] if values else None,
                "p95_ms": values[math.ceil(len(values) * .95) - 1] if values else None,
                "max_ms": max(values) if values else None,
                "method": "nearest rank; no end-to-end pass/fail inference"}}


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    with open(sys.argv[1], encoding="utf-8") as probe, open(sys.argv[2], encoding="utf-8") as kodi:
        print(json.dumps(report(probe.read(), kodi.read()), ensure_ascii=False, indent=2))
