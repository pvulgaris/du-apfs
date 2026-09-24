#!/usr/bin/env python3
"""Compare a stable apfs-fuse dump against a scanner's JSON and inode manifest.

The dump must select one stable volume: an unmounted disposable image or a
frozen snapshot whose filesystem tree is pinned by a read-only mount.
This deliberately uses on-disk stream records, not F_LOG2PHYS_EXT or st_blocks.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import stat
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("dump", type=Path)
parser.add_argument("manifest", type=Path)
parser.add_argument("scan", type=Path)
parser.add_argument("root")
parser.add_argument(
    "--block-size",
    type=int,
    help="required when a snapshot-only dump omits the container header",
)
parser.add_argument(
    "--snapshot-xid",
    type=int,
    help="reject filesystem nodes newer than this snapshot",
)
parser.add_argument(
    "--raw-identity",
    type=Path,
    help="adapter sidecar with exact selected snapshot metadata and dump hash",
)
parser.add_argument(
    "--require-snapshot-identity",
    action="store_true",
    help="fail unless raw and mounted snapshot identities agree",
)
parser.add_argument(
    "--mapped-extents",
    type=Path,
    help="JSON lines of scanner physical ranges by path",
)
parser.add_argument(
    "--strict",
    action="store_true",
    help="fail on address, size, union, or unexplained-inode contradictions; does not verify snapshot identity",
)
parser.add_argument(
    "--subset",
    action="store_true",
    help="manifest selects one physical volume within a larger unified System/Data scan",
)
parser.add_argument(
    "--synthetic-path",
    action="append",
    default=[],
    help="explicitly verified kernel-synthesized, unallocated link or directory absent from the raw tree",
)
parser.add_argument(
    "--namespace-scope",
    type=Path,
    help="manifest walker scope with paths hidden by other mounted volumes",
)
args = parser.parse_args()
if args.strict and not args.mapped_extents:
    parser.error("--strict requires --mapped-extents")
if args.require_snapshot_identity and not args.raw_identity:
    parser.error("--require-snapshot-identity requires --raw-identity")
block_size = args.block_size
streams = {}
attributes = {}
extents = {}
attribute_names = set()
extent_keys = set()
node_xids = []
private_children = set()
node_subtype = None
node_is_leaf = None
awaiting_node_flags = False
headerless_candidate = False
headerless_nodes = 0
for line in args.dump.open():
    line = line.rstrip("\n")
    # Index records contain child OIDs or hashes. Only leaf records carry
    # inode, xattr, and extent values for the oracle.
    if "BTNODE_NOHEADER" in line:
        headerless_nodes += 1
    m = re.search(r"u32 block_size\s+: ([0-9A-F]+)", line)
    if m:
        size = int(m[1], 16)
        if block_size is not None and size != block_size:
            raise ValueError("inconsistent APFS block sizes in dump")
        block_size = size
    m = re.match(
        r"[0-9A-F]+ \| [0-9A-F]+ \| [0-9A-F]+ \| ([0-9A-F]+) \| ([0-9A-F]+) \| ([0-9A-F]+) \| ",
        line,
    )
    if m:
        node_subtype = int(m[3], 16)
        node_is_leaf = None
        headerless_candidate = int(m[2], 16) == 0 and node_subtype == 0
        awaiting_node_flags = (
            node_subtype in (0x0E, 0x1F) or headerless_candidate
        )
        if (
            node_subtype == 0x1F
        ):  # Sealed volumes keep data in a separate fext tree.
            node_xids.append(int(m[1], 16))
    m = re.match(
        r"[0-9A-F]+ \| [0-9A-F]+ \| [0-9A-F]+ \| ([0-9A-F]+) \| [0-9A-F]+ \| [0-9A-F]+ \| Filesystem Tree$",
        line,
    )
    if m:
        node_xids.append(int(m[1], 16))
    if awaiting_node_flags:
        flags = re.match(
            r"^([0-9A-F]{4}) \| ([0-9A-F]{4}) \| [0-9A-F]{8} \|", line
        )
        if flags:
            bits, level = (int(value, 16) for value in flags.groups())
            if headerless_candidate:
                if not bits & 0x0010:  # BTNODE_NOHEADER
                    raise ValueError(
                        f"zero-header B-tree node lacks BTNODE_NOHEADER: {line}"
                    )
                # Sealed filesystem nodes suppress the object header, including
                # its subtype. Their node flags still distinguish index and leaf.
                node_subtype = 0x0E
            node_is_leaf = bool(bits & 0x0002)  # BTNODE_LEAF
            if node_is_leaf != (level == 0):
                raise ValueError(
                    f"inconsistent B-tree leaf flag and level: {line}"
                )
            awaiting_node_flags = False
    if node_subtype in (0x0E, 0x1F):
        if node_is_leaf is False:
            continue
        if node_is_leaf is None and (
            line.startswith(("Inode ", "XAttr ", "FileExt ", "DirRec "))
        ):
            raise ValueError(
                f"filesystem record before B-tree node flags: {line}"
            )
    elif node_subtype is None and re.search(
        r"=> [0-9A-F]+ [0-9A-F]{64}$", line
    ):
        # Preserve synthetic headerless fixture support. Real B-tree records
        # must be classified by their parsed node flags above.
        continue
    m = re.match(r"DirRec\s+([0-9A-F]+) [0-9A-F]+ '.+' => ([0-9A-F]+) \[", line)
    if m and int(m[1], 16) in (3, 0x0800000000000003):
        private_children.add(int(m[2], 16))
    m = re.match(r"Inode\s+([0-9A-F]+) => [0-9A-F]+ ([0-9A-F]+) ", line)
    if line.startswith("Inode ") and not m:
        raise ValueError(f"unrecognized inode record: {line}")
    if m:
        inode, stream = (int(x, 16) for x in m.groups())
        if inode in streams:
            raise ValueError(f"duplicate inode record: {inode}")
        streams[inode] = stream
    if line.startswith("XAttr "):
        name_match = re.match(r"XAttr\s+([0-9A-F]+) '(.+)' => ", line)
        if not name_match:
            raise ValueError(f"unrecognized attribute record: {line}")
        attr_key = (int(name_match[1], 16), name_match[2])
        if attr_key in attribute_names:
            raise ValueError(f"duplicate attribute record: {attr_key}")
        attribute_names.add(attr_key)
        if "XATTR_DATA_STREAM" in line:
            m = re.match(
                r"XAttr\s+([0-9A-F]+) '(.+)' => .*\[[^]]*\bXATTR_DATA_STREAM\b[^]]*\] [0-9A-F]+ : ([0-9A-F]+) ",
                line,
            )
            if not m:
                raise ValueError(f"unrecognized attribute stream: {line}")
            inode, name, stream = m.groups()
            attributes.setdefault(int(inode, 16), []).append(
                (name, int(stream, 16))
            )
    m = re.match(
        r"FileExt\s+([0-9A-F]+) ([0-9A-F]+) => ([0-9A-F]+) ([0-9A-F]+) [0-9A-F]+$",
        line,
    )
    if line.startswith("FileExt ") and not m:
        raise ValueError(f"unrecognized file extent: {line}")
    if not m and node_subtype == 0x1F:
        m = re.match(
            r"([0-9A-F]+) ([0-9A-F]+) => ([0-9A-F]+) ([0-9A-F]+)$", line
        )
    if m:
        if not block_size:
            parser.error("APFS block size required before extent records")
        stream, logical, length, block = (int(x, 16) for x in m.groups())
        if (stream, logical) in extent_keys:
            raise ValueError(
                f"duplicate extent key: stream={stream} logical={logical}"
            )
        extent_keys.add((stream, logical))
        if block:  # APFS encodes holes with physical block zero.
            length &= (1 << 56) - 1
            extents.setdefault(stream, []).append(
                (block * block_size, block * block_size + length)
            )

if not block_size:
    parser.error("missing APFS block size")


def union(ranges):
    merged = []
    for lo, hi in sorted(ranges, key=lambda r: (r[0], r[1])):
        if lo < 0 or hi <= lo or lo % block_size or hi % block_size:
            raise ValueError(f"invalid physical range: {lo}:{hi}")
        if merged and lo <= merged[-1][1]:
            merged[-1][1] = max(hi, merged[-1][1])
        else:
            merged.append([lo, hi])
    return merged


def union_bytes(ranges):
    return sum(hi - lo for lo, hi in union(ranges))


root = args.root.rstrip("/") or "/"


def relative_path(path):
    if path == root:
        return ""
    return path.removeprefix(root if root == "/" else root + "/")


if args.snapshot_xid is not None:
    if not node_xids or max(node_xids) > args.snapshot_xid:
        raise ValueError(
            "filesystem tree is not pinned to the requested snapshot"
        )


snapshot_identity = {
    "verified": False,
    "level": None,
    "raw_uuid_directly_verified": False,
}
if args.raw_identity:
    if args.snapshot_xid is None or args.namespace_scope is None:
        parser.error(
            "--raw-identity requires --snapshot-xid and --namespace-scope"
        )
    raw = json.loads(args.raw_identity.read_text())
    scope_identity = json.loads(args.namespace_scope.read_text()).get(
        "identity"
    )
    if not scope_identity:
        raise ValueError("namespace scope lacks mounted snapshot identity")
    digest_state = hashlib.sha256()
    with args.dump.open("rb") as binary_dump:
        for chunk in iter(lambda: binary_dump.read(1 << 20), b""):
            digest_state.update(chunk)
    digest = digest_state.hexdigest()
    if raw.get("schema_version") != 1 or raw.get("dump_sha256") != digest:
        raise ValueError("raw identity sidecar does not match this dump")
    expected = {
        "snapshot_xid": scope_identity["snapshot_xid"],
        "snapshot_name": scope_identity["snapshot_name"],
        "base_volume_uuid": scope_identity["base_volume_uuid"],
        "container_uuid": scope_identity["container_uuid"],
    }
    if raw.get("snapshot_xid") != args.snapshot_xid:
        raise ValueError("raw selected XID differs from requested XID")
    for field, value in expected.items():
        if raw.get(field) != value:
            raise ValueError(
                f"raw and mounted snapshot identities disagree on {field}"
            )
    if raw.get("volume_group_uuid") != scope_identity.get("volume_group_uuid"):
        raise ValueError("raw and mounted volume group UUIDs disagree")
    for field in (
        "snapshot_superblock_paddr",
        "snapshot_superblock_oid",
        "snapshot_superblock_xid",
        "filesystem_root_oid",
    ):
        if not isinstance(raw.get(field), int) or raw[field] <= 0:
            raise ValueError(f"raw snapshot identity lacks valid {field}")
    if raw["snapshot_superblock_xid"] > args.snapshot_xid:
        raise ValueError("frozen superblock is newer than selected snapshot")
    if raw.get("sealed") and not raw.get("sealed_fext_tree_oid"):
        raise ValueError("sealed snapshot has no fext tree OID")
    if raw.get("block_size") != block_size:
        raise ValueError("raw sidecar and dump block sizes differ")
    raw_uuid = raw.get("snapshot_uuid")
    if raw_uuid is not None and raw_uuid != scope_identity["snapshot_uuid"]:
        raise ValueError("raw and mounted snapshot UUIDs disagree")
    snapshot_identity = {
        "verified": True,
        "level": "raw_uuid" if raw_uuid is not None else "xid_name_volume",
        "raw_uuid_directly_verified": raw_uuid is not None,
        "snapshot_xid": args.snapshot_xid,
        "snapshot_uuid": scope_identity["snapshot_uuid"],
        "snapshot_name": scope_identity["snapshot_name"],
        "base_volume_uuid": scope_identity["base_volume_uuid"],
        "container_uuid": scope_identity["container_uuid"],
        "snapshot_superblock_paddr": raw["snapshot_superblock_paddr"],
        "snapshot_superblock_xid": raw["snapshot_superblock_xid"],
        "filesystem_root_oid": raw["filesystem_root_oid"],
        "dump_sha256": digest,
    }

manifest = json.loads(args.manifest.read_text())
scan = json.loads(args.scan.read_text())
files = []
all_ranges = []
data_ranges = []
raw_by_path = {}
synthetic = set(args.synthetic_path)
verified_synthetic = set()
for row in manifest:
    inode = row["inode"]
    if inode not in streams:
        if (
            row["path"] not in synthetic
            or row.get("allocated") != 0
            or not (
                stat.S_ISLNK(row.get("mode", 0))
                or stat.S_ISDIR(row.get("mode", 0))
            )
        ):
            raise ValueError(
                f"inode {inode} absent from dump at {row['path']}: stale manifest or wrong image"
            )
        verified_synthetic.add(row["path"])
    data = extents.get(streams.get(inode), [])
    attrs = [
        (name, extents.get(stream, []))
        for name, stream in attributes.get(inode, [])
    ]
    ranges = data + [r for _, attr_ranges in attrs for r in attr_ranges]
    all_ranges.extend(ranges)
    data_ranges.extend(data)
    raw_by_path[row["path"]] = ranges
    files.append(
        {
            "path": row["path"],
            "data_bytes": union_bytes(data),
            "all_stream_bytes": union_bytes(ranges),
            "attribute_bytes": {name: union_bytes(rs) for name, rs in attrs},
        }
    )
if verified_synthetic != synthetic:
    raise ValueError(
        "synthetic exclusions must identify absent, unallocated namespace entries"
    )

by_path = {r["path"]: r for r in files}
scan_files = {}
differences = []
for row in scan["entries"]:
    relative = relative_path(row["path"])
    expected = by_path.get(relative)
    if row["type"] == "file":
        scan_files[relative] = row
    # A directory's reported size is recursive. Its own external attributes
    # must be checked separately from descendant file extents.
    measured = row["physical_bytes"] if row["type"] == "file" else 0
    if expected and expected["all_stream_bytes"] != measured:
        differences.append(
            {
                **expected,
                "type": row["type"],
                "scanner_bytes": measured,
                "scanner_unmeasured": row["unmeasured_bytes"],
            }
        )
unlisted = set(streams) - {row["inode"] for row in manifest}
reserved_roots = {2, 3, 0x0800000000000002, 0x0800000000000003}
overlaid = {}
if args.namespace_scope:
    scope = json.loads(args.namespace_scope.read_text())
    if scope["errors"]:
        raise ValueError("namespace manifest has traversal errors")
    skipped = {row["path"].removeprefix("/") for row in scope["skipped"]}
    parents = {}
    for row in manifest:
        if stat.S_ISDIR(row.get("mode", 0)):
            parents.setdefault(row["inode"], set()).add(row["path"])
    # Match raw directory entries to independently observed mount boundaries.
    # These hidden mount-point inodes still contribute to the raw extent union.
    for line in args.dump.open():
        m = re.match(
            r"DirRec\s+([0-9A-F]+) [0-9A-F]+ '(.+)' => ([0-9A-F]+) \[", line
        )
        if not m or int(m[3], 16) not in unlisted:
            continue
        parent, name, inode = int(m[1], 16), m[2], int(m[3], 16)
        for prefix in parents.get(parent, []):
            path = (prefix + "/" + name).removeprefix("/")
            if path in skipped:
                ranges = extents.get(streams[inode], []) + [
                    r
                    for _, stream in attributes.get(inode, [])
                    for r in extents.get(stream, [])
                ]
                overlaid[inode] = {
                    "path": path,
                    "inode": inode,
                    "raw_stream_bytes": union_bytes(ranges),
                }
report = {
    "block_size": block_size,
    "filesystem_nodes": len(node_xids),
    "snapshot_identity_verified": snapshot_identity["verified"],
    "snapshot_identity": snapshot_identity,
    "headerless_filesystem_nodes": headerless_nodes,
    "sealed_hashes_verified": False if headerless_nodes else None,
    "synthetic_namespace_entries": sorted(verified_synthetic),
    "maximum_node_xid": max(node_xids) if node_xids else None,
    "manifest_inodes": len({row["inode"] for row in manifest}),
    "raw_inodes": len(streams),
    "unlisted_inodes": len(unlisted),
    "private_directory_children": len(private_children),
    "overlaid_namespace_entries": sorted(
        overlaid.values(), key=lambda row: row["path"]
    ),
    "unexplained_unlisted_inodes": sorted(
        unlisted - reserved_roots - private_children - set(overlaid)
    ),
    "data_union_bytes": union_bytes(data_ranges),
    "all_stream_union_bytes": union_bytes(all_ranges),
    "all_extent_record_union_bytes": union_bytes(
        [r for rs in extents.values() for r in rs]
    ),
    "differences": differences,
    "files": files,
}
if args.mapped_extents:
    mapped = []
    unexpected = []
    size_mismatches = []
    seen = set()
    for line in args.mapped_extents.open():
        row = json.loads(line)
        path, ranges = row["path"], row["ranges"]
        if path in seen:
            raise ValueError(f"duplicate mapped path: {path}")
        seen.add(path)
        raw = raw_by_path[path]
        # Adding a subset cannot enlarge the raw union. Check per file so an
        # address belonging to a different inode cannot silently pass.
        excess = union_bytes(raw + ranges) - union_bytes(raw)
        if excess:
            unexpected.append({"path": path, "unexpected_bytes": excess})
        if (
            path not in scan_files
            or union_bytes(ranges) != scan_files[path]["physical_bytes"]
        ):
            size_mismatches.append(path)
        mapped.extend(ranges)
    expected_paths = {
        relative_path(r["path"]) for r in scan["entries"] if r["type"] == "file"
    }
    if args.subset:
        expected_paths &= set(by_path)
    if seen != expected_paths:
        raise ValueError(
            f"mapped file set differs from scan: missing={len(expected_paths - seen)} extra={len(seen - expected_paths)}"
        )
    mapped_bytes = union_bytes(mapped)
    report["address_comparison"] = {
        "files": len(seen),
        "unexpected_ranges": unexpected,
        "per_file_size_mismatches": size_mismatches,
        "mapped_union_bytes": mapped_bytes,
        "scanner_union_bytes": None
        if args.subset
        else scan["total_physical_bytes"],
        "union_matches_scan": None
        if args.subset
        else mapped_bytes == scan["total_physical_bytes"],
        "unmapped_raw_union_bytes": union_bytes(all_ranges + mapped)
        - mapped_bytes,
    }
if args.strict:
    address = report["address_comparison"]
    contradictions = []
    if address["unexpected_ranges"]:
        contradictions.append("mapped ranges outside their raw inode streams")
    if address["per_file_size_mismatches"]:
        contradictions.append("mapped ranges disagree with scanner file sizes")
    if address["union_matches_scan"] is False:
        contradictions.append("mapped union disagrees with scanner total")
    if report["unexplained_unlisted_inodes"]:
        contradictions.append(
            "raw inodes absent from manifest without an explanation"
        )
    if any(
        row["type"] == "file" and row["scanner_bytes"] > row["all_stream_bytes"]
        for row in differences
    ):
        contradictions.append("scanner file size exceeds its raw streams")
    report["strict_verification"] = {
        "passed": not contradictions,
        "contradictions": contradictions,
        "snapshot_identity_verified": snapshot_identity["verified"],
    }
print(json.dumps(report, indent=2))
if args.strict and contradictions:
    sys.exit(1)
if args.require_snapshot_identity and not snapshot_identity["verified"]:
    sys.exit(1)
