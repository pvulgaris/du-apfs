mod accounting;
mod args;
mod materialization;
mod native;
#[cfg(test)]
mod native_tests;
#[cfg(test)]
mod oracle_tests;
mod output;
mod pipeline;
mod scan;

use std::io::{self, Write};
use std::time::Instant;

fn main() {
    // Rust ignores SIGPIPE, which turns `du-apfs | head` into a reported
    // write error. Die from the signal instead, as du does.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let status = match execute() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("du-apfs: {error}");
            1
        }
    };
    if status != 0 {
        std::process::exit(status);
    }
}

// Paths come from the scanned tree; escape control characters so a crafted
// filename cannot send escape sequences to the terminal.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn count(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn execute() -> io::Result<i32> {
    let options = match args::parse(std::env::args_os().skip(1)) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("du-apfs: {error}");
            return Ok(2);
        }
    };
    if options.help {
        io::stdout().lock().write_all(args::USAGE.as_bytes())?;
        return Ok(0);
    }
    if options.version {
        println!("du-apfs {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    // Set policy before resolving operands or asking the filesystem anything.
    // Keep it in force through worker joins and the final statfs report.
    let mut policy = materialization::MaterializationGuard::disable()?;
    let start = Instant::now();
    let result = (|| {
        let scan = scan::run(&options)?;
        let elapsed = start.elapsed().as_secs_f64();
        output::write(&scan, &options, elapsed, io::stdout().lock())?;
        if output::complete(&scan) {
            return Ok(0);
        }
        let mut shown = 0;
        for detail in scan.error_details.iter().take(10) {
            let path = detail["path"].as_str().unwrap_or("?");
            let reason = detail["reason"].as_str().unwrap_or("unknown error");
            eprintln!("du-apfs: {}: {reason}", printable(path));
            shown += 1;
        }
        if scan.errors > shown {
            eprintln!(
                "du-apfs: {} more; see --json coverage",
                count(scan.errors - shown, "error")
            );
        }
        eprintln!(
            "du-apfs: incomplete: {}, {} known unmeasured",
            count(scan.errors, "error"),
            count(scan.tree.nodes[0].unmeasured as u64, "byte")
        );
        Ok(1)
    })();
    policy.restore()?;
    result
}

#[cfg(test)]
use accounting::Bitmap;
#[cfg(test)]
use pipeline::JobPath;
#[cfg(test)]
use std::ffi::CString;
#[cfg(test)]
use std::sync::Arc;

#[cfg(test)]
fn run(root: CString, workers: usize) -> io::Result<scan::Scan> {
    use std::os::unix::ffi::OsStrExt;
    let mut options = args::parse([
        std::ffi::OsString::from("-s"),
        std::ffi::OsStr::from_bytes(root.to_bytes()).to_owned(),
    ])
    .unwrap();
    options.workers = workers;
    scan::run(&options)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deferred_paths_preserve_names_and_parent_lifetime() {
        for parent in ["/", "/parent"] {
            let parent = Arc::new(CString::new(parent).unwrap());
            let name = CString::new("unicode-é-文件\n").unwrap();
            let expected = native::join(&parent, name.to_bytes());
            let path = JobPath::Child {
                parent: parent.clone(),
                name,
            };
            drop(parent);
            let mut buffer = vec![b'x'; 2048];
            assert_eq!(path.in_buffer(&mut buffer), expected.as_c_str());
            assert_eq!(path.materialize(), expected);
            let short = JobPath::Full(CString::new("/x").unwrap());
            assert_eq!(short.in_buffer(&mut buffer).to_bytes(), b"/x");
        }
    }

    #[test]
    fn bitmap_pages_match_individual_block_set() {
        let mut bitmap = Bitmap::default();
        let mut expected = std::collections::HashSet::new();
        let mut seed = 37u64;
        for boundary in [64u64, 4096, 524288, 1 << 33] {
            for _ in 0..60 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let device = 1 + (seed % 3) as i32;
                let first = boundary - 32 + (seed >> 8) % 64;
                let last = first + 1 + (seed >> 16) % 128;
                bitmap.add(
                    device,
                    native::Extent {
                        start: (first * 4096) as i64,
                        end: (last * 4096) as i64,
                    },
                    4096,
                );
                for block in first..last {
                    expected.insert((device, block));
                }
                assert_eq!(bitmap.physical, expected.len() as i64 * 4096);
            }
        }
    }
}
