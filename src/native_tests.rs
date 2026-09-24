use crate::{Bitmap, native, run};
use std::ffi::{CStr, CString};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(crate) struct Fixture(pub PathBuf);

impl Fixture {
    pub(crate) fn new() -> Self {
        let template = std::env::temp_dir().join("du-apfs-rust-test.XXXXXX");
        let mut name = CString::new(template.as_os_str().as_bytes())
            .unwrap()
            .into_bytes_with_nul();
        let created = unsafe { libc::mkdtemp(name.as_mut_ptr().cast()) };
        assert!(!created.is_null(), "{}", std::io::Error::last_os_error());
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(
            unsafe { CStr::from_ptr(created) }.to_bytes(),
        ));
        let fixture = Self(path);
        let fs = native::filesystem(&fixture.path("")).unwrap();
        assert_eq!(
            unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) }.to_bytes(),
            b"apfs"
        );
        assert_eq!(
            fs.f_bsize, 4096,
            "fixtures require 4096-byte allocation blocks"
        );
        fixture
    }

    fn path(&self, name: &str) -> CString {
        CString::new(self.0.join(name).as_os_str().as_bytes()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct DenyAccess(PathBuf, fs::Permissions);

impl DenyAccess {
    fn new(path: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let guard = Self(path.to_owned(), fs::metadata(path).unwrap().permissions());
        fs::set_permissions(path, fs::Permissions::from_mode(0)).unwrap();
        guard
    }
}

impl Drop for DenyAccess {
    fn drop(&mut self) {
        // Restore access even on assertion failure so fixture cleanup succeeds.
        let _ = fs::set_permissions(&self.0, self.1.clone());
    }
}

fn path(name: &CStr) -> &Path {
    Path::new(std::ffi::OsStr::from_bytes(name.to_bytes()))
}

fn set_xattr(file: &CStr, name: &CStr, value: &[u8]) {
    assert_eq!(
        unsafe {
            libc::setxattr(
                file.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
}

// A provenance-tagged test runner can add this xattr to every fixture it
// creates. Check the exact ambient names rather than changing security metadata.
fn ambient_provenance(file: &CStr) -> bool {
    let mut names = [0u8; 1024];
    let n = unsafe { libc::listxattr(file.as_ptr(), names.as_mut_ptr().cast(), names.len(), 0x21) };
    assert!(n >= 0, "{}", std::io::Error::last_os_error());
    let mut provenance = false;
    for name in names[..n as usize]
        .split(|&byte| byte == 0)
        .filter(|name| !name.is_empty())
    {
        match name {
            b"com.apple.provenance" => provenance = true,
            b"com.apple.ResourceFork" => {}
            _ => panic!("unexpected fixture xattr: {:?}", name),
        }
    }
    provenance
}

#[test]
fn ambient_provenance_check_rejects_other_attributes() {
    let fixture = Fixture::new();
    let file = fixture.path("other-attribute");
    fs::write(path(&file), []).unwrap();
    set_xattr(&file, c"user.du_apfs_test", b"unexpected");
    assert!(std::panic::catch_unwind(|| ambient_provenance(&file)).is_err());
}

fn mapping(file: &CStr) -> (native::Ranges, i64, bool) {
    let meta = native::stat(file).unwrap();
    let result = native::map_file(file, meta, 4096);
    let (ranges, missing) = result.extents.unwrap();
    (ranges, missing, result.attributes.unwrap())
}

fn union(ranges: &native::Ranges) -> i64 {
    let mut bitmap = Bitmap::default();
    for range in ranges.iter() {
        bitmap.add(1, range, 4096);
    }
    bitmap.physical
}

#[test]
fn short_data_and_resource_tails_are_counted_once() {
    let fixture = Fixture::new();
    for data in [0usize, 1, 4095, 4096, 4097] {
        for resource in [4096usize, 4097, 65536, 65537] {
            let file = fixture.path(&format!("forks-{data}-{resource}"));
            fs::write(path(&file), vec![0; data]).unwrap();
            fs::write(
                path(&native::join(&file, b"..namedfork/rsrc")),
                vec![0; resource],
            )
            .unwrap();
            let (ranges, missing, unknown) = mapping(&file);
            assert_eq!(missing, 0);
            assert_eq!(unknown, ambient_provenance(&file));
            assert_eq!(
                union(&ranges),
                ((data.div_ceil(4096) + resource.div_ceil(4096)) * 4096) as i64
            );
        }
    }
}

#[test]
fn partial_clones_and_hardlinks_share_only_their_actual_blocks() {
    let fixture = Fixture::new();
    let original = fixture.path("original");
    let clone = fixture.path("clone");
    fs::write(path(&original), vec![b'a'; 1 << 20]).unwrap();
    assert_eq!(
        unsafe { libc::clonefile(original.as_ptr(), clone.as_ptr(), 0) },
        0
    );
    let before = run(fixture.path(""), 3).unwrap();
    assert_eq!(before.tree.nodes[0].physical, 1 << 20);
    assert_eq!(before.errors, 0);
    let original_ranges = mapping(&original).0;
    let mut f = OpenOptions::new().write(true).open(path(&clone)).unwrap();
    // Use aligned 64 KiB writes: a buffered 4 KiB write can allocate a whole
    // 16 KiB VM page on Apple Silicon despite APFS's 4 KiB allocation blocks.
    f.seek(SeekFrom::Start(8 * 65536)).unwrap();
    f.write_all(&[b'b'; 65536]).unwrap();
    f.sync_all().unwrap();
    fs::hard_link(path(&original), fixture.0.join("hardlink")).unwrap();
    let after = run(fixture.path(""), 3).unwrap();
    assert_eq!(after.tree.nodes[0].physical, (1 << 20) + 65536);
    assert_eq!(after.errors, 0);
    assert_eq!(after.tree.nodes[0].unmeasured, 0);
    let ambient_count = [
        &fixture.path(""),
        &original,
        &clone,
        &fixture.path("hardlink"),
    ]
    .into_iter()
    .filter(|file| ambient_provenance(file))
    .count() as u64;
    assert_eq!(after.unknown, ambient_count);
    assert_eq!(union(&original_ranges), 1 << 20);
    assert_eq!(fs::read(path(&original)).unwrap()[8 * 65536], b'a');
}

#[test]
fn sparse_holes_and_preallocated_tails_preserve_allocation() {
    let fixture = Fixture::new();
    let sparse = fixture.path("sparse");
    let mut f = fs::File::create(path(&sparse)).unwrap();
    f.write_all(&[1; 65536]).unwrap();
    f.seek(SeekFrom::Start((1 << 30) - 65536)).unwrap();
    f.write_all(&[2; 65536]).unwrap();
    f.sync_all().unwrap();
    let (ranges, missing, unknown) = mapping(&sparse);
    assert_eq!(union(&ranges), 2 * 65536);
    assert_eq!(missing, 0);
    assert_eq!(unknown, ambient_provenance(&sparse));

    let preallocated = fixture.path("preallocated");
    let mut f = fs::File::create(path(&preallocated)).unwrap();
    let mut request = libc::fstore_t {
        fst_flags: libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: 1 << 20,
        fst_bytesalloc: 0,
    };
    assert_eq!(
        unsafe { libc::fcntl(f.as_raw_fd(), libc::F_PREALLOCATE, &mut request) },
        0
    );
    f.write_all(&[3; 4096]).unwrap();
    f.sync_all().unwrap();
    let meta = native::stat(&preallocated).unwrap();
    assert!(
        meta.allocated > meta.size,
        "fixture must retain preallocation beyond EOF"
    );
    let (ranges, missing, unknown) = mapping(&preallocated);
    assert_eq!(union(&ranges), meta.allocated);
    assert_eq!(missing, 0);
    assert_eq!(unknown, ambient_provenance(&preallocated));
}

#[test]
fn external_attributes_cannot_claim_complete_coverage() {
    let fixture = Fixture::new();
    for data in [0, 4096] {
        let file = fixture.path(&format!("attribute-{data}"));
        fs::write(path(&file), vec![0; data]).unwrap();
        set_xattr(&file, c"user.du_apfs_test", &[7; 65536]);
        let scan = run(file, 3).unwrap();
        assert_eq!(scan.unknown, 1);
        assert_eq!(scan.tree.nodes[0].physical, data as i64);
    }
}

// Use Apple's existing zlib only for a valid native compression fixture. The
// scanner itself neither reads file contents nor depends on a decompressor.
#[link(name = "z")]
unsafe extern "C" {
    fn compressBound(source_len: libc::c_ulong) -> libc::c_ulong;
    fn compress2(
        dest: *mut u8,
        dest_len: *mut libc::c_ulong,
        source: *const u8,
        source_len: libc::c_ulong,
        level: libc::c_int,
    ) -> libc::c_int;
}

#[test]
fn compressed_attribute_stream_is_reported_as_unmapped() {
    let fixture = Fixture::new();
    let file = fixture.path("compressed-attribute");
    let mut seed = 31u64;
    let data: Vec<u8> = (0..3000)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as u8
        })
        .collect::<Vec<_>>()
        .repeat(8);
    let mut compressed_len = unsafe { compressBound(data.len() as _) };
    let mut compressed = vec![0; compressed_len as usize];
    assert_eq!(
        unsafe {
            compress2(
                compressed.as_mut_ptr(),
                &mut compressed_len,
                data.as_ptr(),
                data.len() as _,
                6,
            )
        },
        0
    );
    compressed.truncate(compressed_len as usize);
    let mut attribute = Vec::new();
    attribute.extend_from_slice(&0x636d7066u32.to_le_bytes());
    attribute.extend_from_slice(&3u32.to_le_bytes());
    attribute.extend_from_slice(&(data.len() as u64).to_le_bytes());
    attribute.extend_from_slice(&compressed);
    fs::write(path(&file), []).unwrap();
    set_xattr(&file, c"com.apple.decmpfs", &attribute);
    assert_eq!(
        unsafe { libc::chflags(file.as_ptr(), libc::UF_COMPRESSED) },
        0
    );
    assert_eq!(
        fs::read(path(&file)).unwrap(),
        data,
        "macOS must successfully decompress the fixture"
    );
    let meta = native::stat(&file).unwrap();
    assert!(meta.allocated > 0);
    for root in [file, fixture.path("")] {
        let directory_root = root == fixture.path("");
        let expected_unknown = 1 + if directory_root {
            u64::from(ambient_provenance(&root))
        } else {
            0
        };
        let scan = run(root, 3).unwrap();
        assert_eq!(scan.tree.nodes[0].physical, 0);
        assert_eq!(scan.tree.nodes[0].unmeasured, meta.allocated);
        assert_eq!(scan.unknown, expected_unknown);
        assert_eq!(scan.errors, 1);
    }
}

#[test]
fn replacing_an_enumerated_file_with_a_fifo_does_not_block() {
    let fixture = Fixture::new();
    let file = fixture.path("replaced");
    fs::write(path(&file), [1; 4096]).unwrap();
    let meta = native::stat(&file).unwrap();
    fs::remove_file(path(&file)).unwrap();
    assert_eq!(unsafe { libc::mkfifo(file.as_ptr(), 0o600) }, 0);
    let (sender, receiver) = std::sync::mpsc::channel();
    let owned = file.clone();
    let worker = std::thread::spawn(move || {
        sender
            .send(native::map_file(&owned, meta, 4096).extents.map(|_| ()))
            .unwrap();
    });
    let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
    if result.is_err() {
        // Release a regressed blocking reader before failing the test.
        let fd = unsafe { libc::open(file.as_ptr(), libc::O_WRONLY | libc::O_NONBLOCK) };
        if fd >= 0 {
            unsafe {
                libc::close(fd);
            }
        }
    }
    worker.join().unwrap();
    let error = result
        .expect("mapper blocked on replacement FIFO")
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn directory_and_operand_totals_preserve_partial_clone_sharing() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("left")).unwrap();
    fs::create_dir_all(fixture.0.join("right/deep")).unwrap();
    let original = fixture.path("left/original");
    let clone = fixture.path("right/deep/clone");
    fs::write(path(&original), vec![b'a'; 1 << 20]).unwrap();
    assert_eq!(
        unsafe { libc::clonefile(original.as_ptr(), clone.as_ptr(), 0) },
        0
    );
    let mut writer = OpenOptions::new().write(true).open(path(&clone)).unwrap();
    writer.seek(SeekFrom::Start(65536)).unwrap();
    writer.write_all(&[b'b'; 65536]).unwrap();
    writer.sync_all().unwrap();

    for (flags, expected_paths) in [
        (vec!["-s"], vec![""]),
        (vec!["-d0"], vec![""]),
        (vec!["-d1"], vec!["", "left", "right"]),
        (vec![], vec!["", "left", "right", "right/deep"]),
        (
            vec!["-a"],
            vec![
                "",
                "left",
                "left/original",
                "right",
                "right/deep",
                "right/deep/clone",
            ],
        ),
        (vec!["-a", "-d0"], vec![""]),
        (vec!["-a", "-d1"], vec!["", "left", "right"]),
    ] {
        let mut argv: Vec<std::ffi::OsString> = flags.into_iter().map(Into::into).collect();
        argv.push(fixture.0.as_os_str().to_owned());
        let options = crate::args::parse(argv).unwrap();
        let scan = crate::scan::run(&options).unwrap();
        assert_eq!(scan.tree.nodes[0].physical, (1 << 20) + 65536);
        assert_eq!(scan.errors, 0);
        assert_eq!(
            scan.tree.domain_bytes.values().sum::<i64>(),
            (1 << 20) + 65536
        );
        for node in scan.tree.nodes.iter().skip(1) {
            assert_eq!(
                node.physical,
                if node.root {
                    (1 << 20) + 65536
                } else {
                    1 << 20
                }
            );
        }
        if options.summary || options.depth == Some(0) {
            assert_eq!(
                scan.tree.nodes.len(),
                2,
                "summary retains no descendant paths"
            );
        }
        let mut options = options;
        options.json = true;
        let mut output = Vec::new();
        crate::output::write(&scan, &options, 0.0, &mut output).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let mut actual_paths: Vec<_> = report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                Path::new(entry["path"].as_str().unwrap())
                    .strip_prefix(&fixture.0)
                    .unwrap()
                    .to_str()
                    .unwrap()
            })
            .collect();
        actual_paths.sort_unstable();
        assert_eq!(actual_paths, expected_paths);
    }
    // Every operand gets its own complete union. The combined total counts
    // shared blocks once, even for repeated and overlapping operands.
    for flags in [vec!["-s"], vec!["-d1"], vec!["-a"]] {
        let mut argv: Vec<std::ffi::OsString> = flags.into_iter().map(Into::into).collect();
        argv.extend([
            fixture.0.join("left").into_os_string(),
            fixture.0.join("right").into_os_string(),
            fixture.0.join("left/original").into_os_string(),
            fixture.0.join("left").into_os_string(),
        ]);
        let scan = crate::scan::run(&crate::args::parse(argv).unwrap()).unwrap();
        assert_eq!(scan.tree.nodes[0].physical, (1 << 20) + 65536);
        let roots: Vec<_> = scan.tree.nodes.iter().filter(|n| n.root).collect();
        assert_eq!(roots.len(), 4);
        assert!(roots.iter().all(|n| n.physical == 1 << 20));
        assert_eq!(
            scan.tree.domain_bytes.values().sum::<i64>(),
            (1 << 20) + 65536
        );
    }
}

