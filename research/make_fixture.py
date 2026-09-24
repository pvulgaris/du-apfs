#!/usr/bin/env python3
"""Create deterministic allocation cases below a new, explicitly supplied directory."""

import ctypes
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1])
root.mkdir()
for part in ("regular", "clones", "forks", "compressed", "names"):
    (root / part).mkdir()
for size in (0, 1, 4095, 4096, 4097, 65537, 1048576):
    (root / "regular" / str(size)).write_bytes(os.urandom(size))
source = root / "clones" / "original"
source.write_bytes(os.urandom(1 << 20))
for name in ("full", "partial", "truncate", "extend"):
    subprocess.run(
        ["cp", "-c", str(source), str(source.with_name(name))], check=True
    )
with source.with_name("partial").open("r+b") as f:
    f.seek(1 << 18)
    f.write(os.urandom(4096))
with source.with_name("truncate").open("r+b") as f:
    f.truncate(100000)
with source.with_name("extend").open("ab") as f:
    f.write(os.urandom(8192))
os.link(source, source.with_name("hardlink"))
with (root / "regular" / "sparse").open("wb") as f:
    f.truncate(1 << 30)
    f.seek(1 << 29)
    f.write(os.urandom(4096))
for name, data in [("resource-only", b""), ("both-forks", b"data" * 1024)]:
    p = root / "forks" / name
    p.write_bytes(data)
    with open(str(p) + "/..namedfork/rsrc", "wb") as f:
        f.write(os.urandom(65536))
xattr = root / "forks" / "external-xattr"
xattr.write_bytes(b"hello")
value = os.urandom(65536)
libc = ctypes.CDLL(None, use_errno=True)
libc.setxattr.argtypes = [
    ctypes.c_char_p,
    ctypes.c_char_p,
    ctypes.c_void_p,
    ctypes.c_size_t,
    ctypes.c_uint32,
    ctypes.c_int,
]
if (
    libc.setxattr(
        os.fsencode(xattr), b"user.du_apfs_test", value, len(value), 0, 0
    )
    != 0
):
    raise OSError(ctypes.get_errno(), "setxattr")
for name, size in [("inline", 1024), ("large", 2 << 20)]:
    p = root / "compressed" / (name + "-source")
    p.write_bytes((b"APFS compression fixture\n" * (size // 25 + 1))[:size])
    subprocess.run(
        [
            "ditto",
            "--noclone",
            "--hfsCompression",
            str(p),
            str(p.with_name(name)),
        ],
        check=True,
    )
(root / "names" / "unicode-é-文件").write_bytes(b"name")
(root / "names" / "line\nbreak").write_bytes(b"newline")
os.symlink("../clones/original", root / "names" / "symlink")
rows = []
for p in root.rglob("*"):
    st = p.lstat()
    rows.append(
        {
            "path": str(p.relative_to(root)),
            "inode": st.st_ino,
            "size": st.st_size,
            "allocated_bytes": st.st_blocks * 512,
            "flags": st.st_flags,
        }
    )
print(json.dumps(rows, indent=2))
