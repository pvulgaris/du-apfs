use crate::accounting::{Node, Tree};
use crate::args::Options;
use crate::native::{self, Metadata};
use crate::pipeline::{Job, JobPath, Mapped, map};
use crossbeam_channel::{Receiver, Sender, bounded, select};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

pub(crate) struct OperandFailure {
    pub index: usize,
    pub reason: String,
}

pub(crate) struct Scan {
    filesystems: HashMap<i32, (i32, i64, bool)>,
    pub domains: HashMap<String, i32>,
    pub roots: Vec<CString>,
    pub filesystem_operand: Option<usize>,
    pub filesystem_control: Option<libc::statfs>,
    pub operand_failures: Vec<OperandFailure>,
    pub tree: Tree,
    pub files: u64,
    pub errors: u64,
    pub error_details: Vec<Value>,
    pub error_counts: BTreeMap<String, u64>,
    pub unreadable: u64,
    pub unreadable_paths: Vec<String>,
    pub unknown: u64,
    pub unknown_paths: Vec<String>,
    pub skipped: Vec<String>,
    outstanding: usize,
    one_fs: bool,
    all: bool,
    max_depth: Option<usize>,
}

#[derive(Clone, Copy)]
struct Location {
    parent: usize,
    depth: usize,
    operand: usize,
    root_dev: i32,
    root: bool,
}

