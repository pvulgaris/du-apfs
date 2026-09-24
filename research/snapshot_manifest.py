#!/usr/bin/env python3
"""Record visible inodes and bind them to a mounted read-only APFS snapshot."""

import argparse
import ctypes
import json
import os
from pathlib import Path
import plistlib
import stat
import struct
import subprocess
import time

MNT_RDONLY = 0x00000001
MNT_SNAPSHOT = 0x40000000


class StatFS(ctypes.Structure):
    # macOS SDK sys/mount.h: __DARWIN_STRUCT_STATFS64.
    _fields_ = [
        ("bsize", ctypes.c_uint32),
        ("iosize", ctypes.c_int32),
        ("blocks", ctypes.c_uint64),
        ("bfree", ctypes.c_uint64),
        ("bavail", ctypes.c_uint64),
        ("files", ctypes.c_uint64),
        ("ffree", ctypes.c_uint64),
        ("fsid", ctypes.c_int32 * 2),
        ("owner", ctypes.c_uint32),
        ("type", ctypes.c_uint32),
        ("flags", ctypes.c_uint32),
        ("subtype", ctypes.c_uint32),
        ("fstype", ctypes.c_char * 16),
        ("mnton", ctypes.c_char * 1024),
        ("mntfrom", ctypes.c_char * 1024),
        ("flags_ext", ctypes.c_uint32),
        ("reserved", ctypes.c_uint32 * 7),
    ]


def statfs(path):
    libc = ctypes.CDLL(None, use_errno=True)
    libc.statfs.argtypes = [ctypes.c_char_p, ctypes.POINTER(StatFS)]
    libc.statfs.restype = ctypes.c_int
    result = StatFS()
    if libc.statfs(os.fsencode(path), ctypes.byref(result)):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), path)
    return result


def diskutil(*args):
    command = (
        ["/usr/sbin/diskutil", args[0], "-plist", *args[1:]]
        if args[0] == "info"
        else ["/usr/sbin/diskutil", *args, "-plist"]
    )
    process = subprocess.run(
        command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False
    )
    if process.returncode:
        raise ValueError(
            f"diskutil {args[0]} failed: {process.stderr.decode(errors='replace').strip()}"
        )
    result = plistlib.loads(process.stdout)
    if result.get("Error"):
        raise ValueError(
            f"diskutil {args[0]} failed: {result.get('ErrorMessage')}"
        )
    return result


def mount_identity(root):
    info = statfs(root)
    mountpoint = os.fsdecode(info.mnton)
    source = os.fsdecode(info.mntfrom)
    if mountpoint != root or info.fstype != b"apfs":
        raise ValueError("root must be an APFS mount point")
    if not info.flags & MNT_RDONLY or not info.flags & MNT_SNAPSHOT:
        raise ValueError("root must be a read-only APFS snapshot mount")

    # `diskutil info` cannot resolve a direct `mount_apfs -s` mount path on
    # macOS 26. In that case statfs still identifies its snapshot and base disk.
    snapshot_info = None
    try:
        snapshot_info = diskutil("info", root)
    except ValueError:
        pass
    name_from_mount, separator, base_device = source.rpartition("@/dev/")
    if separator:
        base_device = "/dev/" + base_device
        base_info = diskutil("info", base_device)
        if base_info.get("DeviceNode") != base_device or base_info.get(
            "APFSSnapshot"
        ):
            raise ValueError(
                "mount source does not resolve to a base APFS volume"
            )
        if base_info.get("FilesystemType") != "apfs":
            raise ValueError("mount source base is not APFS")
        base_mount = base_info.get("MountPoint")
        if not base_mount:
            raise ValueError(
                "base APFS volume must be mounted for snapshot listing"
            )
        listings = diskutil("apfs", "listSnapshots", base_mount)["Snapshots"]
        matches = [
            item
            for item in listings
            if item.get("SnapshotName") == name_from_mount
        ]
        if len(matches) != 1:
            raise ValueError(
                "mount source snapshot name is absent or ambiguous"
            )
        selected = matches[0]
        if snapshot_info and snapshot_info.get("APFSSnapshot"):
            if (
                snapshot_info.get("APFSSnapshotUUID")
                != selected["SnapshotUUID"]
                or snapshot_info.get("APFSSnapshotName")
                != selected["SnapshotName"]
            ):
                raise ValueError("snapshot mount and diskutil listing disagree")
        container = base_info["APFSContainerReference"]
        group = base_info.get("APFSVolumeGroupID")
        volume_uuid = base_info["VolumeUUID"]
        base_identifier = base_info["DeviceIdentifier"]
        uuid_direct_from_mount = bool(
            snapshot_info and snapshot_info.get("APFSSnapshot")
        )
    else:
        if not snapshot_info or not snapshot_info.get("APFSSnapshot"):
            raise ValueError("snapshot mount source cannot be resolved")
        selected_name = snapshot_info["APFSSnapshotName"]
        listings = diskutil("apfs", "listSnapshots", root)["Snapshots"]
        matches = [
            item
            for item in listings
            if item.get("SnapshotName") == selected_name
            and item.get("SnapshotUUID") == snapshot_info["APFSSnapshotUUID"]
        ]
        if len(matches) != 1:
            raise ValueError(
                "snapshot mount is absent or ambiguous in diskutil listing"
            )
        selected = matches[0]
        container = snapshot_info["APFSContainerReference"]
        group = snapshot_info.get("APFSVolumeGroupID")
        # The snapshot's VolumeUUID is its own UUID, not its base volume's.
        topology = diskutil("apfs", "list")["Containers"]
        candidates = [
            volume
            for item in topology
            if item.get("ContainerReference") == container
            for volume in item.get("Volumes", [])
            if volume.get("Name") == snapshot_info.get("VolumeName")
            and volume.get("APFSVolumeUUID") != selected["SnapshotUUID"]
        ]
        if len(candidates) != 1:
            raise ValueError("cannot uniquely resolve base APFS volume")
        volume_uuid = candidates[0]["APFSVolumeUUID"]
        base_identifier = candidates[0]["DeviceIdentifier"]
        uuid_direct_from_mount = True
    if not selected.get("SnapshotUUID") or not isinstance(
        selected.get("SnapshotXID"), int
    ):
        raise ValueError("snapshot listing lacks UUID or numeric XID")
    matching_containers = [
        item
        for item in diskutil("apfs", "list")["Containers"]
        if item.get("ContainerReference") == container
    ]
    if len(matching_containers) != 1 or not matching_containers[0].get(
        "APFSContainerUUID"
    ):
        raise ValueError("cannot uniquely resolve APFS container UUID")
    container_uuid = matching_containers[0]["APFSContainerUUID"]
    return {
        "snapshot_name": selected["SnapshotName"],
        "snapshot_uuid": selected["SnapshotUUID"].upper(),
        "snapshot_xid": selected["SnapshotXID"],
        "snapshot_uuid_direct_from_mount": uuid_direct_from_mount,
        "base_volume_uuid": volume_uuid.upper(),
        "base_device_identifier": base_identifier,
        "container_reference": container,
        "container_uuid": container_uuid.upper(),
        "volume_group_uuid": group,
        "mount_source": source,
        "mount_flags": info.flags,
        "mount_point": mountpoint,
    }


