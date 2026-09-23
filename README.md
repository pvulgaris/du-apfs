# du-apfs

`du-apfs` is a read-only, APFS-aware alternative for common `du` invocations on
macOS. It reports distinct physical blocks referenced by each path, including
partial APFS clones and sparse files. The Go source is self-contained here so it
can move to its own repository. The only dotfiles integration is the
`bin/du-apfs` launcher, which builds the binary on first use into
`~/.local/share/du-apfs/`.

```sh
bin/du-apfs -sh ~/.tart
bin/du-apfs -h -d1 ~/Library
bin/du-apfs -B1 -s ~/Projects
bin/du-apfs -s --json / > disk.json
(cd src/du-apfs && go test ./...)
```

The CLI accepts `-a`, `-s`, `-d`, `-h`, `-k`, `-m`, `-g`, `-B`, `-c`, `-x`, and
`-0`, including combined forms such as `-sh`. It also accepts `--json` and the
corresponding long options. Default numeric output uses 512-byte blocks, as
macOS `du` does. Unsupported flags fail explicitly. `-c` computes a distinct
union across all operands.

Use `-s` or `-d0` for whole-disk totals. These modes retain only one directory
record and use a bitmap to count distinct APFS allocation blocks as files are
scanned. `-d1` uses sparse bitmap chunks for top-level children. Deeper tree
output keeps extent events for independent subtree totals and uses more memory.
The scanner maps regular-file extents with macOS `F_LOG2PHYS_EXT`; it does not
substitute `st_blocks` for physical usage.

Each reported directory is measured independently. If two children share blocks,
their sizes can sum to more than their parent's size. A number is the physical
space referenced within that subtree, not necessarily space that deleting it
would reclaim. Snapshots, open files, and paths outside the scan may still
reference its blocks.

Compressed files are mapped through their resource fork when needed. Some
compressed extended-attribute allocations cannot be mapped through the public
API. Errors, skipped paths, and unmeasured bytes appear in JSON `coverage`; the
command still writes results but exits nonzero when its physical measurement is
incomplete. A full `/` scan omits the alternate `/System/Volumes/Data` path to
avoid scanning the Data volume twice. `-x` restricts each operand to its
starting filesystem.

`coverage.unmeasured_bytes` counts allocated bytes on entries whose extents
could not be fully mapped. It is not an estimate of physical bytes inside
unreadable directories. JSON separately reports their count and up to 100 paths.
If any directory is unreadable, no path scan can claim a complete whole-disk
physical total. Compare with `filesystem.used_bytes`, the APFS container-level
figure, when assessing the entire disk.

`filesystem.used_bytes` in JSON comes from the APFS shared-container capacity
and free count. It need not equal the sum of visible files because APFS
snapshots, other volumes, filesystem metadata, and restricted files can occupy
space outside the scanned namespace. DaisyDisk also uses a different displayed
folder-size rule for full and partial clones, and its access permissions may
differ from a terminal scan.

## Whole-disk benchmark

On 2026-09-23, a local macOS 27.0 arm64 machine with 12 CPUs and 48 GiB RAM gave
these sequential `/` scan results. The disk was live, so file counts and
physical totals changed slightly between runs. Resident memory is the largest
sample observed during each run, not a measured peak.

| Scanner                          |          Elapsed |    Files seen | Sampled resident memory |
| -------------------------------- | ---------------: | ------------: | ----------------------: |
| Go `du-apfs -s --json /`         |          200.6 s | 6.415 million |                  126 MB |
| Python prototype, same traversal |          337.6 s | 6.398 million |                 3.63 GB |
| DaisyDisk, Macintosh HD rescan   | under about 80 s |   unavailable |             unavailable |

The Python prototype used the same APFS extent call but retained Python
directory and extent objects. Go was about 1.68 times as fast in these runs and
used far less memory. The Go scanner first measured 267.8 s and sampled 194 MB
before its walker/mapper pipeline, three-worker tuning, and metadata allocation
reductions. A `/ -d1` pass took 196.9 s and sampled 1.02 GB with the event-based
tree, versus 194.2 s and 207 MB with sparse bitmap chunks. Those memory samples
were taken around four million files. DaisyDisk's completion was inferred from
its scan CPU activity because its window was unavailable during this run, so its
time is only a coarse upper bound. Its scan may also cover fewer protected paths
and its display rules differ, so it is not an equivalent byte-for-byte workload.
