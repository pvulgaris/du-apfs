# Measurement baseline and scanner survey

Research date: 2026-09-23. This is a survey of discovered relevant projects, not
a claim to enumerate every disk scanner ever published.

## What an exact answer means

| Quantity                      | Independent evidence                                                                                    | What it does not mean                   |
| ----------------------------- | ------------------------------------------------------------------------------------------------------- | --------------------------------------- |
| Logical file length           | inode/data-stream length                                                                                | occupied storage                        |
| Allocated references          | inode allocation and fork allocation                                                                    | distinct blocks across clones           |
| Unique physical stream bytes  | union of raw APFS file-extent records, including external xattr streams                                 | filesystem metadata or deletion savings |
| Container occupied bytes      | APFS spaceman allocation and container capacity/free counters                                           | bytes attributable to one directory     |
| Space reclaimable by deletion | references from all live files, snapshots, and open unlinked files, plus filesystem transaction effects | sum of sizes in the selected namespace  |

The ideal fixture oracle is a quiescent, unmounted APFS image. A read-only
parser uses Apple's published on-disk format to identify streams and union their
block ranges. This is independent of the scanner's kernel mapping API. Container
allocation is a separate control total. The difference includes metadata and
allocation outside the selected streams; it must not be silently distributed
among files or described as an error in the file-extent union.

A scan of a changing mounted volume is not an atomic snapshot. Permission
failures, unexposed xattr stream addresses, and snapshots outside the namespace
must remain explicit. A passing fixture comparison does not establish 100%
coverage of a live system disk.

## Discovered implementations

| Project                                                      | Measurement and implementation                                                             | Useful technique or limitation                                                                                                         |
| ------------------------------------------------------------ | ------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------- |
| macOS `du`, GNU `du`                                         | allocated references with hardlink handling                                                | no APFS partial-clone extent union                                                                                                     |
| [DaisyDisk](https://daisydiskapp.com/guide/4/en/HardLinks)   | counts the first full clone; later clones contribute zero                                  | its own documentation says partial clones are not correctly accounted for; unsuitable as an exact oracle                               |
| [duh](https://github.com/cheapsteak/duh)                     | Rust, bulk attributes, clone-ID groups, SQLite scan database                               | useful bulk traversal; modified clones have different IDs despite retaining shared extents                                             |
| [dua](https://github.com/Byron/dua-cli)                      | Rust, native macOS bulk metadata; optional full-clone deduplication                        | README explicitly excludes partial clones from deduplication; separates data-fork clone identity                                       |
| [diskus](https://github.com/sharkdp/diskus)                  | Rust parallel traversal, `metadata.blocks() * 512` and hardlink identities                 | useful bounded worker tuning; no distinct APFS extent union in inspected implementation                                                |
| [gdu](https://github.com/dundee/gdu)                         | Go concurrent scanning, Unix `stat.Blocks * 512`                                           | useful traversal and coverage handling; allocated references are not clone-aware physical usage                                        |
| [ncdu](https://dev.yorhel.nl/ncdu)                           | C or Zig; parallel scan and compact export                                                 | useful memory-oriented representation; not independently established as an APFS extent oracle                                          |
| [dust](https://github.com/bootandy/dust/issues/590)          | Rust disk-usage tree                                                                       | upstream issue documents shared-extent overcounting                                                                                    |
| [MacDirStat](https://github.com/jaenster/macdirstat)         | Zig, native macOS tree scanner                                                             | name interning saves memory; README explicitly acknowledges APFS clone overcounting                                                    |
| [GrandPerspective](https://grandperspectiv.sourceforge.net/) | open-source Objective-C disk usage GUI                                                     | logical/allocated display and hardlinks; no verified partial-clone extent implementation established in this survey                    |
| Apple `sm_stats`                                             | installed APFS space-manager statistics, including live-container free-space fragmentation | useful native container diagnostic; displayed distribution sizes are rounded, not per-file attribution                                 |
| Apple `apfs_diskimage_map`                                   | installed disk-image inspection utility with JSON export                                   | on the owned unmounted fixture it reported compressed-file extents only, so it is not a general directory-usage scanner                |
| [apfs-fuse](https://github.com/sgan81/apfs-fuse)             | C++ read-only raw APFS parser and dump tools                                               | built locally and used to verify stream addresses without F_LOG2PHYS_EXT; encrypted internal drives have additional access limitations |
| [libfsapfs](https://github.com/libyal/libfsapfs)             | experimental C read-only APFS library                                                      | supports xattrs and several compression methods; lists snapshots and T2 encryption as unsupported                                      |
| [go-apfs-v2](https://github.com/deploymenttheory/go-apfs-v2) | Go APFS reader/writer for disk images                                                      | independently generated snapshot fixtures may help when native snapshot creation requires root                                         |
| [Sleuth Kit](https://github.com/sleuthkit/sleuthkit)         | forensic filesystem readers including APFS                                                 | another possible raw-image cross-check, not a normal live directory scanner                                                            |

Installed inventory found DaisyDisk, native/GNU `du`, the original Python
prototype, and the Go du-apfs binaries. The installed `gdu` command is GNU du,
not the separate Go project. A local apfs-dedupe checkout also uses fclones for
content duplicate detection; that does not establish physical block ownership.

## Darwin interfaces inspected

- `F_LOG2PHYS_EXT`: XNU translates logical offsets through VNOP_BLOCKMAP and
  returns a device-relative physical address and contiguous length. Sparse holes
  use a negative physical address.
- `getattrlistbulk`: batches inode metadata. Returned-attribute masks must be
  checked; packed fields can be unaligned. Reuse the buffer between directories.
- `ATTR_CMNEXT_CLONEID`: identifies a data stream, not every stream sharing some
  of its blocks. Modified clones can have different IDs.
- `ATTR_CMNEXT_PRIVATESIZE`: documented as bytes not retained by a clone or
  snapshot. The fixture probe shows it does not include ordinary resource forks.
- `EF_MAY_SHARE_BLOCKS` can be clear while a file shares blocks with its
  snapshot. A metadata-only fast path using that flag is therefore unsafe across
  snapshot aliases. No such fast path has been enabled. `EF_NO_XATTRS` avoids
  unnecessary attribute queries when supplied by the bulk metadata API.
- Public named-fork lookup in XNU specifically requests
  `com.apple.ResourceFork`. General xattr access returns values, not the
  physical addresses of external attribute streams.

The follow-up API review found no documented userspace selector for mapping an
arbitrary external xattr stream. `getxattr(name, NULL, 0)` supplies a logical
length, not its allocated or shared blocks. The APFS format instead resolves an
external xattr's stream ID through raw filesystem records. `PRIVATESIZE` is a
scalar and cannot be unioned across partially overlapping files; clone reference
counts describe full clones. Neither is a replacement for physical extent
addresses. This is a limitation of the reviewed interfaces, not proof that every
private APFS interface has been exhausted.

Primary references:
[Apple APFS format](https://developer.apple.com/support/downloads/Apple-File-System-Reference.pdf),
[XNU mapping](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_descrip.c),
[XNU attributes](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_attrlist.c),
[XNU named forks](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/vfs/vfs_lookup.c),
[Apple getxattr](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/getxattr.2.html),
[XNU getattrlist contract](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/getattrlist.2).