class VolumeIdentity:
    def __init__(self):
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.libc.getattrlist.argtypes = [
            ctypes.c_char_p,
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_size_t,
            ctypes.c_ulong,
        ]
        self.libc.getattrlist.restype = ctypes.c_int
        self.request = ctypes.create_string_buffer(
            struct.pack("<HHIIIII", 5, 0, 0x80000000, 0, 0, 0, 0x40)
        )
        self.reply = ctypes.create_string_buffer(28)

    def device(self, path):
        ctypes.memset(self.reply, 0, len(self.reply))
        if self.libc.getattrlist(
            os.fsencode(path), self.request, self.reply, 28, 0x21
        ):
            error = ctypes.get_errno()
            raise OSError(error, os.strerror(error), path)
        if not struct.unpack_from("I", self.reply, 20)[0] & 0x40:
            return None
        return struct.unpack_from("I", self.reply, 24)[0]


def manifest(root):
    if not stat.S_ISDIR(os.lstat(root).st_mode):
        raise ValueError("root must be a directory, not a symlink")
    before = mount_identity(root)
    identity = VolumeIdentity()
    device = identity.device(root)
    if device is None:
        raise ValueError("root does not expose a real device ID")
    stack, rows, skipped, errors, seen = [root], [], [], [], set()
    start = time.monotonic()
    while stack:
        path = stack.pop()
        try:
            current = identity.device(path)
            if current != device:
                skipped.append({"path": path, "device": current})
                continue
            metadata = os.lstat(path)
            relative = os.path.relpath(path, root)
            rows.append(
                {
                    "path": "" if relative == "." else relative,
                    "inode": metadata.st_ino,
                    "size": metadata.st_size,
                    "allocated": metadata.st_blocks * 512,
                    "mode": metadata.st_mode,
                }
            )
            if stat.S_ISDIR(metadata.st_mode):
                key = (current, metadata.st_ino)
                if key in seen:
                    raise ValueError(
                        f"directory alias requires explicit handling: {path}"
                    )
                seen.add(key)
                with os.scandir(path) as entries:
                    stack.extend(entry.path for entry in entries)
        except OSError as error:
            errors.append(str(error))
    after = mount_identity(root)
    if before != after or identity.device(root) != device:
        raise ValueError("snapshot mount identity changed during traversal")
    return rows, {
        "root": root,
        "real_device": device,
        "identity": before,
        "elapsed": time.monotonic() - start,
        "entries": len(rows),
        "skipped": skipped,
        "errors": errors,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root")
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    rows, scope = manifest(os.path.abspath(args.root))
    args.output.write_text(json.dumps(rows))
    Path(str(args.output) + ".scope").write_text(json.dumps(scope))
    print(
        f"{len(rows)} entries, {len(scope['skipped'])} volume boundaries, {len(scope['errors'])} errors"
    )
    raise SystemExit(bool(scope["errors"]))
