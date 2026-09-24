use std::io;

// Public macOS sys/resource.h constants. The libc crate does not expose these APIs.
const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: libc::c_int = 3;
const IOPOL_SCOPE_PROCESS: libc::c_int = 0;
const IOPOL_SCOPE_THREAD: libc::c_int = 1;
const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: libc::c_int = 1;

unsafe extern "C" {
    fn getiopolicy_np(iotype: libc::c_int, scope: libc::c_int) -> libc::c_int;
    fn setiopolicy_np(iotype: libc::c_int, scope: libc::c_int, policy: libc::c_int) -> libc::c_int;
}

fn get_policy(scope: libc::c_int) -> io::Result<libc::c_int> {
    let value = unsafe { getiopolicy_np(IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, scope) };
    if value == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

fn set_policy(policy: libc::c_int) -> io::Result<()> {
    let result = unsafe {
        setiopolicy_np(
            IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            IOPOL_SCOPE_PROCESS,
            policy,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub struct MaterializationGuard {
    previous: libc::c_int,
    active: bool,
}

impl MaterializationGuard {
    pub fn disable() -> io::Result<Self> {
        let previous = get_policy(IOPOL_SCOPE_PROCESS)
            .map_err(|e| io::Error::other(format!("read dataless materialization policy: {e}")))?;
        set_policy(IOPOL_MATERIALIZE_DATALESS_FILES_OFF)
            .map_err(|e| io::Error::other(format!("disable dataless materialization: {e}")))?;
        let mut guard = Self {
            previous,
            active: true,
        };
        let verified = get_policy(IOPOL_SCOPE_PROCESS).and_then(|current| {
            if current == IOPOL_MATERIALIZE_DATALESS_FILES_OFF {
                // An explicit thread policy overrides the process policy.
                // The walker must also be protected before its first stat.
                match get_policy(IOPOL_SCOPE_THREAD)? {
                    0 | IOPOL_MATERIALIZE_DATALESS_FILES_OFF => Ok(()),
                    _ => Err(io::Error::other(
                        "thread policy allows dataless materialization",
                    )),
                }
            } else {
                Err(io::Error::other(
                    "dataless materialization policy did not stay disabled",
                ))
            }
        });
        if let Err(e) = verified {
            return Err(match guard.restore() {
                Ok(()) => e,
                Err(restore) => io::Error::other(format!("{e}; restore policy: {restore}")),
            });
        }
        Ok(guard)
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if self.active {
            set_policy(self.previous)?;
            self.active = false;
        }
        Ok(())
    }
}

impl Drop for MaterializationGuard {
    fn drop(&mut self) {
        // Panic paths get a best-effort restore; ordinary paths report errors.
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_policy_covers_default_workers_and_restores() {
        let previous = get_policy(IOPOL_SCOPE_PROCESS).unwrap();
        let mut guard = MaterializationGuard::disable().unwrap();
        assert_eq!(get_policy(IOPOL_SCOPE_PROCESS).unwrap(), 1);

        let workers: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    // Threads may report their own default (0) or inherited OFF (1).
                    // Their process policy must stay OFF during local metadata I/O.
                    let process = get_policy(IOPOL_SCOPE_PROCESS).unwrap();
                    let thread = get_policy(1).unwrap();
                    let path = std::env::current_exe().unwrap();
                    std::fs::metadata(path).unwrap();
                    (process, thread)
                })
            })
            .collect();
        for worker in workers {
            let (process, thread) = worker.join().unwrap();
            assert_eq!(process, 1);
            assert!(thread == 0 || thread == 1);
        }

        guard.restore().unwrap();
        assert_eq!(get_policy(IOPOL_SCOPE_PROCESS).unwrap(), previous);
    }
}
