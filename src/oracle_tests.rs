use crate::native;
use std::ffi::CString;
use std::io::{BufWriter, Write};
use std::os::unix::ffi::OsStrExt;

fn real_device(path: &std::ffi::CStr) -> std::io::Result<Option<u32>> {
    let mut attrs = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0x80000000,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0x40,
    };
    let mut reply = [0u8; 28];
    // POSIX device IDs unify System and Data. The raw-volume oracle must
    // select the real volume, including for symlinks and mount points.
    if unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attrs as *mut libc::attrlist).cast(),
            reply.as_mut_ptr().cast(),
            reply.len(),
            0x21,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let returned = u32::from_le_bytes(reply[20..24].try_into().unwrap());
    Ok((returned & 0x40 != 0).then(|| u32::from_le_bytes(reply[24..28].try_into().unwrap())))
}

#[test]
#[ignore = "requires DU_APFS_RESEARCH_ROOT read-only snapshot and DU_APFS_RESEARCH_OUTPUT"]
fn export_snapshot_extents() {
    let _policy = crate::materialization::MaterializationGuard::disable().unwrap();
    let root = std::env::var_os("DU_APFS_RESEARCH_ROOT").expect("snapshot root");
    let root = CString::new(root.as_bytes()).unwrap();
    let output = std::env::var_os("DU_APFS_RESEARCH_OUTPUT").expect("output file");
    let filesystem = native::filesystem(&root).unwrap();
    assert_ne!(
        filesystem.f_flags & 0x40000000,
        0,
        "requires snapshot mount"
    );
    assert_ne!(filesystem.f_flags & 1, 0, "requires read-only mount");
    assert_eq!(filesystem.f_bsize, 4096);
    let root_device = real_device(&root).unwrap().expect("real root device");
    let mut output = BufWriter::new(std::fs::File::create(output).unwrap());
    let mut stack = vec![root.clone()];
    while let Some(path) = stack.pop() {
        if real_device(&path).unwrap() != Some(root_device) {
            continue;
        }
        let metadata = native::stat(&path).unwrap();
        if metadata.directory() {
            let p = std::path::Path::new(std::ffi::OsStr::from_bytes(path.to_bytes()));
            for entry in std::fs::read_dir(p).unwrap() {
                stack.push(native::join(&path, entry.unwrap().file_name().as_bytes()));
            }
            continue;
        }
        let ranges: Vec<[i64; 2]> = if metadata.regular() && metadata.allocated > 0 {
            let (ranges, _) = native::map_file(&path, metadata, 4096)
                .extents
                .unwrap_or_else(|error| panic!("failed to map {:?}: {error}", path));
            ranges.iter().map(|r| [r.start, r.end]).collect()
        } else {
            Vec::new()
        };
        let prefix = root.to_bytes().len() + usize::from(!root.to_bytes().ends_with(b"/"));
        let relative = &path.to_bytes()[prefix..];
        // The oracle fixture deliberately uses UTF-8 names. Reject a different
        // fixture instead of hiding collisions behind lossy name conversion.
        let relative = std::str::from_utf8(relative).unwrap();
        serde_json::to_writer(
            &mut output,
            &serde_json::json!({"path": relative, "ranges": ranges}),
        )
        .unwrap();
        writeln!(output).unwrap();
    }
    output.flush().unwrap();
}
