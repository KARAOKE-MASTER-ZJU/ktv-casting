"""Compare every encoded packet and its timing, without re-encoding.

Usage: python3 scripts/check_mp4_packets.py ORIGINAL_FILE OUTPUT_FILE_OR_URL
Requires ffprobe only on the test host; this is not an app runtime dependency.
"""
import argparse
import collections
import json
import subprocess
import time


def probe(source, timeout):
    started = time.monotonic()
    result = subprocess.run(
        ["ffprobe", "-v", "error", "-show_packets", "-show_data_hash", "sha256",
         "-show_entries", "packet=stream_index,pts,dts,duration,size,flags,data_hash",
         "-of", "json", source],
        capture_output=True, text=True, timeout=timeout, check=True,
    )
    tracks = collections.defaultdict(list)
    for packet in json.loads(result.stdout)["packets"]:
        tracks[packet.pop("stream_index")].append(packet)
    return dict(tracks), round((time.monotonic() - started) * 1000, 2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("original")
    parser.add_argument("output")
    parser.add_argument("--timeout", type=int, default=240)
    args = parser.parse_args()
    original, original_ms = probe(args.original, args.timeout)
    output, output_ms = probe(args.output, args.timeout)
    passed = bool(original) and original == output
    report = {
        "passed": passed,
        "checks": ["packet_count", "pts", "dts", "duration", "size", "flags", "sha256"],
        "original_packets": {str(k): len(v) for k, v in original.items()},
        "output_packets": {str(k): len(v) for k, v in output.items()},
        "original_probe_ms": original_ms,
        "output_probe_ms": output_ms,
        "note": "Probe duration is NOT time to resumed picture/audio on a renderer.",
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
