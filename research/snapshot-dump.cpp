// SPDX-License-Identifier: GPL-2.0-or-later
// Builds only against apfs-fuse's ApfsLib, so it carries apfs-fuse's license.
// Read-only adapter for apfs-fuse's snapshot tree reader, not its FUSE mount.
#include <cstdio>
#include <array>
#include <CommonCrypto/CommonDigest.h>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <memory>
#include <sstream>
#include <string>
#include <ApfsLib/Device.h>
#include <ApfsLib/ApfsContainer.h>
#include <ApfsLib/ApfsVolume.h>
#include <ApfsLib/BlockDumper.h>
#include <ApfsLib/Util.h>

#ifndef ORACLE_PATCH_SHA256
#error ORACLE_PATCH_SHA256 must be supplied by the pinned builder
#endif
#ifndef ORACLE_ADAPTER_SHA256
#error ORACLE_ADAPTER_SHA256 must be supplied by the pinned builder
#endif

static std::string json_string(const std::string &value) {
    std::ostringstream out;
    out << '"';
    for (unsigned char ch : value) {
        if (ch == '"' || ch == '\\') out << '\\' << static_cast<char>(ch);
        else if (ch < 0x20) {
            out << "\\u00" << std::hex << std::uppercase << std::setw(2)
                << std::setfill('0') << static_cast<unsigned>(ch) << std::dec;
        } else out << static_cast<char>(ch);
    }
    out << '"';
    return out.str();
}

static std::string sha256_file(const char *path) {
    std::ifstream input(path, std::ios::binary);
    if (!input) throw std::runtime_error("cannot reopen dump for hashing");
    CC_SHA256_CTX context;
    CC_SHA256_Init(&context);
    std::array<char, 1 << 20> buffer;
    while (input) {
        input.read(buffer.data(), buffer.size());
        const auto count = input.gcount();
        if (count) CC_SHA256_Update(&context, buffer.data(), static_cast<CC_LONG>(count));
    }
    if (!input.eof()) throw std::runtime_error("error hashing dump");
    unsigned char digest[CC_SHA256_DIGEST_LENGTH];
    CC_SHA256_Final(digest, &context);
    std::ostringstream out;
    out << std::hex << std::setfill('0');
    for (unsigned char byte : digest) out << std::setw(2) << static_cast<unsigned>(byte);
    return out.str();
}

int main(int argc, char **argv) {
    if (argc != 5) {
        std::cerr << "usage: snapshot-dump device volume-index snapshot-xid output\n";
        return 2;
    }
    try {
        const auto index = std::stoul(argv[2]);
        const auto xid = std::stoull(argv[3]);
        if (xid == 0) {
            std::cerr << "a nonzero snapshot transaction is required\n";
            return 2;
        }
        std::unique_ptr<Device> device(Device::OpenDevice(argv[1]));
        if (!device) return 1;
        ApfsContainer container(device.get(), 0, device->GetSize());
        if (!container.Init()) return 1;
        std::unique_ptr<ApfsVolume> volume(container.GetVolume(index, {}, xid));
        if (!volume || volume->selectedSnapshotXid() != xid) return 1;
        const auto &sb = volume->selectedSuperblock();
        std::cerr << "Selected snapshot volume: " << volume->name()
                  << " xid=" << volume->selectedSnapshotXid()
                  << " name=" << volume->selectedSnapshotName() << '\n';
        std::ofstream output(argv[4]);
        if (!output) return 1;
        BlockDumper dump(output, container.GetBlocksize());
        // Upstream prints the live volume header before the selected snapshot
        // tree. The sidecar below contains the selected superblock identity.
        volume->dump(dump);
        output.close();
        if (!output.good()) return 1;

        const std::string dump_sha256 = sha256_file(argv[4]);
        const std::string path = std::string(argv[4]) + ".identity.json";
        const std::string temporary = path + ".tmp";
        std::ofstream identity(temporary);
        if (!identity) return 1;
        identity << "{\n"
                 << "  \"schema_version\": 1,\n"
                 << "  \"apfs_fuse_commit\": \"66b86bd525e8cb90f9012543be89b1f092b75cf3\",\n"
                 << "  \"apfs_fuse_patch_sha256\": " << json_string(ORACLE_PATCH_SHA256) << ",\n"
                 << "  \"adapter_sha256\": " << json_string(ORACLE_ADAPTER_SHA256) << ",\n"
                 << "  \"dump_sha256\": " << json_string(dump_sha256) << ",\n"
                 << "  \"volume_index\": " << index << ",\n"
                 << "  \"container_uuid\": " << json_string(uuidstr(container.uuid())) << ",\n"
                 << "  \"base_volume_uuid\": " << json_string(uuidstr(volume->liveVolumeUuid())) << ",\n"
                 << "  \"volume_group_uuid\": " << json_string(uuidstr(sb.apfs_volume_group_id)) << ",\n"
                 << "  \"volume_role\": " << sb.apfs_role << ",\n"
                 << "  \"snapshot_xid\": " << volume->selectedSnapshotXid() << ",\n"
                 << "  \"snapshot_name\": " << json_string(volume->selectedSnapshotName()) << ",\n"
                 << "  \"snapshot_uuid\": ";
        if (volume->hasSelectedSnapshotUuid())
            identity << json_string(uuidstr(volume->selectedSnapshotUuid()));
        else
            identity << "null";
        identity << ",\n"
                 << "  \"snapshot_superblock_paddr\": " << volume->selectedSuperblockPaddr() << ",\n"
                 << "  \"snapshot_superblock_oid\": " << sb.apfs_o.o_oid << ",\n"
                 << "  \"snapshot_superblock_xid\": " << sb.apfs_o.o_xid << ",\n"
                 << "  \"filesystem_root_oid\": " << sb.apfs_root_tree_oid << ",\n"
                 << "  \"sealed_fext_tree_oid\": " << sb.apfs_fext_tree_oid << ",\n"
                 << "  \"sealed\": " << ((sb.apfs_incompatible_features & APFS_INCOMPAT_SEALED_VOLUME) ? "true" : "false") << ",\n"
                 << "  \"block_size\": " << container.GetBlocksize() << "\n"
                 << "}\n";
        identity.close();
        if (!identity.good() || std::rename(temporary.c_str(), path.c_str()) != 0) return 1;
        return 0;
    } catch (const std::exception &error) {
        std::cerr << error.what() << '\n';
        return 1;
    }
}
