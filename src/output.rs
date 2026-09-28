use crate::{accounting::Node, args, scan::Scan};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::ffi::{CStr, OsStr};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub fn complete(scan: &Scan) -> bool {
    scan.errors == 0 && scan.tree.nodes[0].unmeasured == 0 && scan.unknown == 0
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn entries(nodes: &[Node]) -> impl Iterator<Item = &Node> {
    // The walker retains only requested output nodes, grouped by operand.
    // Reverse each group so children precede their parent, while operands
    // keep command-line order, including gaps from failed operands.
    nodes[1..]
        .chunk_by(|a, b| a.operand == b.operand)
        .flat_map(|operand| operand.iter().rev())
}

fn display_path(path: &CStr, root: &CStr, operand: &Path) -> Vec<u8> {
    let path_bytes = path.to_bytes();
    if path_bytes == root.to_bytes() {
        return operand.as_os_str().as_bytes().to_vec();
    }
    let path = Path::new(OsStr::from_bytes(path_bytes));
    let root = Path::new(OsStr::from_bytes(root.to_bytes()));
    let Ok(relative) = path.strip_prefix(root) else {
        return path_bytes.to_vec();
    };
    let mut display = operand.as_os_str().as_bytes().to_vec();
    if !display.ends_with(b"/") {
        display.push(b'/');
    }
    display.extend_from_slice(relative.as_os_str().as_bytes());
    display
}

fn write_text(
    nodes: &[Node],
    roots: &[std::ffi::CString],
    options: &args::Options,
    out: impl Write,
) -> io::Result<()> {
    let mut out = BufWriter::new(out);
    let end: &[u8] = if options.null { b"\0" } else { b"\n" };
    for node in entries(nodes) {
        let operand = node.operand;
        if node.directory_read_error {
            continue;
        }
        out.write_all(args::formatted_size(node.physical, options).as_bytes())?;
        out.write_all(b"\t")?;
        out.write_all(&display_path(
            &node.path,
            &roots[operand],
            &options.paths[operand],
        ))?;
        out.write_all(end)?;
    }
    if options.total && nodes.iter().any(|node| node.root) {
        out.write_all(args::formatted_size(nodes[0].physical, options).as_bytes())?;
        out.write_all(b"\ttotal")?;
        out.write_all(end)?;
    }
    out.flush()
}

fn add_path(value: &mut Map<String, Value>, key: &str, bytes: &[u8]) {
    match std::str::from_utf8(bytes) {
        Ok(path) => {
            value.insert(key.into(), json!(path));
        }
        Err(_) => {
            value.insert(key.into(), json!(String::from_utf8_lossy(bytes)));
            value.insert(format!("{key}_bytes"), json!(bytes));
        }
    }
}

fn cchars(chars: &[libc::c_char]) -> Vec<u8> {
    chars
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect()
}

fn checked_bytes(blocks: u64, block_size: u64) -> io::Result<u64> {
    blocks
        .checked_mul(block_size)
        .ok_or_else(|| invalid("filesystem byte count overflow"))
}

fn write_json_entries(
    nodes: &[Node],
    roots: &[std::ffi::CString],
    options: &args::Options,
    out: &mut impl Write,
) -> io::Result<()> {
    out.write_all(b"[")?;
    let mut first = true;
    for node in entries(nodes) {
        let operand = node.operand;
        if !first {
            out.write_all(b",")?;
        }
        first = false;
        let mut entry = Map::new();
        add_path(&mut entry, "path", node.path.to_bytes());
        add_path(
            &mut entry,
            "display_path",
            &display_path(&node.path, &roots[operand], &options.paths[operand]),
        );
        entry.insert(
            "type".into(),
            json!(if node.directory { "directory" } else { "file" }),
        );
        entry.insert("physical_bytes".into(), json!(node.physical));
        entry.insert("allocated_bytes".into(), json!(node.allocated));
        entry.insert("unmeasured_bytes".into(), json!(node.unmeasured));
        if node.directory_read_error {
            entry.insert("enumeration_complete".into(), json!(false));
        }
        serde_json::to_writer(&mut *out, &entry)?;
    }
    out.write_all(b"]")
}

pub fn write(
    scan: &Scan,
    options: &args::Options,
    elapsed: f64,
    out: impl Write,
) -> io::Result<()> {
    if !options.json {
        return write_text(&scan.tree.nodes, &scan.roots, options, out);
    }

    let filesystem = if let (Some(operand), Some(fs)) =
        (scan.filesystem_operand, scan.filesystem_control.as_ref())
    {
        let block_size = u64::from(fs.f_bsize);
        if block_size == 0 {
            return Err(invalid("invalid filesystem block size"));
        }
        let used_blocks = fs
            .f_blocks
            .checked_sub(fs.f_bfree)
            .ok_or_else(|| invalid("filesystem free blocks exceed capacity"))?;
        let mut filesystem = Map::new();
        filesystem.insert("measurement".into(), json!("statfs_capacity_minus_free"));
        add_path(
            &mut filesystem,
            "scope_path",
            scan.roots[operand].to_bytes(),
        );
        filesystem.insert(
            "mounted_from".into(),
            json!(String::from_utf8_lossy(&cchars(&fs.f_mntfromname))),
        );
        filesystem.insert(
            "capacity_bytes".into(),
            json!(checked_bytes(fs.f_blocks, block_size)?),
        );
        filesystem.insert(
            "free_bytes".into(),
            json!(checked_bytes(fs.f_bfree, block_size)?),
        );
        filesystem.insert(
            "used_bytes".into(),
            json!(checked_bytes(used_blocks, block_size)?),
        );
        Value::Object(filesystem)
    } else {
        Value::Null
    };

    let by_container: BTreeMap<_, _> = scan
        .domains
        .iter()
        .map(|(name, id)| (name.clone(), *scan.tree.domain_bytes.get(id).unwrap_or(&0)))
        .collect();
    let mut out = BufWriter::new(out);
    out.write_all(
        b"{\"schema_version\":1,\"measurement\":\"distinct_file_fork_extents\",\"entries\":",
    )?;
    write_json_entries(&scan.tree.nodes, &scan.roots, options, &mut out)?;
    out.write_all(b",\"total_physical_bytes\":")?;
    serde_json::to_writer(&mut out, &scan.tree.nodes[0].physical)?;
    out.write_all(b",\"physical_bytes_by_container\":")?;
    serde_json::to_writer(&mut out, &by_container)?;
    out.write_all(b",\"filesystem\":")?;
    serde_json::to_writer(&mut out, &filesystem)?;
    out.write_all(b",\"coverage\":{\"complete\":")?;
    serde_json::to_writer(&mut out, &complete(scan))?;
    out.write_all(b",\"unmapped_xattr_entries\":")?;
    serde_json::to_writer(&mut out, &scan.unknown)?;
    out.write_all(b",\"unmapped_xattr_paths\":")?;
    serde_json::to_writer(&mut out, &scan.unknown_paths)?;
    if !scan.operand_failures.is_empty() {
        out.write_all(b",\"operand_failures\":[")?;
        for (position, failure) in scan.operand_failures.iter().enumerate() {
            if position != 0 {
                out.write_all(b",")?;
            }
            let mut item = Map::new();
            item.insert("operand_index".into(), json!(failure.index));
            add_path(&mut item, "path", scan.roots[failure.index].to_bytes());
            add_path(
                &mut item,
                "display_path",
                options.paths[failure.index].as_os_str().as_bytes(),
            );
            item.insert("reason".into(), json!(failure.reason));
            serde_json::to_writer(&mut out, &item)?;
        }
        out.write_all(b"]")?;
    }
    out.write_all(b",\"error_count\":")?;
    serde_json::to_writer(&mut out, &scan.errors)?;
    out.write_all(b",\"error_counts\":")?;
    serde_json::to_writer(&mut out, &scan.error_counts)?;
    out.write_all(b",\"errors\":")?;
    serde_json::to_writer(&mut out, &scan.error_details)?;
    out.write_all(b",\"errors_truncated\":")?;
    serde_json::to_writer(&mut out, &(scan.errors > scan.error_details.len() as u64))?;
    out.write_all(b",\"unmeasured_bytes\":")?;
    serde_json::to_writer(&mut out, &scan.tree.nodes[0].unmeasured)?;
    out.write_all(b",\"unreadable_directories\":")?;
    serde_json::to_writer(&mut out, &scan.unreadable)?;
    out.write_all(b",\"unreadable_directory_paths\":")?;
    serde_json::to_writer(&mut out, &scan.unreadable_paths)?;
    out.write_all(b",\"skipped\":")?;
    serde_json::to_writer(&mut out, &scan.skipped)?;
    out.write_all(b"},\"files_scanned\":")?;
    serde_json::to_writer(&mut out, &scan.files)?;
    out.write_all(b",\"elapsed_seconds\":")?;
    serde_json::to_writer(&mut out, &elapsed)?;
    out.write_all(b"}\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_tests::Fixture;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStringExt;

    fn node(
        path: &[u8],
        parent: usize,
        operand: usize,
        directory: bool,
        root: bool,
        physical: i64,
    ) -> Node {
        Node {
            path: CString::new(path).unwrap(),
            parent: Some(parent),
            operand,
            directory,
            directory_read_error: false,
            root,
            physical,
            allocated: physical,
            unmeasured: 0,
        }
    }

    #[test]
    fn postorder_per_operand_and_raw_text_paths() {
        let nodes = vec![
            node(b"", 0, 0, true, false, 4096),
            node(b"/first", 0, 0, true, true, 2048),
            node(b"/first/name-\xff", 1, 0, false, false, 1024),
            node(b"/first/sub", 1, 0, true, false, 512),
            node(b"/second", 0, 1, true, true, 2048),
        ];
        let roots = vec![
            CString::new("/first").unwrap(),
            CString::new("/second").unwrap(),
        ];
        let mut options = args::Options {
            paths: vec![
                Path::new(".").to_path_buf(),
                Path::new("/second/").to_path_buf(),
            ],
            all: true,
            null: true,
            total: true,
            block_size: 1,
            ..Default::default()
        };
        let mut out = Vec::new();
        write_text(&nodes, &roots, &options, &mut out).unwrap();
        assert_eq!(
            out,
            b"512\t./sub\0\
1024\t./name-\xff\0\
2048\t.\0\
2048\t/second/\0\
4096\ttotal\0"
        );
        options.json = true;
        let mut json_bytes = Vec::new();
        write_json_entries(&nodes, &roots, &options, &mut json_bytes).unwrap();
        let entries: Value = serde_json::from_slice(&json_bytes).unwrap();
        let entries = entries.as_array().unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0]["path"], "/first/sub");
        assert_eq!(entries[1]["path_bytes"], json!(b"/first/name-\xff"));
        assert_eq!(entries[1]["display_path_bytes"], json!(b"./name-\xff"));
        assert_eq!(entries[1]["type"], "file");
        assert_eq!(entries[1]["physical_bytes"], 1024);
        assert_eq!(entries[2]["display_path"], ".");
        assert_eq!(entries[3]["display_path"], "/second/");
    }

    #[test]
    fn display_paths_preserve_raw_bytes() {
        let mut object = Map::new();
        add_path(&mut object, "path", b"/root/file-\xff");
        assert_eq!(object["path_bytes"], json!(b"/root/file-\xff"));
        assert!(object["path"].as_str().unwrap().contains('\u{fffd}'));
        let raw_operand = std::ffi::OsString::from_vec(b"name-\xff/".to_vec());
        assert_eq!(
            display_path(c"/root/child", c"/root", Path::new(&raw_operand)),
            b"name-\xff/child"
        );
        assert_eq!(
            display_path(c"/Applications", c"/", Path::new("/")),
            b"/Applications"
        );
    }

    #[test]
    fn writer_errors_are_propagated() {
        struct Reject;
        impl Write for Reject {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let nodes = vec![
            node(b"", 0, 0, true, false, 0),
            node(b"/root", 0, 0, true, true, 0),
        ];
        let roots = vec![CString::new("/root").unwrap()];
        let options = args::Options {
            paths: vec![Path::new("root").to_path_buf()],
            ..Default::default()
        };
        assert_eq!(
            write_text(&nodes, &roots, &options, Reject)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            write_json_entries(&nodes, &roots, &options, &mut Reject)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn streamed_document_has_the_expected_schema() {
        let fixture = Fixture::new();
        let options = args::Options {
            paths: vec![fixture.0.clone()],
            json: true,
            ..Default::default()
        };
        let scan = crate::scan::run(&options).unwrap();
        let mut out = Vec::new();
        write(&scan, &options, 0.5, &mut out).unwrap();
        assert_eq!(out.last(), Some(&b'\n'));
        let report: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["measurement"], "distinct_file_fork_extents");
        assert_eq!(report["elapsed_seconds"], 0.5);
        assert_eq!(report["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            report["entries"][0]["display_path"].as_str(),
            fixture.0.to_str()
        );
        assert_eq!(report["entries"][0]["type"], "directory");
        assert_eq!(
            report["filesystem"]["measurement"],
            "statfs_capacity_minus_free"
        );
        assert!(report["filesystem"]["capacity_bytes"].is_u64());
        assert!(report["physical_bytes_by_container"].is_object());
        assert!(report["coverage"]["complete"].is_boolean());
        assert!(report["coverage"]["errors"].is_array());
    }
}