#[test]
fn symlink_operands_use_the_links_filesystem_without_following_targets() {
    let fixture = Fixture::new();
    let regular = fixture.path("regular");
    fs::write(path(&regular), [7; 4096]).unwrap();
    let dangling = fixture.path("dangling");
    let to_dev = fixture.path("to-dev");
    std::os::unix::fs::symlink("missing-target", path(&dangling)).unwrap();
    std::os::unix::fs::symlink("/dev", path(&to_dev)).unwrap();
    let expected_fs = native::filesystem(&fixture.path("")).unwrap();
    let expected_source = unsafe { CStr::from_ptr(expected_fs.f_mntfromname.as_ptr()) }
        .to_string_lossy()
        .into_owned();

    for link in [&dangling, &to_dev] {
        // Standalone links must find their containing filesystem without
        // relying on a regular operand to populate the filesystem cache.
        for (operands, physical, link_entry) in [
            (vec![link], 0, 0),
            (vec![link, &regular], 4096, 0),
            (vec![&regular, link], 4096, 1),
        ] {
            let mut argv = vec![std::ffi::OsString::from("--json")];
            argv.extend(
                operands
                    .iter()
                    .map(|path| std::ffi::OsStr::from_bytes(path.to_bytes()).to_owned()),
            );
            let options = crate::args::parse(argv).unwrap();
            let scan = crate::scan::run(&options).unwrap();
            assert_eq!(scan.errors, 0);
            assert_eq!(scan.files, operands.len() as u64);
            assert!(scan.skipped.is_empty());
            assert_eq!(scan.domains.len(), 1);
            assert_eq!(scan.tree.nodes[0].physical, physical);
            let mut json = Vec::new();
            crate::output::write(&scan, &options, 0.0, &mut json).unwrap();
            let report: serde_json::Value = serde_json::from_slice(&json).unwrap();
            assert_eq!(report["filesystem"]["mounted_from"], expected_source);
            assert_eq!(report["coverage"]["error_count"], 0);
            assert_eq!(report["total_physical_bytes"], physical);
            assert_eq!(report["entries"].as_array().unwrap().len(), operands.len());
            assert_eq!(report["entries"][link_entry]["physical_bytes"], 0);
            let by_container = report["physical_bytes_by_container"].as_object().unwrap();
            assert_eq!(by_container.len(), 1);
            assert_eq!(
                by_container.values().next().unwrap(),
                &serde_json::json!(physical)
            );
        }
    }
}

