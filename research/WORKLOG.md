# Measurement evidence

Investigated 2026-09-23 through 2026-09-24. This records accepted findings,
reproducible checks, and their limits. Transient experiments, exact historical
binaries, profiles, private manifests, and raw dumps are kept outside the
project because they can contain private paths. The retired Go implementation
remains in version history. [README.md](README.md) describes the retained
verification tools; [SURVEY.md](SURVEY.md) explains the measurement and reviewed
interfaces.

## Production contract

The scanner measures the distinct physical data/resource-fork extents referenced
by a namespace. It does not measure all container occupancy or deletion savings.
Partial clones must union their actual ranges. Live and snapshot aliases in the
same APFS container share an address domain even when their mount devices
differ. Different containers must stay separate.

Native fixtures exposed and now cover these cases:

- A live file and its snapshot alias refer to one 4096-byte allocation, despite
  different mount device IDs. Independent files on two volumes of one container
  total 8192 bytes in one container bucket.
- Clone IDs change after partial modification. `EF_MAY_SHARE_BLOCKS` can be
  clear while a snapshot still retains the file's blocks. Neither scalar
  provides a substitute for physical extent mapping.
- Ordinary resource forks must be mapped independently. Compressed resource
  fallback must discard partial failed data-fork mappings. Final inode identity,
  allocation, modification time, and change time checks reject races.
- `F_LOG2PHYS_EXT` can map preallocation beyond EOF. Requesting at least one
  allocation block also handles short tails on system asset volumes. A FIFO
  substituted after enumeration must not block the mapper.
- General external xattr addresses are unavailable through the reviewed public
  APIs. Their presence is explicitly unknown; logical xattr length is not a
  physical allocation estimate. Directory attributes also count.
- The dataless materialization policy is disabled before path resolution and
  remains disabled through worker joins and output. An overriding thread policy
  is rejected. iCloud and Dropbox placeholders are not assumed to consume zero
  physical blocks or permitted to trigger downloads for measurement.
- Missing and unsupported operands leave valid operands usable. Unreadable
  directories suppress misleading zero text rows and retain explicit JSON
  coverage failures. Final symlinks use their containing filesystem, including
  dangling links and cross-filesystem targets. Output uses captured controls so
  removing a path after scanning cannot discard the completed report.

## Independent file-extent proof

The raw reader is pinned to apfs-fuse
`66b86bd525e8cb90f9012543be89b1f092b75cf3`, with decoder sources and the local
patch recorded by `build_oracle.sh`. Apple Command Line Tools build it without
installing FUSE. Raw checks used frozen, unencrypted macOS 26.6.2 snapshots. The
mapper exporter calls the production Rust mapping code.

| Scope                           | Mapped entries |     Mapped union | Unmapped raw visible streams |
| ------------------------------- | -------------: | ---------------: | ---------------------------: |
| Data snapshot, XID 5116         |        123,597 |  6,249,107,456 B |                149,975,040 B |
| Sealed System snapshot, XID 384 |        316,026 | 12,389,412,864 B |                          0 B |

Every exported range belongs to its corresponding raw inode stream. Both strict
comparisons found zero unexpected ranges, per-file size mismatches, or
unexplained inodes. The fresh Rust exporter reproduced the preserved Data
address export byte for byte. Its binary SHA-256 was
`fc9ba85350c4e3e7ee4ecbf8ab40332cba0e3e68117a3162d8be6c0d7098f2f2`.

The Data gap consists of 2,105,344 bytes in 514 external compressed attributes
and 147,869,696 bytes in 120 directory attributes. Its raw visible-stream union
is 6,399,082,496 bytes. The namespace also omits APFS's private directory and 45
retained dead entries; their allocation is outside visible paths. Mapping
preallocated tails recovered 38,215,680 distinct bytes previously left
unmeasured, verified by exact raw addresses rather than allocation-counter
parity.

The Data snapshot's raw UUID matches the mounted snapshot. System identity is
bound by raw transaction/name/base-volume/container and Apple's snapshot
listing; its raw UUID is unavailable. The System manifest records 25 mount
boundaries and the separately justified synthetic, unallocated `home` entry. A
broader root scan requires `--subset`; its global union is not a System-only
measurement.

The comparator distinguishes index records from leaf records and accepts omitted
sealed-node headers only with `BTNODE_NOHEADER`. Thirteen synthetic tests cover
these distinctions, strict failure conditions, identity mismatches, and
dump-hash binding on local Python and the research host's Python 3.9. It does
not validate Merkle seals or authenticate sidecars against tampering. Strict
success proves the mapped bytes while retaining reported raw-stream gaps.

## Container control proof

On an immutable VM disk clone, main-container checkpoint 6555 yielded:

| Counter                 |          Bytes |
| ----------------------- | -------------: |
| Capacity                | 44,107,001,856 |
| Literal bitmap free     | 13,955,731,456 |
| Literal bitmap occupied | 30,151,270,400 |
| Main pending-free queue |         61,440 |
| Apple free              | 13,955,792,896 |

Adding the 15 pending-free blocks to bitmap-free bytes exactly matched Apple's
`diskutil` and a read-only Data mount's `statfs`. The other two containers also
matched after their 13 and 12 pending-free blocks. Internal-pool free blocks
remain reserved for metadata and are not added to the main allocator total.

The reader checks checksums, bitmap counts, checkpoint UUID/geometry/completion,
and mapping object IDs, types, subtypes, and filesystem IDs. Ten checksum-valid
corruptions were rejected in each container without changing image bytes;
checksum and bitmap/counter corruption were also rejected. Tested layouts use 4
KiB blocks, contiguous descriptors, no chunk-info address blocks, spaceman
flags/version 1, and container incompatible features 2. Unsupported layouts
fail. This fixture result does not establish a formula for every APFS layout.