impl Scan {
    fn new(options: &Options, roots: Vec<CString>) -> Self {
        let max_depth = if options.summary {
            Some(0)
        } else {
            options.depth
        };
        Self {
            tree: Tree::new(max_depth, options.all, roots.len()),
            roots,
            filesystem_operand: None,
            filesystem_control: None,
            operand_failures: Vec::new(),
            filesystems: HashMap::new(),
            domains: HashMap::new(),
            files: 0,
            errors: 0,
            error_details: Vec::new(),
            error_counts: BTreeMap::new(),
            unreadable: 0,
            unreadable_paths: Vec::new(),
            unknown: 0,
            unknown_paths: Vec::new(),
            skipped: Vec::new(),
            outstanding: 0,
            one_fs: options.one_fs,
            all: options.all,
            max_depth,
        }
    }
    fn filesystem(&mut self, path: &CStr, meta: Metadata) -> std::io::Result<(i32, i64, bool)> {
        let dev = meta.dev;
        if let Some(f) = self.filesystems.get(&dev) {
            return Ok(*f);
        }
        let fs = native::filesystem_nofollow(path, meta.mode & libc::S_IFMT == libc::S_IFLNK)?;
        let apfs = unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) }.to_bytes() == b"apfs";
        let mut domain = 0;
        if apfs {
            let source = unsafe { CStr::from_ptr(fs.f_mntfromname.as_ptr()) }.to_string_lossy();
            let device = source.rsplit('@').next().unwrap();
            let digits = device
                .strip_prefix("/dev/disk")
                .ok_or_else(|| std::io::Error::other("unknown APFS device"))?;
            let count = digits.bytes().take_while(|b| b.is_ascii_digit()).count();
            if count == 0 || (count < digits.len() && digits.as_bytes()[count] != b's') {
                return Err(std::io::Error::other("unknown APFS device"));
            }
            let key = &device[.."/dev/disk".len() + count];
            let next = self.domains.len() as i32 + 1;
            domain = *self.domains.entry(key.into()).or_insert(next);
            if self
                .filesystems
                .values()
                .any(|(d, block, _)| *d == domain && *block != fs.f_bsize as i64)
            {
                return Err(std::io::Error::other("inconsistent container block sizes"));
            }
        }
        let f = (domain, fs.f_bsize as i64, apfs);
        self.filesystems.insert(dev, f);
        Ok(f)
    }
    fn issue(&mut self, path: &CStr, reason: String) {
        self.errors += 1;
        *self.error_counts.entry(reason.clone()).or_default() += 1;
        if self.error_details.len() < 100 {
            self.error_details
                .push(json!({"path":path.to_string_lossy(),"reason":reason}));
        }
    }
    fn operand_issue(&mut self, index: usize, reason: String) {
        let path = self.roots[index].clone();
        self.issue(&path, reason.clone());
        self.operand_failures.push(OperandFailure { index, reason });
    }
    fn note_unknown(&mut self, path: impl FnOnce() -> String) {
        self.unknown += 1;
        if self.unknown_paths.len() < 100 {
            self.unknown_paths.push(path());
        }
    }
    fn audit(&mut self, path: &CStr) {
        match native::unknown_xattrs(path) {
            Ok(true) => self.note_unknown(|| path.to_string_lossy().into_owned()),
            Ok(false) => (),
            Err(e) => self.issue(path, e.to_string()),
        }
    }

    fn consume(&mut self, result: Mapped) {
        self.outstanding -= 1;
        match result.mapping.attributes {
            Ok(true) => {
                self.note_unknown(|| result.job.path.materialize().to_string_lossy().into_owned())
            }
            Ok(false) => (),
            Err(error) => self.issue(&result.job.path.materialize(), error.to_string()),
        }
        let (ranges, extra) = match result.mapping.extents {
            Ok(mapped) => mapped,
            Err(error) => {
                self.issue(&result.job.path.materialize(), error.to_string());
                self.tree.nodes[result.job.owner].unmeasured += result.job.meta.allocated;
                return;
            }
        };
        if extra > 0 {
            self.issue(
                &result.job.path.materialize(),
                "allocated bytes outside mapped forks".into(),
            );
            self.tree.nodes[result.job.owner].unmeasured += extra;
        }
        for extent in ranges.iter() {
            self.tree.add(
                result.job.owner,
                result.job.domain,
                extent,
                result.job.block,
            );
        }
    }

    fn add_node(&mut self, path: CString, location: Location, directory: bool) -> usize {
        self.tree.push(Node {
            path,
            parent: Some(location.parent),
            operand: location.operand,
            directory,
            directory_read_error: false,
            root: location.root,
            physical: 0,
            allocated: 0,
            unmeasured: 0,
        })
    }

    fn file(
        &mut self,
        path: JobPath,
        meta: Metadata,
        location: Location,
        jobs: &Sender<Job>,
        results: &Receiver<Mapped>,
    ) {
        if self.one_fs && meta.dev != location.root_dev {
            self.skipped
                .push(path.materialize().to_string_lossy().into_owned());
            return;
        }
        let filesystem = match self.filesystems.get(&meta.dev) {
            Some(f) => Ok(*f),
            None => self.filesystem(&path.materialize(), meta),
        };
        let (domain, block, apfs) = match filesystem {
            Ok(f) => f,
            Err(error) => {
                self.issue(&path.materialize(), error.to_string());
                return;
            }
        };
        if !apfs {
            self.skipped
                .push(path.materialize().to_string_lossy().into_owned());
            return;
        }
        self.files += 1;
        let owner = if location.root
            || (self.all && self.max_depth.is_none_or(|max| location.depth <= max))
        {
            self.add_node(path.materialize(), location, false)
        } else {
            location.parent
        };
        self.tree.nodes[owner].allocated += meta.allocated;
        if meta.regular() && (meta.allocated > 0 || meta.extended & native::NO_XATTRS == 0) {
            let job = Job {
                path,
                meta,
                block,
                domain,
                owner,
            };
            loop {
                select! {
                    send(jobs,job)->r => {r.expect("mapping workers alive");self.outstanding+=1;break;},
                    recv(results)->r => self.consume(r.expect("mapping results available")),
                }
            }
        } else {
            self.tree.nodes[owner].unmeasured += meta.allocated;
            if meta.extended & native::NO_XATTRS == 0 {
                self.audit(&path.materialize());
            }
        }
    }
}

