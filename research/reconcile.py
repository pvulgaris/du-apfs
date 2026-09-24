#!/usr/bin/env python3
"""Compare a scan's per-container extent union with Apple's occupied counters.

This is a control total, not another per-file oracle. The gap can include other
volumes, snapshots, metadata, unreadable files, and external attribute streams.
It cannot be labeled as snapshot size or reclaimable space.
"""

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import plistlib
import subprocess


def reconcile(scan, topology):
    mapped = scan["physical_bytes_by_container"]
    containers = []
    found = set()
    for container in topology["Containers"]:
        device = "/dev/" + container["ContainerReference"]
        if device not in mapped:
            continue
        found.add(device)
        capacity = container["CapacityCeiling"]
        free = container["CapacityFree"]
        if not 0 <= free <= capacity:
            raise ValueError(f"invalid capacity counters for {device}")
        occupied = capacity - free
        gap = occupied - mapped[device]
        containers.append(
            {
                "device": device,
                "capacity_bytes": capacity,
                "free_bytes": free,
                "occupied_bytes": occupied,
                "mapped_file_fork_bytes": mapped[device],
                "occupied_minus_mapped_bytes": gap,
                "mapped_exceeds_control": gap < 0,
                "volumes": [
                    {
                        "device": "/dev/" + volume["DeviceIdentifier"],
                        "used_bytes": volume["CapacityInUse"],
                        "quota_bytes": volume.get("CapacityQuota", 0),
                        "reserve_bytes": volume.get("CapacityReserve", 0),
                        "roles": volume.get("Roles", []),
                    }
                    for volume in container["Volumes"]
                ],
            }
        )
    return {
        "measurement": "apfs_container_capacity_minus_free",
        "atomic_with_scan": False,
        "containers": containers,
        "missing_containers": sorted(set(mapped) - found),
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("scan", type=Path)
    parser.add_argument(
        "--apfs-plist",
        type=Path,
        help="saved diskutil apfs list -plist; otherwise query now",
    )
    args = parser.parse_args()
    if args.apfs_plist:
        raw = args.apfs_plist.read_bytes()
        sampled = None  # A file's modification time is not its sample time.
    else:
        raw = subprocess.check_output(
            ["/usr/sbin/diskutil", "apfs", "list", "-plist"]
        )
        sampled = datetime.now(timezone.utc).isoformat()
    result = reconcile(json.loads(args.scan.read_text()), plistlib.loads(raw))
    result["control_sampled_at"] = sampled
    print(json.dumps(result, indent=2))