#[test]
fn nofollow_filesystem_parent_handles_root_relative_and_dotdot_paths() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.join("nested")).unwrap();
    let link = fixture.path("link");
    std::os::unix::fs::symlink("missing-target", path(&link)).unwrap();
    let through_dotdot = fixture.path("nested/../link");
    let expected = native::filesystem(&fixture.path("")).unwrap();
    let actual = native::filesystem_nofollow(&through_dotdot, true).unwrap();
    assert_eq!(
        unsafe { CStr::from_ptr(actual.f_mntfromname.as_ptr()) },
        unsafe { CStr::from_ptr(expected.f_mntfromname.as_ptr()) }
    );

    let root = native::filesystem(c"/").unwrap();
    let root_parent = native::filesystem_nofollow(c"/synthetic-link", true).unwrap();
    assert_eq!(
        unsafe { CStr::from_ptr(root_parent.f_mntfromname.as_ptr()) },
        unsafe { CStr::from_ptr(root.f_mntfromname.as_ptr()) }
    );
    let cwd = native::filesystem(c".").unwrap();
    let relative_parent = native::filesystem_nofollow(c"synthetic-link", true).unwrap();
    assert_eq!(
        unsafe { CStr::from_ptr(relative_parent.f_mntfromname.as_ptr()) },
        unsafe { CStr::from_ptr(cwd.f_mntfromname.as_ptr()) }
    );
    let root_itself = native::filesystem_nofollow(c"/", false).unwrap();
    assert_eq!(
        unsafe { CStr::from_ptr(root_itself.f_mntfromname.as_ptr()) },
        unsafe { CStr::from_ptr(root.f_mntfromname.as_ptr()) }
    );
}