pub(crate) fn run(options: &Options) -> std::io::Result<Scan> {
    let mut path_errors = vec![None; options.paths.len()];
    let roots: Vec<_> = options
        .paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let absolute = match std::path::absolute(path) {
                Ok(path) => path,
                Err(error) => {
                    path_errors[index] = Some(error.to_string());
                    path.clone()
                }
            };
            CString::new(absolute.as_os_str().as_bytes()).map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains a NUL byte")
            })
        })
        .collect::<std::io::Result<_>>()?;
    let mut scan = Scan::new(options, roots);
    let mut root_symlinks = vec![false; scan.roots.len()];
    let mut valid = Vec::new();
    for operand in 0..scan.roots.len() {
        if let Some(reason) = path_errors[operand].take() {
            scan.operand_issue(operand, reason);
            continue;
        }
        let path = scan.roots[operand].clone();
        let meta = match native::stat(&path) {
            Ok(meta) => meta,
            Err(error) => {
                scan.operand_issue(operand, error.to_string());
                continue;
            }
        };
        match scan.filesystem(&path, meta) {
            Ok((_, _, true)) => (),
            Ok(_) => {
                scan.operand_issue(operand, "not on APFS".into());
                continue;
            }
            Err(error) => {
                scan.operand_issue(operand, error.to_string());
                continue;
            }
        }
        root_symlinks[operand] = meta.mode & libc::S_IFMT == libc::S_IFLNK;
        valid.push((
            path,
            Location {
                parent: 0,
                depth: 0,
                operand,
                root_dev: meta.dev,
                root: true,
            },
        ));
    }
    let mut stack: Vec<_> = valid.into_iter().rev().collect();
    let (jobs_tx, jobs_rx) = bounded::<Job>(2048);
    let (results_tx, results_rx) = bounded::<Mapped>(2048);
    std::thread::scope(|scope| {
        for _ in 0..options.workers {
            let jobs = jobs_rx.clone();
            let results = results_tx.clone();
            scope.spawn(move || {
                let mut path_buffer = Vec::with_capacity(1024);
                for job in jobs {
                    if results.send(map(job, &mut path_buffer)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(jobs_rx);
        drop(results_tx);
        let mut reader = native::DirectoryReader::new();
        while let Some((path, location)) = stack.pop() {
            while let Ok(result) = results_rx.try_recv() {
                scan.consume(result);
            }
            if scan.roots[location.operand].to_bytes() == b"/"
                && path.to_bytes() == b"/System/Volumes/Data"
            {
                scan.skipped.push(path.to_string_lossy().into_owned());
                continue;
            }
            let meta = match native::stat(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    scan.issue(&path, error.to_string());
                    continue;
                }
            };
            if scan.one_fs && meta.dev != location.root_dev {
                scan.skipped.push(path.to_string_lossy().into_owned());
                continue;
            }
            match scan.filesystem(&path, meta) {
                Ok((_, _, true)) => (),
                Ok(_) => {
                    scan.skipped.push(path.to_string_lossy().into_owned());
                    continue;
                }
                Err(error) => {
                    scan.issue(&path, error.to_string());
                    continue;
                }
            }
            if !meta.directory() {
                scan.file(JobPath::Full(path), meta, location, &jobs_tx, &results_rx);
                continue;
            }
            let owner = if location.root || scan.max_depth.is_none_or(|max| location.depth <= max) {
                scan.add_node(path.clone(), location, true)
            } else {
                location.parent
            };
            scan.tree.nodes[owner].allocated += meta.allocated;
            scan.tree.nodes[owner].unmeasured += meta.allocated;
            scan.audit(&path);
            let path = Arc::new(path);
            let child = Location {
                parent: owner,
                depth: location.depth + 1,
                root: false,
                ..location
            };
            if let Err(error) = reader.read(&path, |name, meta| {
                if let Some(meta) = meta {
                    // Parent paths are shared by queued children; workers join
                    // them in reusable buffers instead of retaining full paths.
                    let file = JobPath::Child {
                        parent: path.clone(),
                        name: CString::new(name).unwrap(),
                    };
                    scan.file(file, meta, child, &jobs_tx, &results_rx);
                } else {
                    stack.push((native::join(&path, name), child));
                }
            }) {
                scan.issue(&path, error.to_string());
                scan.unreadable += 1;
                // A hidden directory contributes to its retained ancestor, but
                // a failure to list it must not suppress that ancestor's row.
                if scan.tree.nodes[owner].path.as_c_str() == path.as_c_str() {
                    scan.tree.nodes[owner].directory_read_error = true;
                }
                if scan.unreadable_paths.len() < 100 {
                    scan.unreadable_paths
                        .push(path.to_string_lossy().into_owned());
                }
            }
        }
        drop(jobs_tx);
        while scan.outstanding > 0 {
            scan.consume(
                results_rx
                    .recv()
                    .expect("all submitted files receive a result"),
            );
        }
    });
    scan.tree.finish();
    scan.filesystem_operand = scan
        .tree
        .nodes
        .iter()
        .find(|node| node.root)
        .map(|node| node.operand);
    if let Some(operand) = scan.filesystem_operand {
        let path = scan.roots[operand].clone();
        match native::filesystem_nofollow(&path, root_symlinks[operand]) {
            Ok(fs) => scan.filesystem_control = Some(fs),
            Err(error) => scan.issue(&path, format!("filesystem control: {error}")),
        }
    }
    Ok(scan)
}
