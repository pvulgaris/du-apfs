// Probe Darwin's allocation metadata independently of the scanner.
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/attr.h>
#include <unistd.h>

#pragma pack(push, 4)
struct result {
    uint32_t length;
    attribute_set_t returned;
    uint64_t inode;
    int64_t allocated, data_length, data_allocated, rsrc_length, rsrc_allocated;
    int64_t private_size;
    uint64_t clone_id, flags;
    uint32_t clone_refs;
};
#pragma pack(pop)

int main(int argc, char **argv) {
    struct attrlist request = {0};
    request.bitmapcount = ATTR_BIT_MAP_COUNT;
    request.commonattr = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_FILEID;
    request.fileattr = ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH |
                       ATTR_FILE_DATAALLOCSIZE | ATTR_FILE_RSRCLENGTH |
                       ATTR_FILE_RSRCALLOCSIZE;
    request.forkattr = ATTR_CMNEXT_PRIVATESIZE | ATTR_CMNEXT_CLONEID |
                       ATTR_CMNEXT_EXT_FLAGS | ATTR_CMNEXT_CLONE_REFCNT;
    for (int i = 1; i < argc; i++) {
        struct result r = {0};
        if (getattrlist(argv[i], &request, &r, sizeof(r),
                        FSOPT_ATTR_CMN_EXTENDED | FSOPT_PACK_INVAL_ATTRS |
                            FSOPT_NOFOLLOW)) {
            perror(argv[i]);
            return 1;
        }
        printf("%s inode=%llu allocated=%lld data=%lld/%lld rsrc=%lld/%lld "
               "private=%lld clone=%llu refs=%u flags=0x%llx returned=%x/%x\n",
               argv[i], r.inode, r.allocated, r.data_length, r.data_allocated,
               r.rsrc_length, r.rsrc_allocated, r.private_size, r.clone_id,
               r.clone_refs, r.flags, r.returned.fileattr, r.returned.forkattr);
    }
}
