#!/usr/bin/env python3
"""Reject checksum-valid APFS metadata misbindings using a read-only image overlay.

The fixture must have a non-block-zero selected checkpoint, contiguous checkpoint
 descriptors, and a nonempty main pending-free queue. No image bytes are written.
"""

import argparse
import importlib.util
import os
from pathlib import Path
import stat
import struct


def require(condition, message):
    if not condition:
        raise ValueError(message)


def load_reader(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    require(
        spec is not None and spec.loader is not None, f"cannot import {path}"
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def u32(data, at):
    return struct.unpack_from("<I", data, at)[0]


def u64(data, at):
    return struct.unpack_from("<Q", data, at)[0]


class ReadOverlay:
    def __init__(self, source, patches):
        self.source = source
        self.patches = patches

    def seek(self, position):
        self.source.seek(position)

    def read(self, size):
        position = self.source.tell()
        result = bytearray(self.source.read(size))
        for patch_position, patch in self.patches.items():
            left = max(position, patch_position)
            right = min(position + len(result), patch_position + len(patch))
            if left < right:
                result[left - position : right - position] = patch[
                    left - patch_position : right - patch_position
                ]
        return bytes(result)


def repaired(data, checksum):
    data = bytearray(data)
    struct.pack_into("<Q", data, 0, 0)
    words = struct.unpack("<%dI" % (len(data) // 4), data)
    modulus = 0xFFFFFFFF
    a = b = 0
    for word in words[2:]:
        a = (a + word) % modulus
        b = (b + a) % modulus
    struct.pack_into("<II", data, 0, (-b - a) % modulus, b)
    require(checksum(data), "test checksum repair failed")
    return bytes(data)


def fingerprint(metadata):
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def run(source, offset, reader, baseline):
    source.seek(offset)
    first = source.read(4096)
    require(
        first[32:36] == b"NXSB",
        "fixture is not an APFS container at this offset",
    )
    block_size = u32(first, 36)
    require(
        4096 <= block_size <= 65536 and block_size & (block_size - 1) == 0,
        "unsupported fixture block size",
    )

    def read_block(address):
        source.seek(offset + address * block_size)
        data = source.read(block_size)
        require(
            len(data) == block_size, f"fixture short read at block {address}"
        )
        return data

    initial = read_block(0)
    descriptor_count = u32(initial, 104)
    descriptor_base = u64(initial, 112)
    require(
        0 < descriptor_count < 0x80000000,
        "fixture needs contiguous checkpoint descriptors",
    )
    candidates = [(u64(initial, 16), 0, initial)]
    for address in range(descriptor_base, descriptor_base + descriptor_count):
        data = read_block(address)
        if (
            data[32:36] == b"NXSB"
            and u32(data, 24) & 0xFFFF == 1
            and reader.checksum(data)
        ):
            candidates.append((u64(data, 16), address, data))
    xid, nx_address, nx = max(candidates, key=lambda row: row[:2])
    require(
        nx_address != 0,
        "fixture needs a selected checkpoint outside block zero",
    )
    require(reader.checksum(nx), "fixture checkpoint checksum invalid")
    index, length = struct.unpack_from("<II", nx, 136)
    require(
        0 < length <= descriptor_count and index < descriptor_count,
        "fixture checkpoint span invalid",
    )
    spaceman_oid = u64(nx, 152)
    mappings = {}
    last_map = None
    for n in range(length):
        address = descriptor_base + (index + n) % descriptor_count
        data = read_block(address)
        require(u64(data, 16) == xid, "fixture checkpoint XID mismatch")
        if u32(data, 24) & 0xFFFF != 12:
            continue
        last_map = (address, data)
        for j in range(u32(data, 36)):
            at = 40 + 40 * j
            require(at + 40 <= block_size, "fixture map entry out of bounds")
            oid = u64(data, at + 24)
            require(oid not in mappings, "fixture duplicate mapping OID")
            mappings[oid] = (address, at, u64(data, at + 32))
    require(
        last_map is not None and u32(last_map[1], 32) & 1,
        "fixture needs a final checkpoint map with LAST set",
    )
    require(spaceman_oid in mappings, "fixture has no spaceman mapping")
    spaceman_address = mappings[spaceman_oid][2]
    spaceman = read_block(spaceman_address)
    require(u64(spaceman, 8) == spaceman_oid, "fixture spaceman OID mismatch")
    main_queue_count = u64(spaceman, 240)
    main_queue_oid = u64(spaceman, 248)
    require(
        main_queue_count > 0 and main_queue_oid != 0,
        "fixture must exercise a nonempty main pending-free queue",
    )
    require(
        main_queue_oid in mappings,
        "fixture main queue root is not checkpoint-mapped",
    )
    queue_map_address, queue_entry_at, queue_address = mappings[main_queue_oid]
    queue = read_block(queue_address)
    require(u64(queue, 8) == main_queue_oid, "fixture queue root OID mismatch")

    def mutate(address, at, fmt, value):
        data = bytearray(read_block(address))
        struct.pack_into(fmt, data, at, value)
        return {offset + address * block_size: repaired(data, reader.checksum)}

    queue_map = read_block(queue_map_address)
    cases = {
        "spaceman_wrong_oid": mutate(
            spaceman_address, 8, "<Q", spaceman_oid ^ 1
        ),
        "queue_wrong_oid": mutate(queue_address, 8, "<Q", main_queue_oid ^ 1),
        "queue_wrong_mapping_type": mutate(
            queue_map_address,
            queue_entry_at,
            "<I",
            u32(queue_map, queue_entry_at) ^ 1,
        ),
        "queue_wrong_mapping_subtype": mutate(
            queue_map_address,
            queue_entry_at + 4,
            "<I",
            u32(queue_map, queue_entry_at + 4) ^ 1,
        ),
        "queue_wrong_fs_oid": mutate(
            queue_map_address,
            queue_entry_at + 16,
            "<Q",
            u64(queue_map, queue_entry_at + 16) ^ 1,
        ),
        "checkpoint_wrong_uuid": mutate(nx_address, 72, "<B", nx[72] ^ 1),
        "checkpoint_wrong_descriptor_base": mutate(
            nx_address, 112, "<Q", descriptor_base + 1
        ),
        "checkpoint_wrong_descriptor_count": mutate(
            nx_address, 104, "<I", descriptor_count - 1
        ),
        "missing_last_mapping_flag": mutate(
            last_map[0], 32, "<I", u32(last_map[1], 32) & ~1
        ),
        "fusion_incompatible_flag": mutate(
            nx_address, 64, "<Q", u64(nx, 64) | 0x100
        ),
    }
    for label, patches in cases.items():
        if baseline:
            try:
                baseline.measure(ReadOverlay(source, patches), offset)
            except (ValueError, KeyError, struct.error) as error:
                raise AssertionError(
                    f"{label}: baseline unexpectedly rejected: {error}"
                ) from error
        try:
            reader.measure(ReadOverlay(source, patches), offset)
        except ValueError as error:
            print(f"{label}: rejected ({error})")
        else:
            raise AssertionError(
                f"{label}: reader accepted checksum-valid mutation"
            )
    print(
        f"{len(cases)} binding mutations rejected; XID={xid}, block_size={block_size}, main_queue_blocks={main_queue_count}"
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path)
    parser.add_argument(
        "--offset",
        required=True,
        type=int,
        help="APFS container byte offset within image",
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        help="optional old reader expected to accept all mutations",
    )
    args = parser.parse_args()
    require(args.offset >= 0, "negative container offset")
    reader = load_reader(
        "spaceman_current", Path(__file__).with_name("spaceman_oracle.py")
    )
    baseline = (
        load_reader("spaceman_baseline", args.baseline)
        if args.baseline
        else None
    )
    with args.image.open("rb") as source:
        before = os.fstat(source.fileno())
        require(stat.S_ISREG(before.st_mode), "fixture must be a regular image")
        run(source, args.offset, reader, baseline)
        require(
            fingerprint(before) == fingerprint(os.fstat(source.fileno())),
            "fixture changed during read-overlay test",
        )
    print("image fingerprint unchanged")


if __name__ == "__main__":
    main()
