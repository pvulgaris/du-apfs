use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

pub const USAGE: &str =
    "usage: du-apfs [-achkmgsx0] [-d depth] [-B blocksize] [--json] [--workers N] [path ...]

APFS-aware physical usage. Common du flags are supported:
  -a  include files                 -s  show only each operand
  -d  maximum directory depth      -h  human-readable sizes
  -k/-m/-g  KiB/MiB/GiB units       -B  block size (for example -B1)
  -c  print a distinct total       -x  stay on each operand's filesystem
  -0  NUL-terminate rows           --json  structured byte counts
  --workers N  mapping workers (default 3)
  --help  show this help
";

#[derive(Debug, PartialEq, Eq)]
pub struct Options {
    pub all: bool,
    pub summary: bool,
    pub human: bool,
    pub total: bool,
    pub one_fs: bool,
    pub json: bool,
    pub null: bool,
    pub help: bool,
    pub depth: Option<usize>,
    pub block_size: i64,
    pub paths: Vec<PathBuf>,
    pub workers: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            all: false,
            summary: false,
            human: false,
            total: false,
            one_fs: false,
            json: false,
            null: false,
            help: false,
            depth: None,
            block_size: 512,
            paths: Vec::new(),
            workers: 3,
        }
    }
}

fn number(value: &[u8], error: &str) -> Result<usize, String> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| error.to_owned())
}

fn block_size(value: &[u8]) -> Result<i64, String> {
    if value.is_empty() {
        return Err("empty block size".into());
    }
    let (number, multiplier) = match value.last().copied().unwrap().to_ascii_uppercase() {
        b'K' => (&value[..value.len() - 1], 1_i64 << 10),
        b'M' => (&value[..value.len() - 1], 1_i64 << 20),
        b'G' => (&value[..value.len() - 1], 1_i64 << 30),
        b'T' => (&value[..value.len() - 1], 1_i64 << 40),
        _ => (value, 1),
    };
    let size = std::str::from_utf8(number)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|&n| n > 0)
        .and_then(|n| n.checked_mul(multiplier))
        .ok_or_else(|| "invalid block size".to_owned())?;
    Ok(size)
}

pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut args = args.into_iter();
    let mut options = Options::default();
    let mut operands_only = false;
    while let Some(arg) = args.next() {
        let bytes = arg.as_bytes();
        if operands_only || bytes == b"-" || !bytes.starts_with(b"-") {
            if bytes.contains(&0) {
                return Err("path contains NUL".into());
            }
            options.paths.push(PathBuf::from(arg));
            continue;
        }
        if bytes == b"--" {
            operands_only = true;
            continue;
        }
        if let Some(long) = bytes.strip_prefix(b"--") {
            let (name, inline_value) = match long.iter().position(|&b| b == b'=') {
                Some(index) => (&long[..index], Some(&long[index + 1..])),
                None => (long, None),
            };
            match name {
                b"help" | b"json" | b"human-readable" | b"summarize" | b"all" | b"total"
                | b"one-file-system" | b"null" => {
                    if inline_value.is_some() {
                        return Err(format!(
                            "--{} does not take a value",
                            String::from_utf8_lossy(name)
                        ));
                    }
                    match name {
                        b"help" => {
                            options.help = true;
                            return Ok(options);
                        }
                        b"json" => options.json = true,
                        b"human-readable" => options.human = true,
                        b"summarize" => options.summary = true,
                        b"all" => options.all = true,
                        b"total" => options.total = true,
                        b"one-file-system" => options.one_fs = true,
                        b"null" => options.null = true,
                        _ => unreachable!(),
                    }
                }
                b"max-depth" | b"block-size" | b"workers" => {
                    let value = match inline_value {
                        Some(value) => value.to_vec(),
                        None => args
                            .next()
                            .ok_or_else(|| {
                                format!("--{} needs a value", String::from_utf8_lossy(name))
                            })?
                            .as_bytes()
                            .to_vec(),
                    };
                    match name {
                        b"max-depth" => options.depth = Some(number(&value, "invalid depth")?),
                        b"block-size" => {
                            options.block_size = block_size(&value)?;
                            options.human = false;
                        }
                        b"workers" => {
                            options.workers = number(&value, "invalid worker count")?;
                            if !(1..=256).contains(&options.workers) {
                                return Err("invalid worker count".into());
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                _ => {
                    return Err(format!(
                        "unsupported option --{}",
                        String::from_utf8_lossy(name)
                    ));
                }
            }
            continue;
        }
        let mut index = 1;
        while index < bytes.len() {
            match bytes[index] {
                b'a' => options.all = true,
                b's' => options.summary = true,
                b'h' => options.human = true,
                b'c' => options.total = true,
                b'x' => options.one_fs = true,
                b'0' => options.null = true,
                b'k' | b'm' | b'g' => {
                    options.block_size = match bytes[index] {
                        b'k' => 1 << 10,
                        b'm' => 1 << 20,
                        _ => 1 << 30,
                    };
                    options.human = false;
                }
                b'd' | b'B' => {
                    let value = if index + 1 < bytes.len() {
                        bytes[index + 1..].to_vec()
                    } else {
                        args.next()
                            .ok_or_else(|| format!("-{} needs a value", bytes[index] as char))?
                            .as_bytes()
                            .to_vec()
                    };
                    if bytes[index] == b'd' {
                        options.depth = Some(number(&value, "invalid depth")?);
                    } else {
                        options.block_size = block_size(&value)?;
                        options.human = false;
                    }
                    break;
                }
                flag => return Err(format!("unsupported option -{}", flag as char)),
            }
            index += 1;
        }
    }
    if options.all && options.summary {
        return Err("-a and -s cannot be combined".into());
    }
    if options.paths.is_empty() {
        options.paths.push(PathBuf::from("."));
    }
    Ok(options)
}

pub fn formatted_size(bytes: i64, options: &Options) -> String {
    if options.human {
        if bytes == 0 {
            return "0B".into();
        }
        let mut value = bytes as f64;
        for unit in ["B", "K", "M", "G", "T"] {
            if value < 1024.0 || unit == "T" {
                return if unit == "B" {
                    format!("{bytes}B")
                } else {
                    format!("{value:.1}{unit}")
                };
            }
            value /= 1024.0;
        }
        unreachable!();
    }
    (bytes / options.block_size + i64::from(bytes % options.block_size > 0)).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn parse_str(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn defaults_and_combined_flags() {
        let defaults = parse_str(&[]).unwrap();
        assert_eq!(defaults.paths, vec![PathBuf::from(".")]);
        assert_eq!(defaults.block_size, 512);
        assert_eq!(defaults.workers, 3);
        assert_eq!(defaults.depth, None);

        let options =
            parse_str(&["-achx0k", "-d2", "-B1K", "--json", "--workers=4", "root"]).unwrap();
        assert!(options.all && options.total && options.one_fs && options.null && options.json);
        assert!(!options.human);
        assert_eq!(options.depth, Some(2));
        assert_eq!(options.block_size, 1024);
        assert_eq!(options.workers, 4);
        assert_eq!(options.paths, vec![PathBuf::from("root")]);
    }

    #[test]
    fn operands_preserve_non_utf8_bytes_and_separator() {
        let raw = OsString::from_vec(b"name-\xff".to_vec());
        let options = parse([
            OsString::from("-"),
            raw.clone(),
            OsString::from("--"),
            OsString::from("--json"),
            OsString::from("-d7"),
        ])
        .unwrap();
        assert_eq!(options.paths[0], PathBuf::from("-"));
        assert_eq!(options.paths[1].as_os_str().as_bytes(), raw.as_bytes());
        assert_eq!(options.paths[2], PathBuf::from("--json"));
        assert_eq!(options.paths[3], PathBuf::from("-d7"));
        assert!(!options.json);
        assert_eq!(options.depth, None);
    }

    #[test]
    fn option_order_controls_units() {
        let a = parse_str(&["-h", "-k"]).unwrap();
        let b = parse_str(&["-k", "-h"]).unwrap();
        assert_eq!(formatted_size(1536, &a), "2");
        assert_eq!(formatted_size(1536, &b), "1.5K");
        assert_eq!(formatted_size(1023, &b), "1023B");
        assert_eq!(formatted_size(0, &b), "0B");
        assert_eq!(formatted_size(1537, &Options::default()), "4");
        assert_eq!(
            parse_str(&["--block-size", "2m"]).unwrap().block_size,
            2 << 20
        );
        let after_operand = parse_str(&["root", "--max-depth=0", "--workers", "2", "-g"]).unwrap();
        assert_eq!(after_operand.paths, vec![PathBuf::from("root")]);
        assert_eq!(after_operand.depth, Some(0));
        assert_eq!(after_operand.workers, 2);
        assert_eq!(after_operand.block_size, 1 << 30);
    }

    #[test]
    fn long_aliases_set_the_same_flags() {
        let options = parse_str(&[
            "--all",
            "--human-readable",
            "--total",
            "--one-file-system",
            "--null",
            "--json",
        ])
        .unwrap();
        assert!(options.all);
        assert!(options.human);
        assert!(options.total);
        assert!(options.one_fs);
        assert!(options.null);
        assert!(options.json);
        assert!(parse_str(&["--summarize"]).unwrap().summary);
    }

    #[test]
    fn help_and_invalid_options() {
        assert!(parse_str(&["--help"]).unwrap().help);
        for args in [
            vec!["--block-size=0"],
            vec!["--block-size=9223372036854775808"],
            vec!["--block-size=8388608T"],
            vec!["--max-depth=-1"],
            vec!["--max-depth=18446744073709551616"],
            vec!["--workers=0"],
            vec!["--workers=257"],
            vec!["--workers=abc"],
            vec!["--workers"],
            vec!["-B"],
            vec!["-d"],
            vec!["-a", "-s"],
            vec!["--apparent-size"],
        ] {
            assert!(parse_str(&args).is_err(), "{args:?}");
        }
        for flag in [
            "help",
            "json",
            "human-readable",
            "summarize",
            "all",
            "total",
            "one-file-system",
            "null",
        ] {
            assert!(parse_str(&[&format!("--{flag}=yes")]).is_err(), "{flag}");
        }
        assert!(parse([OsString::from_vec(b"--workers=\xff".to_vec())]).is_err());
    }
}