#[test]
fn unreadable_directories_are_not_displayed_as_empty() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("permission-denial fixture requires an unprivileged test runner");
        return;
    }
    let fixture = Fixture::new();
    let denied = fixture.0.join("denied");
    fs::create_dir(&denied).unwrap();
    fs::write(denied.join("hidden"), [7; 4096]).unwrap();
    fs::write(fixture.0.join("visible"), [9; 4096]).unwrap();
    let _restore = DenyAccess::new(&denied);

    for (flag, operand) in [("-s", &denied), ("-a", &fixture.0), ("-s", &fixture.0)] {
        let mut options = crate::args::parse([
            std::ffi::OsString::from(flag),
            std::ffi::OsString::from("-B1"),
            operand.as_os_str().to_owned(),
        ])
        .unwrap();
        let scan = crate::scan::run(&options).unwrap();
        assert_eq!(scan.unreadable, 1);
        assert!(!crate::output::complete(&scan));
        let mut text = Vec::new();
        crate::output::write(&scan, &options, 0.0, &mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(!text.contains(denied.to_str().unwrap()));
        if operand == &denied {
            assert!(text.is_empty());
        } else {
            assert!(text.contains(&format!("4096\t{}\n", fixture.0.display())));
        }
        options.json = true;
        let mut output = Vec::new();
        crate::output::write(&scan, &options, 0.0, &mut output).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let denied_entry = report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["path"] == denied.to_str().unwrap());
        if flag == "-a" || operand == &denied {
            assert_eq!(denied_entry.unwrap()["enumeration_complete"], false);
        } else {
            assert!(denied_entry.is_none());
            assert!(report["entries"][0].get("enumeration_complete").is_none());
        }
    }
}

