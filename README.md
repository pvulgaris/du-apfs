# du-apfs

`du-apfs` is a read-only, APFS-aware alternative for common `du` invocations on
macOS. It reports distinct physical blocks referenced by each path, including
partial APFS clones and sparse files.

`du` counts a cloned file's blocks once per file, so it overstates usage
wherever APFS clones share data:

```console
$ cp -c photo-library.dmg copy.dmg
$ du -sh .
200M	.
$ du-apfs -sh .
100.0M	.
```

Install the Apple silicon release binary with [mise](https://mise.jdx.dev), or
download it from the [releases page](https://github.com/pvulgaris/du-apfs/releases):

```sh
mise use -g github:pvulgaris/du-apfs
```

Building from source requires Rust 1.85 or later and Apple's Command Line Tools:

```sh
cargo install --locked --git https://github.com/pvulgaris/du-apfs du-apfs
```

```sh
du-apfs -sh ~/.tart
du-apfs -h -d1 ~/Library
du-apfs -B1 -s ~/Projects
du-apfs -s --json / > disk.json
```

The CLI accepts `-a`, `-s`, `-d`, `-h`, `-k`, `-m`, `-g`, `-B`, `-c`, `-x`, and
`-0`, including combined forms such as `-sh`. It also accepts `--json` and the
corresponding long options, plus `--workers N` and `--version`. Default numeric output uses
512-byte blocks, as macOS `du` does. Unsupported flags fail explicitly. `-c`
computes a distinct union across all operands.

If an operand is missing or unsupported, other operands still produce results
and the command exits nonzero. JSON records those failures, with their original
argument indices, in `coverage.operand_failures`. A failed operand has no
numeric text row. Diagnostics on stderr name the affected paths and are bounded.

Release builds use ThinLTO. Rust is the only maintained implementation.

Before resolving scan paths, the scanner disables dataless-file materialization
through macOS's public I/O policy API and verifies the policy. This covers
iCloud and Dropbox File Provider placeholders. If a metadata or mapping request
requires a download, its failure contributes to incomplete coverage. Online-only
files are not assumed to use zero local blocks.

Use `-s` or `-d0` for large-directory scans. These modes retain only one
directory record and use a bitmap to count distinct APFS allocation blocks as
files are scanned. `-d1` uses sparse bitmap chunks for top-level children.
Deeper tree output keeps extent events for independent subtree totals and uses
more memory. The scanner maps regular-file extents with macOS `F_LOG2PHYS_EXT`;
it does not substitute `st_blocks` for physical usage.

JSON identifies this measurement as `distinct_file_fork_extents` and reports
`physical_bytes_by_container`. Snapshot aliases within a container share one
physical address space. A mounted disk image belongs to a different container
from its backing file, so use the appropriate bucket when comparing against host
capacity. File extents exclude filesystem metadata and unseen snapshots; even
complete file coverage is not a complete inventory of container occupancy.

Each reported directory is measured independently. If two children share blocks,
their sizes can sum to more than their parent's size. A number is the physical
space referenced within that subtree, not necessarily space that deleting it
would reclaim. Snapshots, open files, and paths outside the scan may still
reference its blocks.

Compressed files are mapped through their resource fork when needed. Some
compressed extended-attribute allocations cannot be mapped through the public
API. Errors, skipped paths, and unmeasured bytes appear in JSON `coverage`.

| Exit status | Meaning |
| ----------- | ------- |
| 0 | Every requested path was measured. |
| 1 | Results were written, but an error or unmeasured allocation left them incomplete. stderr names the first ten errors; `--json` lists them in `coverage`. |
| 2 | Invalid arguments. |

As with `du`, a closed output pipe ends the process with `SIGPIPE`.

A full `/` scan omits the alternate `/System/Volumes/Data` path to
avoid scanning the Data volume twice. `-x` restricts each operand to its
starting filesystem.

`coverage.unmeasured_bytes` counts allocated bytes on entries whose extents
could not be fully mapped. It is not an estimate of physical bytes inside
unreadable directories. JSON separately reports their count and up to 100 paths.
These allocation references cannot be deduplicated without their addresses;
adding them to the mapped total does not produce a distinct physical total. If
any directory is unreadable, no path scan can claim a complete whole-disk
physical total. A directory whose own listing fails has no numeric text row;
JSON retains its measured lower bound with `enumeration_complete: false`. Parent
rows can still contain measured bytes from accessible children, so always check
the exit status and overall coverage before treating a total as complete.
Extended attributes other than the resource fork are not mapped. JSON counts
entries that have them in `coverage.unmapped_xattr_entries`, including
directories. The count is conservative: it includes small attributes embedded in
filesystem metadata, which occupy no file blocks, so it does not affect the exit
status or `coverage.complete`. Large attributes stored as separate streams do
occupy space that totals omit.

`filesystem.used_bytes` in JSON is statfs capacity minus its free-space counter
for the first scanned APFS operand. These counters are sampled before output;
`filesystem` is `null` when no such operand exists or the control query fails. A
volume quota can limit those counters, so they are not an unconditional
container total. Apple's container accounting can also include blocks queued for
freeing that are still set in on-disk allocation bitmaps. The counters need not
equal visible file usage because APFS snapshots, other volumes, filesystem
metadata, and restricted files can occupy space outside the scanned namespace.

## Verification and benchmarks

From a checkout, run `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked --release`, and
`python3 -m unittest discover -s research -p 'test_*.py'`.
CI runs these checks on macOS. Native tests require APFS with 4096-byte blocks.
The raw-snapshot export test is explicitly ignored in ordinary runs because it
requires a separately prepared read-only snapshot and independent raw records.
Passing CI does not establish whole-disk coverage or rerun the historical raw
snapshot comparisons described below.

The production mapper was checked against raw records from frozen APFS
snapshots: every mapped range belongs to its raw inode, and the independently
calculated union matches the scan. External attribute streams remain
unmeasured, and APFS's private directory holds allocations a namespace walk
cannot see. See [the measurement evidence](research/WORKLOG.md) for scope,
benchmarks and remaining gaps. These fixture proofs do not establish complete
live-disk coverage.

For a separate container control total, the research helper can compare a JSON
scan with Apple's `diskutil apfs list -plist` counters:

```sh
python3 research/reconcile.py disk.json
```

It keeps containers separate and reports occupied bytes minus mapped bytes. That
remainder includes more than snapshots and does not estimate deletion savings.
The counters are sampled after the scan, not atomically with it; compare them
before mounts change or device identifiers are reassigned.

## License

MIT, except two research files derived from apfs-fuse; see
[research/README.md](research/README.md).
