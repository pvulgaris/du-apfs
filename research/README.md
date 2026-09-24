# Measurement research

These tools are independent checks and experiments. They are not runtime
dependencies of `du-apfs`. Keep machine-specific dumps, manifests, profiles and
benchmark output outside the repository: they can contain private filenames.

The baseline is a union of raw APFS stream extents from a stable volume. A
matching `du`, clone-ID scanner, or GUI number is not an independent physical
allocation proof. [SURVEY.md](SURVEY.md) defines the quantities being compared;
[WORKLOG.md](WORKLOG.md) records the tested revisions, environments and gaps.

## Raw snapshot oracle

Use a disposable macOS guest or image. The tested snapshots are unencrypted.
Keep the read-only snapshot mounted throughout manifest and mapper export. A
snapshot pins file contents; it does not make a live container's free-space
counter atomic with the scan.

1. Check out `sgan81/apfs-fuse` at `66b86bd525e8cb90f9012543be89b1f092b75cf3`,
   including its `3rdparty/lzfse` submodule. Run
   `sh research/build_oracle.sh /path/to/apfs-fuse /path/to/snapshot-dump`. The
   builder checks both pinned revisions and clean working trees, exports their
   tracked files, and applies the exact local `apfs-fuse.patch` to the build
   copy. It uses Apple's Command Line Tools, system zlib and bzip2, and the
   pinned decoder sources. It does not install FUSE or another package. Retain
   the binary's `.build.json` provenance record.
2. Identify the snapshot transaction and its volume's index in the container.
   Run `snapshot-dump DEVICE VOLUME_INDEX SNAPSHOT_XID OUTPUT` with read
   permission for the raw container device. The reader opens it read-only and
   emits an `.identity.json` sidecar binding the dump hash to the selected raw
   snapshot metadata, frozen superblock, volume, and container. Retain stderr
   and investigate every diagnostic; reject errors and unexplained warnings.
3. Run `python3 research/snapshot_manifest.py SNAPSHOT_ROOT MANIFEST.json`. It
   requires a read-only snapshot mount, records underlying device IDs rather
   than unified System/Data IDs, and writes a `.scope` sidecar. It checks the
   kernel mount identity against Apple's snapshot listing before and after
   traversal. Traversal errors invalidate the manifest.
4. From the project root, set `DU_APFS_RESEARCH_ROOT` to the mounted snapshot
   and `DU_APFS_RESEARCH_OUTPUT` to an output JSON-lines file, then run
   `cargo test --locked --release oracle_tests::export_snapshot_extents -- --ignored --exact`.
   It requires a read-only snapshot with 4096-byte blocks and uses the
   production Rust mapper. Mapping failures fail the export.
5. Capture production `du-apfs -a --json SNAPSHOT_ROOT` output. An
   incomplete-coverage exit status is expected when external attributes cannot
   be mapped; retain both the JSON and stderr.
6. Run
   `python3 research/compare_raw.py RAW_DUMP MANIFEST.json SCAN.json SNAPSHOT_ROOT --block-size 4096 --snapshot-xid XID --mapped-extents EXPORT.jsonl --namespace-scope MANIFEST.json.scope --raw-identity RAW_DUMP.identity.json --require-snapshot-identity --strict`.

Inspect every reported gap, unexpected range, per-file mismatch, and unexplained
inode. The report validates each file's addresses against its own raw streams,
then compares the independently deduplicated union. Raw private-directory
entries can retain blocks outside the visible namespace. Unknown external
attributes must not be silently counted as zero.

Strict mode rejects contradictory addresses, sizes, unions, and unexplained
inodes. It permits explicitly reported raw stream gaps, so an exit status of
zero does not mean complete file coverage. With the identity arguments above,
the comparator verifies the dump hash and raw/mounted transaction, snapshot
name, volume UUID, and container UUID. Where the raw snapshot extension exposes
a UUID, it must also match. Older snapshots without that extension report the
weaker `xid_name_volume` binding through Apple's snapshot listing, with
`raw_uuid_directly_verified=false`. Omitting identity arguments leaves snapshot
identity unverified. Neither mode authenticates the sidecars against tampering;
retain their build provenance and input hashes with the experiment.

For the boot System snapshot, the ordinary `/` namespace crosses into Data and
other volumes. The manifest/exporter select the underlying System device. Use
`--subset` for comparison with the broader production root scan and
`--namespace-scope MANIFEST.json.scope` to match hidden mount-point inodes to
raw directory records. In the tested guest, `--synthetic-path home` explicitly
accounts for the kernel-created, unallocated symlink. Never add an exclusion
merely to make a mismatch disappear.