fn report_for(paths: &[&CStr]) -> (crate::scan::Scan, serde_json::Value) {
    let mut args = vec![
        std::ffi::OsString::from("--json"),
        std::ffi::OsString::from("-s"),
    ];
    args.extend(
        paths
            .iter()
            .map(|path| std::ffi::OsStr::from_bytes(path.to_bytes()).to_owned()),
    );
    let options = crate::args::parse(args).unwrap();
    let scan = crate::scan::run(&options).unwrap();
    let mut bytes = Vec::new();
    crate::output::write(&scan, &options, 0.0, &mut bytes).unwrap();
    (scan, serde_json::from_slice(&bytes).unwrap())
}

#[test]
fn missing_operands_preserve_valid_results_and_all_failed_json() {
    let fixture = Fixture::new();
    let data = fixture.path("data");
    let missing = fixture.path("missing");
    let also_missing = fixture.path("also-missing");
    fs::write(path(&data), [7; 4096]).unwrap();
    let expected = native::stat(&data).unwrap().allocated;
    assert!(expected > 0);
    for (paths, failed_index) in [([&missing, &data], 0), ([&data, &missing], 1)] {
        let (scan, report) = report_for(&paths.map(|path| path.as_c_str()));
        assert_eq!(scan.operand_failures.len(), 1);
        assert_eq!(scan.operand_failures[0].index, failed_index);
        assert_eq!(scan.filesystem_operand, Some(1 - failed_index));
        assert_eq!(report["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            report["entries"][0]["path"],
            data.to_string_lossy().as_ref()
        );
        assert_eq!(report["total_physical_bytes"], expected);
        assert_eq!(report["coverage"]["error_count"], 1);
        assert_eq!(
            report["coverage"]["operand_failures"][0]["operand_index"],
            failed_index
        );
        assert_eq!(
            report["coverage"]["operand_failures"][0]["display_path"],
            missing.to_string_lossy().as_ref()
        );
        assert_eq!(
            report["filesystem"]["scope_path"],
            data.to_string_lossy().as_ref()
        );
        assert!(!report["coverage"]["complete"].as_bool().unwrap());
    }
    let link = fixture.path("link");
    std::os::unix::fs::symlink(path(&data), path(&link)).unwrap();
    let (scan, report) = report_for(&[&missing, &link]);
    assert_eq!(scan.filesystem_operand, Some(1));
    assert_eq!(
        report["filesystem"]["scope_path"],
        link.to_string_lossy().as_ref()
    );

    let (scan, report) = report_for(&[&missing, &also_missing]);
    assert_eq!(scan.filesystem_operand, None);
    assert_eq!(
        scan.operand_failures
            .iter()
            .map(|f| f.index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(report["entries"].as_array().unwrap().len(), 0);
    assert_eq!(report["filesystem"], serde_json::Value::Null);
    assert_eq!(report["total_physical_bytes"], 0);
    assert_eq!(report["coverage"]["error_count"], 2);
    assert_eq!(
        report["coverage"]["operand_failures"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn unreadable_and_unsupported_operands_do_not_hide_duplicates_or_zero() {
    let fixture = Fixture::new();
    let data = fixture.path("data");
    let zero = fixture.path("zero");
    let hidden_dir = fixture.0.join("blocked");
    fs::create_dir(&hidden_dir).unwrap();
    let hidden = fixture.path("blocked/inside");
    fs::write(path(&hidden), [1; 4096]).unwrap();
    fs::write(path(&data), [2; 4096]).unwrap();
    fs::write(path(&zero), []).unwrap();
    let expected = native::stat(&data).unwrap().allocated;
    if unsafe { libc::geteuid() } != 0 {
        let _restore = DenyAccess::new(&hidden_dir);
        let (scan, report) = report_for(&[&hidden, &data, &data, &zero]);
        assert_eq!(scan.operand_failures[0].index, 0);
        assert_eq!(scan.filesystem_operand, Some(1));
        assert_eq!(report["entries"].as_array().unwrap().len(), 3);
        assert_eq!(report["entries"][0]["physical_bytes"], expected);
        assert_eq!(report["entries"][1]["physical_bytes"], expected);
        assert_eq!(report["entries"][2]["physical_bytes"], 0);
        assert_eq!(report["total_physical_bytes"], expected);
        assert_eq!(
            report["coverage"]["operand_failures"][0]["operand_index"],
            0
        );
    } else {
        eprintln!("permission-denial fixture requires an unprivileged test runner");
    }

    // /dev/null is a read-only devfs operand; all mutable fixture data is owned.
    let unsupported = c"/dev/null";
    let (scan, report) = report_for(&[unsupported, &data]);
    assert_eq!(scan.operand_failures.len(), 1);
    assert_eq!(scan.operand_failures[0].index, 0);
    assert_eq!(report["entries"].as_array().unwrap().len(), 1);
    assert_eq!(report["total_physical_bytes"], expected);
    assert_eq!(
        report["filesystem"]["scope_path"],
        data.to_string_lossy().as_ref()
    );
    assert_eq!(
        report["coverage"]["operand_failures"][0]["display_path"],
        "/dev/null"
    );
}

#[test]
fn json_keeps_measured_results_if_control_path_disappears_after_scan() {
    let fixture = Fixture::new();
    let data = fixture.path("transient");
    fs::write(path(&data), [9; 4096]).unwrap();
    let options = crate::args::parse(vec![
        std::ffi::OsString::from("--json"),
        std::ffi::OsStr::from_bytes(data.to_bytes()).to_owned(),
    ])
    .unwrap();
    let scan = crate::scan::run(&options).unwrap();
    assert!(scan.filesystem_control.is_some());
    fs::remove_file(path(&data)).unwrap();
    let mut bytes = Vec::new();
    crate::output::write(&scan, &options, 0.0, &mut bytes).unwrap();
    let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        report["filesystem"]["scope_path"],
        data.to_string_lossy().as_ref()
    );
    assert_eq!(report["entries"].as_array().unwrap().len(), 1);
    assert!(report["total_physical_bytes"].as_u64().unwrap() > 0);
}
