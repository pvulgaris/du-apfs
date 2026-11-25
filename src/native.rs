use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// EF_NO_XATTRS from sys/attr.h; the libc crate does not export the EF_ flags.
pub const NO_XATTRS: u64 = 2;
// ATTR_CMN_ERROR from sys/attr.h; the libc crate does not export it.
const ATTR_CMN_ERROR: u32 = 0x20000000;
// The record parser in DirectoryReader::read reads these at fixed offsets,
// so the set and its order are part of that layout.
// OBJTYPE keeps XNU on native bulk enumeration. Its per-entry fallback can
// report zero allocation for externally stored compressed attributes.
const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_ERROR
    | libc::ATTR_CMN_FILEID
    | libc::ATTR_CMN_FLAGS
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_CHGTIME
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_NAME;
const FILE: u32 =
    libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;

#[derive(Clone, Copy, Default, Debug)]
pub struct Metadata {
    pub dev: i32,
    pub inode: u64,
    pub size: i64,
    pub allocated: i64,
    pub mode: u16,
    pub flags: u32,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
    pub extended: u64,
}

impl Metadata {
    pub fn regular(self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }
    pub fn directory(self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }
    pub fn same(self, b: Self) -> bool {
        self.dev == b.dev
            && self.inode == b.inode
            && self.size == b.size
            && self.allocated == b.allocated
            && self.mtime == b.mtime
            && self.ctime == b.ctime
    }
    fn from_stat(s: libc::stat) -> Self {
        Self {
            dev: s.st_dev,
            inode: s.st_ino,
            size: s.st_size,
            allocated: s.st_blocks * 512,
            mode: s.st_mode,
            flags: s.st_flags,
            mtime: (s.st_mtime, s.st_mtime_nsec),
            ctime: (s.st_ctime, s.st_ctime_nsec),
            extended: 0,
        }
    }
}

pub fn stat(path: &CStr) -> io::Result<Metadata> {
    let mut s = std::mem::MaybeUninit::uninit();
    if unsafe { libc::lstat(path.as_ptr(), s.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Metadata::from_stat(unsafe { s.assume_init() }))
}

fn fstat(fd: &OwnedFd) -> io::Result<Metadata> {
    let mut s = std::mem::MaybeUninit::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), s.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Metadata::from_stat(unsafe { s.assume_init() }))
}

fn open(path: &CStr, directory: bool) -> io::Result<OwnedFd> {
    // A regular file may become a FIFO between enumeration and open. Do not
    // block waiting for a writer before the descriptor's identity is checked.
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

pub fn join(parent: &CStr, name: &[u8]) -> CString {
    let mut b = Vec::with_capacity(parent.to_bytes().len() + name.len() + 2);
    b.extend_from_slice(parent.to_bytes());
    if b.last() != Some(&b'/') {
        b.push(b'/');
    }
    b.extend_from_slice(name);
    CString::new(b).expect("directory names contain no NUL")
}

/// A fixed-size C string field such as statfs's names, bounded by the array.
pub fn cchars(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect()
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s)
}

pub struct DirectoryReader {
    buffer: Vec<u8>,
}