Sealed filesystem nodes can omit transaction headers. The reader selects their
root through the snapshot superblock, while the comparator checks transaction
IDs on nodes that have them. It does not validate Merkle hashes. Apple's
installed `apfs_checkseal -x XID RAW_VOLUME_DEVICE` provides a separate native
seal check. Supplying no external expected-root image is not a separate
boot-policy authentication check.

The raw dump begins with a live volume header because of an upstream dump
implementation detail. That header must not be used as the selected snapshot's
transaction or allocation counter.

This upstream revision also attempts to initialize a snapshot's absent
extent-reference and nested-snapshot trees. With zero root IDs it emits
`WARNING: extentref tree init failed` and `WARNING: snap meta tree init failed`.
These are auxiliary to the filesystem/fext records being compared. For this
pinned revision, a nonzero root that fails to load also emits an `ERROR` from
`BTree::Init`; that is not an accepted warning. Record the diagnostics and their
explanation alongside the oracle result.

## Container allocation oracle

`spaceman_oracle.py` reads an immutable regular disk-image file. For an image
containing a partition map, provide the APFS container's byte offset:

```sh
python3 research/spaceman_oracle.py immutable.img --offset CONTAINER_OFFSET
```

It verifies metadata checksums, selects a checkpoint, counts allocation bitmap
bits, and compares each chunk and spaceman counter. It also reads the main
pending-free queue and verifies that every queued block is still marked
allocated. Large queued ranges are checked with bitmap masks rather than a
per-block object set. Unsupported layouts and inconsistent metadata fail.

Checkpoint mappings must bind the selected container UUID, descriptor layout,
object IDs, types, and subtypes. The final mapping block must carry its
completion flag. Reports include layout and feature fields; the tested image's
three containers use 4 KiB blocks, contiguous descriptors, and no chunk-info
address blocks. Those checks do not establish support for every APFS layout.

The report separates literal bitmap-free bytes from free bytes including that
queue. The latter exactly matched Apple's `diskutil` counters in all three
containers of the stopped test VM's read-only disk clone, and matched `statfs`
on a read-only Data mount. Internal-pool free blocks remain reserved for
metadata and are not added to the main allocator's free total. This is overall
container accounting, without attributing those bytes to directory paths.

Do not run this against a changing image. The CLI rejects raw device nodes and
checks the regular image's metadata before and after reading. Deliberately
corrupted disposable copies were rejected for both a metadata checksum failure
and a bitmap/counter disagreement.

`check_spaceman_binding.py IMAGE --offset CONTAINER_OFFSET` exercises ten
checksum-valid corruptions through a read overlay, without modifying the image.
Its fixture must contain a nonempty main pending-free queue. An optional
`--baseline OLD_READER.py` also checks that the earlier reader accepted each
mutation. The tested mutations cover object IDs, mapping metadata, checkpoint
identity and geometry, completion flags, and unsupported Fusion features.

## Performance experiments

Use fresh processes and Apple's `/usr/bin/time -lp` for elapsed time and peak
RSS. Rotate the order of variants, verify identical totals and coverage on a
frozen workload, and distinguish those trials from throughput measurements on a
changing live drive. Do not equate userspace allocation counters with resident
memory or compare different machines as a language-speed result.

`benchmark.py` runs summary JSON scans with Apple's timer, rotates variant
order, records binary hashes and checks totals and coverage against an optional
verified scan. It writes stdout and stderr directly to a new private artifact
directory before validating anything, so a failure cannot discard its diagnostic
paths:

```sh
python3 research/benchmark.py --root SNAPSHOT_ROOT --output NEW_ARTIFACT_DIR \
  --expected VERIFIED_SCAN.json --rounds 4 baseline=/path/to/baseline candidate=/path/to/candidate
```

Without `--expected`, the first successful scan supplies the comparison
counters. Matching counters are a benchmark gate, not an independent correctness
oracle. Use `--allow-changing-counters` only for a clearly labeled live
throughput diagnostic. Neither mode makes a scan atomic or supplies a persistent
scan cache.

The production Rust crate is at the project root. It supports `--workers N`. Its
ignored `oracle_tests::export_snapshot_extents` test exports addresses from the
production mapper using the environment variables described above. For
detailed-output memory comparisons, pass `--scan-arg=-a` to `benchmark.py`. Do
not time profiling or compilation concurrently with a benchmark scan.
