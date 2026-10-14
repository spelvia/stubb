//! What every agent process Keel spawns shares: it never outlives the host.
//!
//! `kill_on_drop` covers an orderly exit. On Linux the child also asks the
//! kernel for `SIGKILL` when its parent dies (`PR_SET_PDEATHSIG`), so a host
//! that crashes or is killed leaves no agent behind. The signal follows the
//! thread that spawned the child: spawn from a runtime's worker threads, not
//! from a short-lived blocking thread.

use tokio::process::Command;

/// Makes the child die with this process, where the platform allows it.
pub(crate) fn die_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // SAFETY: between fork and exec the closure makes only
        // async-signal-safe calls (`prctl`, `getppid`) and allocates nothing
        // but the error on failure, after which the child exits.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died before the request took effect.
                if u32::try_from(libc::getppid()).ok() != Some(parent) {
                    return Err(std::io::Error::other("the host exited during spawn"));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::Duration;

    /// Whether the process with this ID still runs (and is not a zombie).
    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.rsplit(") ").next().unwrap_or("").starts_with('Z'))
    }

    /// A child spawned in a thread that then exits is killed with it, as it
    /// would be with its host process.
    #[test]
    fn a_child_dies_with_the_thread_that_spawned_it() {
        let pid = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut command = tokio::process::Command::new("sleep");
                command.arg("30");
                super::die_with_parent(&mut command);
                let child = command.spawn().unwrap();
                let pid = child.id().unwrap();
                // Not dropped: kill_on_drop is not what ends it.
                std::mem::forget(child);
                pid
            })
        })
        .join()
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "the child outlived its parent");
    }
}