impl DirectoryReader {
    pub fn new() -> Self {
        Self {
            buffer: vec![0; 64 * 1024],
        }
    }
    pub fn read(
        &mut self,
        path: &CStr,
        mut visit: impl FnMut(&[u8], Option<Metadata>),
    ) -> io::Result<()> {
        let fd = open(path, true)?;
        let mut attrs = libc::attrlist {
            bitmapcount: 5,
            reserved: 0,
            commonattr: COMMON,
            volattr: 0,
            dirattr: 0,
            fileattr: FILE,
            forkattr: libc::ATTR_CMNEXT_EXT_FLAGS,
        };
        loop {
            let n = unsafe {
                libc::getattrlistbulk(
                    fd.as_raw_fd(),
                    (&mut attrs as *mut libc::attrlist).cast(),
                    self.buffer.as_mut_ptr().cast(),
                    self.buffer.len(),
                    (libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_ATTR_CMN_EXTENDED) as u64,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                if e.raw_os_error() == Some(libc::ENOTSUP) {
                    use std::os::unix::ffi::OsStrExt;
                    let p = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
                    for entry in std::fs::read_dir(p)? {
                        visit(entry?.file_name().as_bytes(), None);
                    }
                    return Ok(());
                }
                return Err(e);
            }
            if n == 0 {
                return Ok(());
            }
            let mut offset = 0;
            for _ in 0..n {
                if offset + 4 > self.buffer.len() {
                    return Err(invalid("truncated attribute record"));
                }
                let len = u32_at(&self.buffer, offset) as usize;
                if len < 36 || len > self.buffer.len() - offset {
                    return Err(invalid("invalid attribute record"));
                }
                let b = &self.buffer[offset..offset + len];
                offset += len;
                let name_offset = 28i64 + u32_at(b, 28) as i32 as i64;
                let name_len = u32_at(b, 32) as usize;
                if name_len == 0 || name_offset < 36 || name_offset as usize + name_len > b.len() {
                    return Err(invalid("invalid attribute name"));
                }
                let at = name_offset as usize;
                if b[at + name_len - 1] != 0 {
                    return Err(invalid("unterminated attribute name"));
                }
                let name = &b[at..at + name_len - 1];
                let m = if b.len() >= 120
                    && u32_at(b, 24) == 0
                    && u32_at(b, 4) & COMMON == COMMON
                    && u32_at(b, 40) == 1
                    && u32_at(b, 16) & FILE == FILE
                {
                    Some(Metadata {
                        dev: u32_at(b, 36) as i32,
                        mtime: (u64_at(b, 44) as i64, u64_at(b, 52) as i64),
                        ctime: (u64_at(b, 60) as i64, u64_at(b, 68) as i64),
                        mode: u32_at(b, 76) as u16 | libc::S_IFREG,
                        flags: u32_at(b, 80),
                        inode: u64_at(b, 84),
                        allocated: u64_at(b, 96) as i64,
                        size: u64_at(b, 104) as i64,
                        extended: if u32_at(b, 20) & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
                            u64_at(b, 112)
                        } else {
                            0
                        },
                    })
                } else {
                    None
                };
                visit(name, m);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub start: i64,
    pub end: i64,
}

#[derive(Default)]
pub struct Ranges {
    first: Option<Extent>,
    rest: Vec<Extent>,
}
impl Ranges {
    fn add(&mut self, e: Extent) {
        if self.first.is_none() {
            self.first = Some(e)
        } else {
            self.rest.push(e)
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = Extent> + '_ {
        self.first.iter().chain(self.rest.iter()).copied()
    }
}

fn map_fork(
    fd: &OwnedFd,
    size: i64,
    budget: i64,
    block: i64,
    ranges: &mut Ranges,
) -> io::Result<i64> {
    let mut offset = 0;
    let mut mapped = 0;
    while offset < size || mapped < budget {
        let mut request = [0u8; 20];
        // Include preallocation beyond logical EOF, including fragmented tails.
        let requested = (size - offset).max(budget - mapped).max(block);
        request[4..12].copy_from_slice(&requested.to_le_bytes());
        request[12..20].copy_from_slice(&offset.to_le_bytes());
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_LOG2PHYS_EXT, request.as_mut_ptr()) } < 0 {
            let error = io::Error::last_os_error();
            if offset >= size && error.raw_os_error() == Some(libc::ERANGE) {
                break;
            }
            return Err(error);
        }
        let length = u64_at(&request, 4) as i64;
        let physical = u64_at(&request, 12) as i64;
        if length <= 0 || length > requested {
            return Err(invalid("invalid extent returned by macOS"));
        }
        if physical >= 0 {
            let start = physical / block * block;
            let end = (physical + length + block - 1) / block * block;
            ranges.add(Extent { start, end });
            mapped += end - start;
        } else if offset >= size {
            break;
        }
        offset += length;
    }
    Ok(mapped)
}

pub struct FileMapping {
    pub extents: io::Result<(Ranges, i64)>,
    pub attributes: io::Result<bool>,
}

pub fn map_file(path: &CStr, expected: Metadata, block: i64) -> FileMapping {
    let fd = match open(path, false) {
        Ok(fd) => fd,
        Err(e) => {
            return FileMapping {
                extents: Err(e),
                attributes: Ok(false),
            };
        }
    };
    let extents = map_open_file(&fd, path, expected, block);
    // Reuse the descriptor for attribute names. This avoids a second pathname
    // lookup and audits the same inode even if its name changes during mapping.
    let attributes = if expected.extended & NO_XATTRS != 0 {
        Ok(false)
    } else {
        list_unknown_xattrs(Some(&fd), path)
    };
    FileMapping {
        extents,
        attributes,
    }
}

fn map_open_file(
    fd: &OwnedFd,
    path: &CStr,
    expected: Metadata,
    block: i64,
) -> io::Result<(Ranges, i64)> {
    let current = fstat(fd)?;
    if !current.same(expected) {
        return Err(invalid("file changed during scan"));
    }
    let mut ranges = Ranges::default();
    let result = map_fork(fd, current.size, current.allocated, block, &mut ranges);
    let compressed = result.as_ref().err().and_then(|e| e.raw_os_error()) == Some(libc::ENOTSUP)
        && current.flags & libc::UF_COMPRESSED != 0;
    if compressed {
        // Discard any partial data-fork result before falling back to the
        // resource fork. Failed mapping must not contribute unverified ranges.
        ranges = Ranges::default();
    }
    let mut mapped = if compressed { 0 } else { result? };
    if compressed || mapped < current.allocated {
        let name = join(path, b"..namedfork/rsrc");
        let rsrc = match open(&name, false) {
            Ok(fd) => Some(fd),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
            Err(e) => return Err(e),
        };
        if let Some(rsrc) = rsrc {
            let meta = fstat(&rsrc)?;
            if meta.dev != current.dev
                || meta.inode != current.inode
                || meta.mtime != current.mtime
                || meta.ctime != current.ctime
            {
                return Err(invalid("file changed during scan"));
            }
            mapped += map_fork(&rsrc, meta.size, meta.allocated, block, &mut ranges)?;
        }
    }
    if !fstat(fd)?.same(current) {
        return Err(invalid("file changed during scan"));
    }
    if mapped > current.allocated {
        return Err(invalid("extent map exceeds allocated blocks"));
    }
    Ok((ranges, current.allocated - mapped))
}

pub fn unknown_xattrs(path: &CStr) -> io::Result<bool> {
    list_unknown_xattrs(None, path)
}

fn list_unknown_xattrs(fd: Option<&OwnedFd>, path: &CStr) -> io::Result<bool> {
    let mut local = [0u8; 1024];
    let mut extra = Vec::new();
    loop {
        let b = if extra.is_empty() {
            &mut local[..]
        } else {
            &mut extra[..]
        };
        let n = unsafe {
            match fd {
                Some(fd) => libc::flistxattr(
                    fd.as_raw_fd(),
                    b.as_mut_ptr().cast(),
                    b.len(),
                    libc::XATTR_SHOWCOMPRESSION,
                ),
                None => libc::listxattr(
                    path.as_ptr(),
                    b.as_mut_ptr().cast(),
                    b.len(),
                    libc::XATTR_SHOWCOMPRESSION | libc::XATTR_NOFOLLOW,
                ),
            }
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            if e.raw_os_error() == Some(libc::ERANGE) && b.len() < 1 << 20 {
                extra = vec![0; b.len() * 2];
                continue;
            }
            return Err(e);
        }
        for name in b[..n as usize]
            .split(|&c| c == 0)
            .filter(|name| !name.is_empty())
        {
            if name != b"com.apple.ResourceFork" {
                return Ok(true);
            }
        }
        return Ok(false);
    }
}

pub fn filesystem_nofollow(path: &CStr, symlink: bool) -> io::Result<libc::statfs> {
    if !symlink {
        return filesystem(path);
    }
    // statfs follows the final symlink. Its parent is on the filesystem
    // containing the link, including when the link is dangling.
    let bytes = path.to_bytes();
    let parent = match bytes.iter().rposition(|&byte| byte == b'/') {
        Some(0) => c"/".to_owned(),
        Some(index) => CString::new(&bytes[..index]).unwrap(),
        None => c".".to_owned(),
    };
    filesystem(&parent)
}

pub fn filesystem(path: &CStr) -> io::Result<libc::statfs> {
    let mut fs = std::mem::MaybeUninit::uninit();
    if unsafe { libc::statfs(path.as_ptr(), fs.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs.assume_init() })
}
