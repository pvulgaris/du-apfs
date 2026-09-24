#!/usr/bin/env python3
"""Rotate scanner binaries, retaining all output before checking measurements."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


def measurements(report):
    coverage = report["coverage"]
    return {
        "physical_bytes": report["total_physical_bytes"],
        "files": report["files_scanned"],
        "errors": coverage["error_count"],
        "error_counts": coverage["error_counts"],
        "unmeasured_bytes": coverage["unmeasured_bytes"],
        "unknown_attributes": coverage["unmapped_xattr_entries"],
        "unreadable_directories": coverage["unreadable_directories"],
    }


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", required=True)
    parser.add_argument(
        "--output",
        type=Path,
        required=True,
        help="new private artifact directory",
    )
    parser.add_argument("--rounds", type=int, default=4)
    parser.add_argument(
        "--scan-arg",
        action="append",
        help="scanner option, repeated; defaults to -s, JSON is always enabled",
    )
    parser.add_argument(
        "--expected", type=Path, help="previously verified scanner JSON"
    )
    parser.add_argument(
        "--allow-changing-counters",
        action="store_true",
        help="live throughput diagnostic; not a parity check",
    )
    parser.add_argument("variants", nargs="+", metavar="LABEL=BINARY")
    args = parser.parse_args()
    scan_args = args.scan_arg if args.scan_arg is not None else ["-s"]
    if args.rounds < 1:
        parser.error("rounds must be positive")
    variants = []
    for value in args.variants:
        label, separator, binary = value.partition("=")
        if not separator or not re.fullmatch(r"[A-Za-z0-9_-]+", label):
            parser.error(
                "each variant must be LABEL=BINARY with a simple label"
            )
        if any(v["label"] == label for v in variants):
            parser.error("variant labels must be distinct")
        path = Path(binary).resolve(strict=True)
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error("variant must be an executable regular file")
        variants.append(
            {"label": label, "binary": str(path), "sha256": digest(path)}
        )
    expected = (
        measurements(json.loads(args.expected.read_text()))
        if args.expected
        else None
    )
    args.output.mkdir(mode=0o700)
    record = {
        "root": args.root,
        "variants": variants,
        "rounds": args.rounds,
        "scan_arguments": scan_args,
        "require_matching_counters": not args.allow_changing_counters,
        "expected": expected,
        "darwin_release": os.uname().release,
        "architecture": os.uname().machine,
        "runs": [],
    }

    def save():
        (args.output / "results.json").write_text(
            json.dumps(record, indent=2) + "\n"
        )

    save()
    for round_number in range(args.rounds):
        offset = round_number % len(variants)
        for variant in variants[offset:] + variants[:offset]:
            stem = "%02d-%s" % (round_number, variant["label"])
            stdout_path = args.output / (stem + ".json")
            stderr_path = args.output / (stem + ".stderr")
            command = [
                "/usr/bin/time",
                "-lp",
                variant["binary"],
                *scan_args,
                "--json",
                args.root,
            ]
            row = {
                "round": round_number,
                "variant": variant["label"],
                "command": command,
                "stdout": stdout_path.name,
                "stderr": stderr_path.name,
                "status": "running",
            }
            record["runs"].append(row)
            save()
            # Stream to files so a mismatch, crash or interrupted harness cannot
            # discard the per-file diagnostics needed to explain the failure.
            with (
                stdout_path.open("wb") as stdout,
                stderr_path.open("wb") as stderr,
            ):
                result = subprocess.run(command, stdout=stdout, stderr=stderr)
            row["exit_status"] = result.returncode
            try:
                report = json.loads(stdout_path.read_text())
                row["measurements"] = measurements(report)
                row["counters_match"] = (
                    expected is None or row["measurements"] == expected
                )
                if result.returncode not in (0, 1):
                    raise ValueError(
                        "scanner failed with status %d" % result.returncode
                    )
                if (result.returncode == 0) != report["coverage"]["complete"]:
                    raise ValueError(
                        "exit status disagrees with reported coverage"
                    )
                if (
                    not row["counters_match"]
                    and not args.allow_changing_counters
                ):
                    raise ValueError(
                        "measurements differ; inspect retained JSON and stderr"
                    )
                timing = stderr_path.read_text()
                row.update(
                    {
                        key: float(value)
                        for key, value in re.findall(
                            r"^(real|user|sys)\s+([0-9.]+)", timing, re.M
                        )
                    }
                )
                if not all(key in row for key in ("real", "user", "sys")):
                    raise ValueError("missing native timing fields")
                row["peak_rss_bytes"] = int(
                    re.search(r"(\d+)\s+maximum resident set size", timing)[1]
                )
                if expected is None:
                    expected = row["measurements"]
                    record["expected"] = expected
                if digest(Path(variant["binary"])) != variant["sha256"]:
                    raise ValueError("binary changed during the experiment")
                row["status"] = "complete"
            except Exception as error:
                row["status"] = "failed"
                row["failure"] = str(error)
                save()
                raise
            save()
            print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
