#!/bin/sh
# Build a read-only oracle from tracked bytes in the pinned upstream revisions.
set -eu
if [ "$#" -ne 2 ]; then
    echo "usage: build_oracle.sh pristine-apfs-fuse-checkout output-binary" >&2
    exit 2
fi
source_dir=$(cd "$1" && pwd)
output=$2
research_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
expected=66b86bd525e8cb90f9012543be89b1f092b75cf3
lzfse_expected=e634ca58b4821d9f3d560cdc6df5dec02ffc93fd
submodule_dir="$source_dir/3rdparty/lzfse"
if [ "$(git -C "$source_dir" rev-parse HEAD)" != "$expected" ] ||
    [ "$(git -C "$source_dir" rev-parse HEAD:3rdparty/lzfse)" != "$lzfse_expected" ] ||
    [ -n "$(git -C "$source_dir" status --porcelain --untracked-files=all)" ] ||
    [ "$(git -C "$submodule_dir" rev-parse HEAD)" != "$lzfse_expected" ] ||
    [ -n "$(git -C "$submodule_dir" status --porcelain --untracked-files=all)" ]; then
    echo "apfs-fuse and lzfse must be clean at the pinned revisions" >&2
    exit 1
fi
patch_file="$research_dir/apfs-fuse.patch"
adapter_file="$research_dir/snapshot-dump.cpp"
patch_hash=$(shasum -a 256 "$patch_file" | awk '{print $1}')
adapter_hash=$(shasum -a 256 "$adapter_file" | awk '{print $1}')
objects=$(mktemp -d "${TMPDIR:-/tmp}/du-apfs-identity.XXXXXX")
trap 'rm -rf "$objects"' EXIT HUP INT TERM
mkdir -p "$objects/source/3rdparty/lzfse"
git -C "$source_dir" archive --output="$objects/apfs-fuse.tar" "$expected"
tar -xf "$objects/apfs-fuse.tar" -C "$objects/source"
git -C "$submodule_dir" archive --output="$objects/lzfse.tar" "$lzfse_expected"
tar -xf "$objects/lzfse.tar" -C "$objects/source/3rdparty/lzfse"
patch -p1 -F 0 -d "$objects/source" <"$patch_file"
for name in lzfse_decode lzfse_decode_base lzfse_fse lzvn_decode_base; do
    xcrun clang -O2 -c "$objects/source/3rdparty/lzfse/src/$name.c" -o "$objects/$name.o"
done
set -- "$adapter_file"
for file in "$objects/source"/ApfsLib/*.cpp; do
    case "$file" in */DeviceVDI.cpp) continue ;; esac
    set -- "$@" "$file"
done
xcrun clang++ -O2 -std=c++11 \
    "-DORACLE_PATCH_SHA256=\"$patch_hash\"" \
    "-DORACLE_ADAPTER_SHA256=\"$adapter_hash\"" \
    -I"$objects/source" -I"$objects/source/3rdparty/lzfse/src" \
    "$@" "$objects/source"/Crypto/*.cpp "$objects"/*.o -lz -lbz2 -o "$output"
python3 - "$output.build.json" "$objects/source" "$output" "$expected" "$lzfse_expected" "$patch_hash" "$adapter_hash" <<'PY'
import datetime
import hashlib
import json
import os
from pathlib import Path
import sys

out, stage, binary, upstream, lzfse, patch_hash, adapter_hash = sys.argv[1:]

def sha(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()

stage = Path(stage)
record = {
    'schema_version': 1,
    'built_at_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
    'upstream_commit': upstream,
    'lzfse_submodule_commit': lzfse,
    'apfs_fuse_patch_sha256': patch_hash,
    'adapter_sha256': adapter_hash,
    'patched_upstream_files_sha256': {
        name: sha(stage / name) for name in (
            'ApfsLib/ApfsContainer.h', 'ApfsLib/ApfsVolume.h',
            'ApfsLib/ApfsVolume.cpp')
    },
    'binary_sha256': sha(binary),
}
target = Path(out)
temporary = Path(str(target) + '.tmp')
temporary.write_text(json.dumps(record, indent=2) + '\n')
os.replace(temporary, target)
PY
