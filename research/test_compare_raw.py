#!/usr/bin/env python3
"""Small raw-record fixtures for the comparator's strict result contract."""

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("compare_raw.py")


class CompareRawTests(unittest.TestCase):
    def run_comparison(
        self,
        *,
        dump_extra="",
        scan_bytes=4096,
        total_bytes=4096,
        mapped_ranges=None,
        strict=True,
        include_mapping=True,
    ):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dump = root / "dump.txt"
            manifest = root / "manifest.json"
            scan = root / "scan.json"
            mapped = root / "mapped.jsonl"
            dump.write_text(
                "u32 block_size : 1000\nInode 2 => 0 2 \n"
                "Inode A => 0 64 \nFileExt 64 0 => 1000 1 0\n" + dump_extra
            )
            manifest.write_text(
                json.dumps(
                    [
                        {
                            "path": "",
                            "inode": 2,
                            "mode": 0o040755,
                            "allocated": 0,
                        },
                        {
                            "path": "file",
                            "inode": 10,
                            "mode": 0o100644,
                            "allocated": 4096,
                        },
                    ]
                )
            )
            scan.write_text(
                json.dumps(
                    {
                        "entries": [
                            {
                                "path": "/snapshot",
                                "type": "directory",
                                "physical_bytes": total_bytes,
                                "unmeasured_bytes": 0,
                            },
                            {
                                "path": "/snapshot/file",
                                "type": "file",
                                "physical_bytes": scan_bytes,
                                "unmeasured_bytes": 0,
                            },
                        ],
                        "total_physical_bytes": total_bytes,
                    }
                )
            )
            mapped.write_text(
                json.dumps(
                    {
                        "path": "file",
                        "ranges": [[4096, 8192]]
                        if mapped_ranges is None
                        else mapped_ranges,
                    }
                )
                + "\n"
            )
            command = [
                sys.executable,
                str(SCRIPT),
                str(dump),
                str(manifest),
                str(scan),
                "/snapshot",
                "--block-size",
                "4096",
            ]
            if include_mapping:
                command += ["--mapped-extents", str(mapped)]
            if strict:
                command.append("--strict")
            result = subprocess.run(
                command, capture_output=True, text=True, check=False
            )
            report = json.loads(result.stdout) if result.stdout else None
            return result, report

    def test_external_attribute_gap_is_reported_but_valid(self):
        extra = (
            "XAttr A 'com.apple.decmpfs' => 0 [XATTR_DATA_STREAM] 0 : 65 \n"
            "FileExt 65 0 => 1000 2 0\n"
        )
        result, report = self.run_comparison(dump_extra=extra)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["strict_verification"]["passed"])
        self.assertEqual(
            report["address_comparison"]["unmapped_raw_union_bytes"], 4096
        )
        self.assertFalse(report["snapshot_identity_verified"])

    def test_sealed_index_hashes_do_not_duplicate_leaf_records(self):
        digest = "A" * 64
        extra = (
            f"Inode A => 0 {digest}\n"
            f"XAttr A 'com.apple.decmpfs' => 0 {digest}\n"
            "XAttr A 'com.apple.decmpfs' => 0 [XATTR_DATA_STREAM] 0 : 65 \n"
            "FileExt 65 0 => 1000 2 0\n"
        )
        result, report = self.run_comparison(dump_extra=extra)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            report["address_comparison"]["unmapped_raw_union_bytes"], 4096
        )

    def test_contradictions_fail_strict_mode(self):
        cases = [
            {"mapped_ranges": [[8192, 12288]]},
            {"scan_bytes": 8192},
            {"total_bytes": 8192},
            {"dump_extra": "Inode B => 0 66 \n"},
        ]
        for case in cases:
            with self.subTest(case=case):
                result, report = self.run_comparison(**case)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(report["strict_verification"]["passed"])
                self.assertTrue(report["strict_verification"]["contradictions"])

    def test_exploratory_mode_still_reports_contradictions(self):
        result, report = self.run_comparison(
            mapped_ranges=[[8192, 12288]], strict=False
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            report["address_comparison"]["unexpected_ranges"][0][
                "unexpected_bytes"
            ],
            4096,
        )
        self.assertNotIn("strict_verification", report)

    def test_strict_mode_requires_address_export(self):
        result, _ = self.run_comparison(include_mapping=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("--strict requires --mapped-extents", result.stderr)

    def test_duplicate_and_malformed_records_fail(self):
        for extra in (
            "Inode A => 0 64 \n",
            "FileExt 64 0 => 1000 2 0\n",
            "XAttr A 'bad' => 0 [XATTR_DATA_STREAM] unexpected\n",
        ):
            with self.subTest(extra=extra):
                result, _ = self.run_comparison(dump_extra=extra)
                self.assertEqual(result.returncode, 1)
                self.assertIn("ValueError", result.stderr)

    def run_identity_comparison(self, change=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            dump = root / "dump.txt"
            manifest = root / "manifest.json"
            scan = root / "scan.json"
            mapped = root / "mapped.jsonl"
            scope = root / "manifest.json.scope"
            raw_sidecar = root / "dump.txt.identity.json"
            dump.write_text(
                "u32 block_size : 1000\n"
                "1 | 1 | 1 | 1 | 1 | 1 | Filesystem Tree\n"
                "Inode 2 => 0 2 \nInode A => 0 64 \n"
                "FileExt 64 0 => 1000 1 0\n"
            )
            manifest.write_text(
                json.dumps(
                    [
                        {
                            "path": "",
                            "inode": 2,
                            "mode": 0o040755,
                            "allocated": 0,
                        },
                        {
                            "path": "file",
                            "inode": 10,
                            "mode": 0o100644,
                            "allocated": 4096,
                        },
                    ]
                )
            )
            scan.write_text(
                json.dumps(
                    {
                        "entries": [
                            {
                                "path": "/snapshot",
                                "type": "directory",
                                "physical_bytes": 4096,
                                "unmeasured_bytes": 0,
                            },
                            {
                                "path": "/snapshot/file",
                                "type": "file",
                                "physical_bytes": 4096,
                                "unmeasured_bytes": 0,
                            },
                        ],
                        "total_physical_bytes": 4096,
                    }
                )
            )
            mapped.write_text(
                json.dumps({"path": "file", "ranges": [[4096, 8192]]}) + "\n"
            )
            mounted = {
                "snapshot_xid": 1,
                "snapshot_name": "fixture-snapshot",
                "snapshot_uuid": "11111111-1111-1111-1111-111111111111",
                "base_volume_uuid": "22222222-2222-2222-2222-222222222222",
                "container_uuid": "33333333-3333-3333-3333-333333333333",
                "volume_group_uuid": "44444444-4444-4444-4444-444444444444",
            }
            scope.write_text(
                json.dumps({"errors": [], "skipped": [], "identity": mounted})
            )
            raw = {
                **mounted,
                "schema_version": 1,
                "dump_sha256": hashlib.sha256(dump.read_bytes()).hexdigest(),
                "snapshot_superblock_paddr": 2,
                "snapshot_superblock_oid": 3,
                "snapshot_superblock_xid": 1,
                "filesystem_root_oid": 4,
                "sealed": False,
                "sealed_fext_tree_oid": 0,
                "block_size": 4096,
            }
            if change:
                change(raw)
            raw_sidecar.write_text(json.dumps(raw))
            command = [
                sys.executable,
                str(SCRIPT),
                str(dump),
                str(manifest),
                str(scan),
                "/snapshot",
                "--block-size",
                "4096",
                "--snapshot-xid",
                "1",
                "--raw-identity",
                str(raw_sidecar),
                "--namespace-scope",
                str(scope),
                "--mapped-extents",
                str(mapped),
                "--strict",
                "--require-snapshot-identity",
            ]
            result = subprocess.run(
                command, capture_output=True, text=True, check=False
            )
            report = json.loads(result.stdout) if result.stdout else None
            return result, report

    def test_ordinary_btree_index_records_are_not_leaf_values(self):
        index = (
            "29F81 | 1 | 8B5E | 13FB | 2 | E | Filesystem Tree\n"
            "0001 | 0003 | 00000003 | 0000 L 0040 | 0613 L 0935 | 05AD L 05D4 | FFFF L 0000  [BTNODE_ROOT]\n"
            "DirRec   1 ACA68C0C 'private-dir' => 20B14\n"
            "XAttr    3A3CE 'com.apple.fs.symlink' => 20B13\n"
            "Inode    50836 => 24323\n"
        )
        result, report = self.run_comparison(dump_extra=index)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["strict_verification"]["passed"])
        self.assertEqual(report["raw_inodes"], 2)

    def test_sealed_headerless_index_records_are_not_leaf_values(self):
        index = (
            "3BD85 | 0 | 0 | 0 | 0 | 0 | 0\n"
            "0019 | 0003 | 0000000F | 0000 L 0080 | 0132 L 0B96 | FFFF L 0000 | FFFF L 0000  [BTNODE_ROOT, BTNODE_HASHED, BTNODE_NOHEADER]\n"
            "Inode    FFFFFFF0000AB1E => DF4 " + "A" * 64 + "\n"
            "XAttr    FFFFFFF0003841F 'com.apple.fs.symlink' => 37BD "
            + "B" * 64
            + "\n"
        )
        result, report = self.run_comparison(dump_extra=index)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["strict_verification"]["passed"])
        self.assertEqual(report["raw_inodes"], 2)
        self.assertEqual(report["headerless_filesystem_nodes"], 1)

    def test_sealed_headerless_malformed_leaf_still_fails(self):
        leaf = (
            "3BD86 | 0 | 0 | 0 | 0 | 0 | 0\n"
            "001A | 0000 | 00000001 | 0000 L 0080 | 0132 L 0B96 | FFFF L 0000 | FFFF L 0000  [BTNODE_LEAF, BTNODE_HASHED, BTNODE_NOHEADER]\n"
            "Inode    FFFFFFF0000AB1E => DF4 " + "A" * 64 + "\n"
        )
        result, _ = self.run_comparison(dump_extra=leaf)
        self.assertEqual(result.returncode, 1)
        self.assertIn("unrecognized inode record", result.stderr)

    def test_malformed_btree_leaf_record_still_fails(self):
        leaf = (
            "2A31E | 1 | 20B14 | 13D8 | 3 | E | Filesystem Tree\n"
            "0002 | 0000 | 00000001 | 0000 L 0040 | 0613 L 0935 | 05AD L 05D4 | FFFF L 0000  [BTNODE_LEAF]\n"
            "Inode    50836 => 24323\n"
        )
        result, _ = self.run_comparison(dump_extra=leaf)
        self.assertEqual(result.returncode, 1)
        self.assertIn("unrecognized inode record", result.stderr)

    def test_matching_raw_and_mounted_snapshot_identity(self):
        result, report = self.run_identity_comparison()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["snapshot_identity_verified"])
        self.assertEqual(report["snapshot_identity"]["level"], "raw_uuid")

    def test_snapshot_identity_contradictions_fail(self):
        cases = [
            ("dump digest", lambda r: r.update(dump_sha256="0" * 64)),
            ("name", lambda r: r.update(snapshot_name="other-snapshot")),
            ("xid", lambda r: r.update(snapshot_xid=2)),
            ("volume", lambda r: r.update(base_volume_uuid="5" * 36)),
            ("container", lambda r: r.update(container_uuid="6" * 36)),
            ("root", lambda r: r.pop("filesystem_root_oid")),
            ("uuid", lambda r: r.update(snapshot_uuid="7" * 36)),
        ]
        for label, change in cases:
            with self.subTest(label=label):
                result, _ = self.run_identity_comparison(change)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("ValueError", result.stderr)

    def test_missing_raw_uuid_reports_weaker_identity(self):
        result, report = self.run_identity_comparison(
            lambda r: r.update(snapshot_uuid=None)
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["snapshot_identity_verified"])
        self.assertEqual(
            report["snapshot_identity"]["level"], "xid_name_volume"
        )
        self.assertFalse(
            report["snapshot_identity"]["raw_uuid_directly_verified"]
        )


if __name__ == "__main__":
    unittest.main()
