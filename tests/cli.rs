//! Runs the built binary: exit status and stderr are part of the interface.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("du-apfs-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn du_apfs(args: &[&str], path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_du-apfs"))
        .args(args)
        .arg(path)
        .output()
        .unwrap()
}

#[test]
fn extended_attributes_do_not_fail_the_scan() {
    let fixture = Fixture::new("xattr");
    let file = fixture.0.join("noted");
    fs::write(&file, [7; 4096]).unwrap();
    let status = Command::new("xattr")
        .args(["-w", "user.note", "hello"])
        .arg(&file)
        .status()
        .unwrap();
    assert!(status.success());
    let output = du_apfs(&["-s"], &fixture.0);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}
