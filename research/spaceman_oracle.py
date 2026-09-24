#!/usr/bin/env python3
"""Count APFS allocation bits and pending-free records in a stable disk image.

This independent research oracle uses Apple's published on-disk structures:
https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf
It supports a single physical store and contiguous checkpoint descriptor area.
Use an immutable image, optionally attached read-only for Apple's control totals.
"""

import argparse
from bisect import bisect_right
import json
import os
from pathlib import Path
import stat
import struct


def require(condition, message):
    if not condition:
        raise ValueError(message)


def u32(data, at):
    return struct.unpack_from("<I", data, at)[0]


def u64(data, at):
    return struct.unpack_from("<Q", data, at)[0]


def checksum(data):
    if not u64(data, 0) or u64(data, 0) == (1 << 64) - 1:
        return False
    words = struct.unpack("<%dI" % (len(data) // 4), data)
    a = b = 0
    for value in words[2:] + words[:2]:
        a += value
        b += a
    return a % 0xFFFFFFFF == 0 and b % 0xFFFFFFFF == 0


def measure(f, offset):
    f.seek(offset)
    first = f.read(4096)
    require(
        first[32:36] == b"NXSB", "not an APFS container at the supplied offset"
    )
    block = u32(first, 36)
    require(
        block >= 4096 and (block & (block - 1)) == 0 and (block <= 65536),
        "unsupported block size",
    )
    blocks = u64(first, 40)

    def read(address, size=None, verify=False):
        size = block if size is None else size
        require(
            address >= 0
            and size > 0
            and (size % block == 0)
            and (address + size // block <= blocks),
            "out-of-container read",
        )
        f.seek(offset + address * block)
        data = f.read(size)
        require(len(data) == size, "short read")
        if verify:
            require(
                checksum(data),
                "invalid metadata checksum at block %d" % address,
            )
        return data

    nx = read(0, verify=True)
    container_uuid = nx[72:88]
    descriptor_count = u32(nx, 104)
    descriptor_base = u64(nx, 112)
    require(
        descriptor_count < 0x80000000,
        "checkpoint descriptor B-tree is unsupported",
    )
    require(descriptor_count <= 1000000, "unreasonable descriptor area")
    # Select the newest checksum-valid checkpoint, then use only its mappings.
    candidates = []
    for address in range(descriptor_base, descriptor_base + descriptor_count):
        data = read(address)
        if (
            data[32:36] == b"NXSB"
            and u32(data, 24) & 0xFFFF == 1
            and checksum(data)
        ):
            candidates.append((u64(data, 16), address, data))
    candidates.append((u64(nx, 16), 0, nx))
    xid, nx_address, nx = max(candidates, key=lambda row: row[:2])
    require(
        u32(nx, 36) == block and u64(nx, 40) == blocks,
        "checkpoint geometry changed",
    )
    require(nx[72:88] == container_uuid, "checkpoint container UUID changed")
    require(
        u32(nx, 104) == descriptor_count and u64(nx, 112) == descriptor_base,
        "checkpoint descriptor geometry changed",
    )
    # Version 1, Fusion, and unknown incompatible features need a different reader.
    require(u64(nx, 64) == 2, "unsupported APFS incompatible features")
    index = u32(nx, 136)
    length = u32(nx, 140)
    require(
        length <= descriptor_count and index < descriptor_count,
        "invalid APFS metadata",
    )
    mapping = []
    ephemeral = {}
    mapping_flags = []
    for n in range(length):
        address = descriptor_base + (index + n) % descriptor_count
        data = read(address, verify=True)
        require(
            u64(data, 16) == xid, "checkpoint descriptor transaction mismatch"
        )
        if u32(data, 24) & 0xFFFF != 12:
            continue
        mapping_flags.append(u32(data, 32))
        count = u32(data, 36)
        require(40 + count * 40 <= len(data), "invalid APFS metadata")
        for i in range(count):
            at = 40 + i * 40
            oid = u64(data, at + 24)
            require(oid not in ephemeral, "invalid APFS metadata")
            ephemeral[oid] = (
                u64(data, at + 32),
                u32(data, at + 8),
                u32(data, at),
                u32(data, at + 4),
                u64(data, at + 16),
            )
            if u64(data, at + 24) == u64(nx, 152):
                mapping.append(ephemeral[oid])
    require(
        mapping_flags
        and all(not (flags & 1) for flags in mapping_flags[:-1])
        and mapping_flags[-1] & 1,
        "incomplete checkpoint mapping",
    )
    require(
        len(mapping) == 1, "spaceman must have exactly one checkpoint mapping"
    )
    sm_address, sm_size, sm_type, sm_subtype, sm_fs_oid = mapping[0]
    sm = read(sm_address, sm_size, True)
    require(
        sm_type & 0xFFFF == 5
        and sm_subtype == 0
        and sm_fs_oid == 0
        and u32(sm, 24) == sm_type
        and u32(sm, 28) == sm_subtype
        and u64(sm, 8) == u64(nx, 152)
        and u64(sm, 16) == xid,
        "wrong spaceman object",
    )
    require(
        u32(sm, 32) == block and u64(sm, 48) == blocks,
        "spaceman geometry mismatch",
    )
    require(u64(sm, 96) == 0, "Fusion/multi-store containers unsupported")
    chunk_count = u64(sm, 56)
    cib_count = u32(sm, 64)
    cab_count = u32(sm, 68)
    recorded_free = u64(sm, 72)
    addresses = u32(sm, 80)
    require(
        addresses + 8 * (cab_count or cib_count) <= len(sm),
        "spaceman address table out of bounds",
    )
    cibs = []
    if cab_count:
        for i in range(cab_count):
            cab = read(u64(sm, addresses + 8 * i), verify=True)
            require(
                u32(cab, 24) & 0xFFFF == 6 and u64(cab, 16) <= xid,
                "invalid APFS metadata",
            )
            count = u32(cab, 36)
            require(40 + count * 8 <= len(cab), "invalid APFS metadata")
            cibs.extend((u64(cab, 40 + 8 * j) for j in range(count)))
    else:
        cibs = [u64(sm, addresses + 8 * i) for i in range(cib_count)]
    require(
        len(cibs) == cib_count and len(set(cibs)) == len(cibs),
        "invalid chunk-info block table",
    )
    chunks = []
    for address in cibs:
        cib = read(address, verify=True)
        require(
            u32(cib, 24) & 0xFFFF == 7 and u64(cib, 16) <= xid,
            "invalid APFS metadata",
        )
        count = u32(cib, 36) & 1048575
        require(40 + count * 32 <= len(cib), "invalid APFS metadata")
        for i in range(count):
            chunks.append(struct.unpack_from("<QQIIQ", cib, 40 + i * 32))
    require(len(chunks) == chunk_count, "chunk count mismatch")
    chunks.sort(key=lambda row: row[1])
    next_block = free = bitmap_count = implicit_free = 0
    for chunk_xid, address, count, expected_free, bitmap_address in chunks:
        require(
            address == next_block
            and 0 < count <= block * 8
            and (chunk_xid <= xid),
            "invalid chunk layout",
        )
        require(expected_free <= count, "invalid APFS metadata")
        # A missing bitmap denotes a wholly free chunk, never an unknown count.
        if bitmap_address:
            bits = (
                int.from_bytes(read(bitmap_address), "little")
                & (1 << count) - 1
            )
            actual_free = count - bin(bits).count("1")
            bitmap_count += 1
        else:
            require(
                expected_free == count, "absent bitmap for a non-free chunk"
            )
            actual_free = count
            implicit_free += count
        require(
            actual_free == expected_free,
            "bitmap and chunk counter disagree at %d: %d != %d"
            % (address, actual_free, expected_free),
        )
        free += actual_free
        next_block += count
    require(
        next_block == blocks and free == recorded_free,
        "spaceman total differs from counted bitmap bits",
    )

    def free_queue(root):
        visited = set()
        records = []

        def walk(oid, expected_level=None):
            require(oid not in visited, "free-queue tree cycle")
            visited.add(oid)
            address, size, object_type, subtype, fs_oid = ephemeral[oid]
            data = read(address, size, True)
            require(
                object_type & 0xFFFF in (2, 3)
                and subtype == 9
                and fs_oid == 0
                and u32(data, 24) == object_type
                and u32(data, 28) == subtype
                and u64(data, 8) == oid
                and u64(data, 16) <= xid,
                "invalid free-queue object",
            )
            flags, level = struct.unpack_from("<HH", data, 32)
            count = u32(data, 36)
            require(flags & 4, "variable-size free-queue keys unsupported")
            require(
                expected_level is None or level == expected_level,
                "invalid APFS metadata",
            )
            require(bool(flags & 2) == (level == 0), "invalid APFS metadata")
            table_offset, table_length = struct.unpack_from("<HH", data, 40)
            require(
                table_offset == 0 and count * 4 <= table_length,
                "invalid APFS metadata",
            )
            keys = 56 + table_length
            values = len(data) - (40 if flags & 1 else 0)
            if flags & 1:
                require(
                    u32(data, len(data) - 32) == 16
                    and u32(data, len(data) - 28) == 8,
                    "invalid APFS metadata",
                )
            for n in range(count):
                key_offset, value_offset = struct.unpack_from(
                    "<HH", data, 56 + 4 * n
                )
                key = keys + key_offset
                require(keys <= key <= values - 16, "invalid APFS metadata")
                value = values - value_offset
                if value_offset != 0xFFFF:
                    require(
                        keys <= value <= values - 8, "invalid APFS metadata"
                    )
                if level:
                    require(value_offset != 0xFFFF, "invalid APFS metadata")
                    walk(u64(data, value), level - 1)
                else:
                    record_xid, start = struct.unpack_from("<QQ", data, key)
                    # Fixed-size free-queue trees encode a one-block run with no value.
                    length = 1 if value_offset == 0xFFFF else u64(data, value)
                    require(
                        record_xid <= xid
                        and length > 0
                        and (start + length <= blocks),
                        "invalid APFS metadata",
                    )
                    records.append((record_xid, start, length))

        if root:
            walk(root)
        return records

    # Internal-pool free blocks stay reserved to metadata. Apple's allocatable
    # counter includes the main queue, which still has set allocation bits.
    pending = free_queue(u64(sm, 248))
    pending_blocks = previous_end = 0
    current_bitmap, current_bits = None, 0
    chunk_starts = [chunk[1] for chunk in chunks]
    # Work in ranges rather than allocating one Python object per queued block.
    for start, end in sorted(
        (start, start + length) for _, start, length in pending
    ):
        require(start >= previous_end, "duplicate free-queue block")
        previous_end = end
        pending_blocks += end - start
        index = bisect_right(chunk_starts, start) - 1
        position = start
        while position < end:
            _, chunk_start, chunk_length, _, bitmap = chunks[index]
            require(bitmap != 0, "queued block is already free")
            if bitmap != current_bitmap:
                current_bitmap, current_bits = (
                    bitmap,
                    int.from_bytes(read(bitmap), "little"),
                )
            limit = min(end, chunk_start + chunk_length)
            mask = ((1 << (limit - position)) - 1) << (position - chunk_start)
            require(current_bits & mask == mask, "queued block is already free")
            position = limit
            index += 1
    require(
        pending_blocks == u64(sm, 240),
        "queue records do not match spaceman count",
    )
    return {
        "measurement": "raw_apfs_allocation_bitmap",
        "checkpoint_xid": xid,
        "checkpoint_block": nx_address,
        "container_uuid_hex": container_uuid.hex(),
        "incompatible_features": u64(nx, 64),
        "checkpoint_descriptor_layout": "contiguous",
        "block_size": block,
        "block_count": blocks,
        "chunks": len(chunks),
        "chunk_info_address_blocks": cab_count,
        "spaceman_flags": u32(sm, 144),
        "spaceman_version": u32(sm, 336),
        "bitmap_blocks": bitmap_count,
        "implicit_free_blocks": implicit_free,
        "capacity_bytes": blocks * block,
        "free_bytes": free * block,
        "occupied_bytes": (blocks - free) * block,
        "spaceman_free_count_matches": True,
        "free_queues": {
            name: {
                "count": u64(sm, 200 + 40 * i),
                "tree_oid": u64(sm, 208 + 40 * i),
                "oldest_xid": u64(sm, 216 + 40 * i),
            }
            for i, name in enumerate(["internal_pool", "main", "tier2"])
        },
        "main_queue_records": pending,
        "main_queue_blocks_verified_allocated": pending_blocks,
        "free_bytes_including_main_queue": (free + pending_blocks) * block,
        "internal_pool_blocks": u64(sm, 152),
        "reserved_blocks": u64(sm, 184),
        "reserved_allocated_blocks": u64(sm, 192),
        "space_manager_block": sm_address,
        "allocation_bitmap_addresses": sorted(
            {chunk[4] for chunk in chunks if chunk[4]}
        ),
    }


def fingerprint(metadata):
    return (
        metadata.st_dev,
        metadata.st_ino,
        metadata.st_size,
        metadata.st_mtime_ns,
        metadata.st_ctime_ns,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("image", type=Path)
    parser.add_argument(
        "--offset",
        type=int,
        default=0,
        help="container byte offset within the image",
    )
    args = parser.parse_args()
    try:
        require(args.offset >= 0, "negative container offset")
        with args.image.open("rb") as source:
            before = os.fstat(source.fileno())
            require(
                stat.S_ISREG(before.st_mode),
                "use a stable regular image, not a live raw device",
            )
            result = measure(source, args.offset)
            require(
                args.offset + result["capacity_bytes"] <= before.st_size,
                "container exceeds the image size",
            )
            require(
                fingerprint(before) == fingerprint(os.fstat(source.fileno())),
                "image changed while reading",
            )
        print(json.dumps(result, indent=2))
    except (OSError, ValueError, KeyError, struct.error) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