A quota fixture limits `statfs` capacity to 256,000,000 bytes in a
2,147,442,688-byte container. Therefore volume controls cannot be labeled
unconditional container occupancy. A guest Storage Settings observation showed
roughly 22.4 GB used versus 27.8 GB physical capacity minus free;
important-usage available capacity included roughly 5.26 GB beyond physical
free. These were nearby live samples, not an exact equality proof. Sandboxed
`statfs` counters were rounded, so small reclamation experiments require
unsandboxed native controls.

## Performance decisions

Apple M4 Pro, 12 CPUs, 48 GiB RAM, macOS 27.0 arm64. Peak RSS is measured by
Apple's `/usr/bin/time -lp`, not lifetime allocator totals. File opens dominate
native samples. Detailed benchmark output and binary hashes remain archived.

| Trial                                                       | Result                                                         | Interpretation                                                                          |
| ----------------------------------------------------------- | -------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| Alternating Go/Rust/Rust/Go whole-root runs                 | Go 185.9 s mean, 54.6–56.9 MB RSS; Rust 178.6 s, 29.7–30.6 MB  | Live, incomplete scans of about 6.157 million entries; not a universal language ranking |
| Later installed Rust whole-root diagnostic                  | 133.58 s, 29,655,040 B RSS, 5,974,051 files                    | Before final CLI edge-case fixes; incomplete live scan                                  |
| Eight rotated detailed-JSON runs                            | Median RSS 424,640,512 to 53,796,864 B; elapsed 1.96 to 1.90 s | Streaming preserves all 172,993 entries exactly                                         |
| `-a -d0` retention change                                   | Median RSS 53,379,072 to 15,982,592 B; about 1.79 s            | Retain only requested output nodes                                                      |
| Full-page bitmap experiment, 12 summary and 12 shallow runs | Summary medians 1.775/1.770 s; shallow both 1.780 s            | No meaningful benefit; rejected; all 2,442 shallow entries agree                        |

The 133.58-second scan reported 285,601,390,592 mapped bytes, 251 unreadable
directories, 61,333,504 known unmeasured bytes, and 4,632,422 unknown-attribute
entries. These numbers are historical observations, not current disk usage.

Go PGO, `GOARM64=v8.2`, Rust native-CPU compilation, alternative queue handling,
resource-first mapping, `O_EVTONLY`, and descriptor-relative opening did not
establish useful repeatable gains. The production build keeps portable ThinLTO,
three mapping workers, bounded queues, deferred path construction, paged bitmaps
for shallow output, and an event sweep for deeper subtree unions.

An earlier DaisyDisk activity observation suggested about 80 seconds, but its
scope was unverified. The guest GUI trial required activation and produced no
timed scan. DaisyDisk also documents different partial-clone accounting. There
is no accepted equivalent-workload DaisyDisk speed comparison.

## Final migration checkpoint

The source was based on `4035c2f`. On macOS 27.0 arm64, 28 Rust release tests
passed; the separately exercised snapshot exporter was ignored in the ordinary
suite. Formatting, launcher checks, and 13 independent comparator tests passed.
The final installed binary SHA-256 was
`23648062496cce92d552a4de0479846e4971e5fbbbcaed67c7382d04ac069549`.

Eight installed-command fixtures covered missing first/last/all operands,
unsupported filesystems, dangling/cross-filesystem symlinks, duplicates, and
unreadable directory text/JSON behavior. An immutable framework scan reproduced
110,409 files and 4,685,074,432 mapped bytes, with zero mapping errors or known
unmeasured bytes and 39,704 conservative unknown-attribute entries. Its exit
status correctly indicated incomplete coverage.

The private receipt binds exact source hashes, environment, build artifact,
installed binary, and results. The owned test VM was stopped and removed after
preserving its immutable image and all oracle evidence. Fixture proofs do not
establish complete live-disk coverage; attributes, access restrictions, unseen
snapshots, and filesystem metadata remain explicit limits.

## Reduction verification

Three reduction rounds removed the retired Go source, condensed this evidence,
removed duplicate output filtering and retained node state, reused native
mapping results across the worker boundary, and shared fixture setup. The
scanner owns output selection; rendered-path tests cover summary, depth, and
file inclusion. The native mapper, materialization guard, and snapshot exporter
are unchanged.

After all Rust cuts, a fresh build passed 27 release tests, with only the
snapshot exporter ignored. Forty-four installed-command comparisons matched the
previous Rust binary's text or normalized JSON, including failed/duplicate
operands and symlinks. Five isolated mutations were rejected: duplicate
physical-block counts, ignored depth, reversed output order, hidden unknown
attributes, and enabled materialization. The 13 raw comparator tests also
passed.

Eight alternating detailed scans of immutable frameworks matched all 172,993
ordered entries, 110,409 files, 4,685,074,432 mapped bytes, and coverage
counters. Median elapsed was 4.470 seconds before and 4.325 seconds after; peak
RSS medians were 52,477,952 and 50,708,480 bytes. Timing varied across runs; no
speedup is claimed. The rebuilt installed binary's SHA-256 is
`e141ecc85d145495065fdf2fb2bab059e410f529c2c63f52c52a5470e06b2c9a`. These
comparisons verify the reductions against the previous Rust implementation; they
do not replace the independent raw-snapshot proof or remove its limits.
