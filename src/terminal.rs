//! Owning the terminal in one-command mode.
//!
//! The client is the one that puts the tty into raw mode, so the shim cannot ask
//! it to undo that on every way out. What it *can* do is make sure the terminal
//! never outlives the pair in that state — the "bare SGR screen" that reads as a
//! crash. Two failure shapes, two guards:
//!
//! * **The shim dies first** (killed, panics, host reboots the shell). Default
//!   signal death runs no destructor at all, so the fix has to live in the
//!   kernel: the client is armed with `PR_SET_PDEATHSIG = SIGTERM`, and a TUI
//!   that gets SIGTERM restores the terminal and exits (measured: `ICANON`,
//!   `ECHO`, `ISIG`, `IEXTEN` all back on, plus `?1049l`/`?1000l`/`?2004l`).
//!
//! * **The client dies first, unschedulable-y** (SIGKILL, OOM). Nobody is left
//!   to put the modes back, so the shim remembers what they were before the
//!   client ran and restores that on the way out.
//!
//! Deliberately *not* done here: printing a "reconnect" epilogue or clearing the
//! screen. Restoring the modes is what makes the shell usable again; the scroll
//! back is the user's to keep.

use std::os::unix::process::CommandExt;

/// The terminal modes in force before the client was started.
///
/// `None` when there is no terminal to speak of (`antex serve 41999` piped into
/// a file, a CI run, `cargo test`) — then everything here is a no-op.
pub struct Terminal {
    saved: Option<(libc::c_int, libc::termios)>,
}

impl Terminal {
    /// Reads the modes of the terminal this process is talking to.
    ///
    /// Call it *before* spawning the client: once the client is up it has
    /// already switched to raw mode, and there is nothing left worth saving.
    pub fn capture() -> Self {
        Self { saved: live_termios().map(|(fd, attrs)| (fd, attrs)) }
    }

    /// Puts the saved modes back. Idempotent, and safe after a crash of the
    /// client: the file descriptor is the same one the client had.
    pub fn restore(&self) {
        let Some((fd, attrs)) = self.saved.as_ref() else { return };
        // SAFETY: `attrs` came from `tcgetattr` on this same descriptor, and
        // `tcsetattr` only reads it. A closed or non-tty fd fails and is ignored.
        unsafe {
            libc::tcsetattr(*fd, libc::TCSANOW, attrs);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

/// The terminal descriptor to work with, and its current modes.
///
/// Prefers stdin (that is the descriptor the TUI puts in raw mode), and falls
/// back to stdout/stderr so `antex > log` still gets its terminal back.
fn live_termios() -> Option<(libc::c_int, libc::termios)> {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        // SAFETY: `isatty`/`tcgetattr` only inspect the descriptor.
        let is_tty = unsafe { libc::isatty(fd) } == 1;
        if !is_tty {
            continue;
        }
        // SAFETY: `tcgetattr` fills a `termios` we hand it exclusively; the
        // value is only read once it reported success.
        unsafe {
            let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(fd, attrs.as_mut_ptr()) == 0 {
                return Some((fd, attrs.assume_init()));
            }
        }
    }
    None
}

/// Arms the child to take a SIGTERM when the thread that spawns it goes away.
///
/// Called for the client so a killed shim cannot leave an orphan TUI holding the
/// terminal. The parent-death signal is delivered by the kernel at thread exit,
/// which is exactly the moment the shim's `main` stops existing — including the
/// deaths that run no user code at all (SIGKILL, OOM kill, `std::process::exit`).
pub fn die_with_parent(command: &mut std::process::Command) {
    // SAFETY: the closure runs between fork and exec. It only makes two libc
    // calls and loads an integer — no allocation, no locks, nothing that could
    // deadlock in a single-threaded child.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM as libc::c_ulong) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The shim may have died in the window between fork and here, in
            // which case the signal already fired and nothing more will come.
            // Fail the spawn instead of exec'ing a TUI nobody owns.
            if libc::getppid() == 1 {
                return Err(std::io::Error::other("parent exited before exec"));
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guard the whole module exists for: a child spawned this way dies when
    /// the thread that spawned it does — even though the process lives on.
    ///
    /// The child is a plain `sleep`, so its only chance of stopping is the
    /// kernel-delivered SIGTERM; a zombie is enough proof it was delivered.
    #[test]
    fn a_child_takes_sigterm_when_its_spawning_thread_ends() {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            die_with_parent(&mut command);
            let child = command.spawn().expect("spawn sleep");
            tx.send(child.id()).expect("hand the pid over");
            drop(child);
        });

        let pid = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("pid");
        thread.join().expect("spawning thread ends");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // Field 3 is the state; `Z` is a zombie, i.e. the signal landed.
                Ok(stat) => {
                    let state = stat.rsplit(')').next().and_then(|rest| rest.split_whitespace().next());
                    if state == Some("Z") {
                        return;
                    }
                }
                Err(_) => return, // reaped already: also gone
            }
            assert!(
                std::time::Instant::now() < deadline,
                "child {pid} survived its spawning thread"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    /// A shim with no terminal (serve mode, CI, this test runner) must not think
    /// it has modes to restore — and restoring must not fail loudly if it does.
    #[test]
    fn a_process_without_a_terminal_has_nothing_to_restore() {
        let terminal = Terminal::capture();
        terminal.restore();
        terminal.restore();
    }
}
