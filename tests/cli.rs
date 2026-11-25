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

#[test]
fn an_unreadable_directory_is_reported_once() {
    if running_as_root() {
        eprintln!("permission-denial fixture requires an unprivileged test runner");
        return;
    }
    let fixture = Fixture::new("denied");
    // The escape character checks that names cannot drive the terminal.
    let denied = fixture.0.join("denied\x1b");
    fs::create_dir(&denied).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    let output = du_apfs(&["-s"], &fixture.0);
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        format!(
            "du-apfs: {}/denied\\u{{1b}}: Permission denied (os error 13)\n\
             du-apfs: incomplete: 1 error, 0 bytes known unmeasured\n",
            fixture.0.display()
        )
    );
}

fn running_as_root() -> bool {
    Command::new("id").arg("-u").output().unwrap().stdout == b"0\n"
}

#[test]
fn a_closed_pipe_ends_quietly() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    let fixture = Fixture::new("pipe");
    fs::write(fixture.0.join("file"), [7; 4096]).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_du-apfs"))
        .arg("-a")
        .arg(&fixture.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Close the reading end before the scan writes anything.
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.signal(), Some(13), "{output:?}"); // SIGPIPE
    assert!(output.stderr.is_empty(), "{output:?}");
}
