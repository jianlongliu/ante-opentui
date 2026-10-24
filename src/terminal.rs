//! Owning the terminal in one-command mode.
//!
//! The client is the one that puts the tty into raw mode, so the shim cannot ask
//! it to undo that on every way out. What it *can* do is make sure the terminal
//! never outlives the pair in that state — the "bare SGR screen" that reads as a
//! crash. Measured before the fix: the orphaned TUI kept the terminal in raw
//! mode, where `ISIG` is off, so even Ctrl+C produced no signal to get out with.
//!
//! Two guards, for the two ways this can go wrong:
//!
//! * **The shim is signalled** (`kill`, Ctrl+C at the shell, host shutting the
//!   session down) — `forward_death_signals` catches it and passes SIGTERM to the
//!   client, which restores the terminal itself and exits (measured: `ICANON`,
//!   `ECHO`, `ISIG`, `IEXTEN` all back on, plus `?1049l`/`?1000l`/`?2004l`).
//!
//! * **The shim dies uncatchably** (SIGKILL, OOM). No user code runs, so nothing
//!   can forward anything. On Linux the kernel does it: `PR_SET_PDEATHSIG` is
//!   armed in the forked child, so the client takes a SIGTERM the moment its
//!   parent thread exits. **macOS has no such facility** — there that case still
//!   leaves an orphan, which is why the shim also restores the modes itself on
//!   the paths it does run (`Terminal::restore`).
//!
//! * **The client dies first** (SIGKILL, OOM). Nobody is left to put the modes
//!   back, so the shim remembers what they were before the client ran and
//!   restores that on the way out.
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
        Self { saved: live_termios() }
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

/// Arms the client to take a SIGTERM when its parent thread goes away.
///
/// Linux only, because the facility is: the kernel delivers the signal for
/// deaths no process can catch (SIGKILL, OOM, `std::process::exit`), which is
/// exactly what `forward_death_signals` cannot reach. Elsewhere this is a no-op
/// and the orphan case stays open — see the module docs.
#[cfg(target_os = "linux")]
pub fn die_with_parent(command: &mut std::process::Command) {
    // SAFETY: the closure runs between fork and exec. It only makes libc calls
    // and loads integers — no allocation, no locks, nothing that could deadlock
    // in a single-threaded child.
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

/// No parent-death signal here; catchable deaths still go through forwarding.
#[cfg(not(target_os = "linux"))]
pub fn die_with_parent(_command: &mut std::process::Command) {}

/// Passes a terminating signal aimed at us on to the client.
///
/// The client is waiting in `main` as a child process; when it gets this signal
/// it restores the terminal and exits, and the shim's own exit path follows.
///
/// Returns the task and a receiver that fires once a signal has actually been
/// forwarded. The shim waits for the client indefinitely in normal use, so the
/// "give up on a wedged client" timer must only start from that moment.
pub fn forward_death_signals(
    client_pid: u32,
) -> (tokio::task::JoinHandle<()>, tokio::sync::oneshot::Receiver<()>) {
    let (forwarded, notify) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};

        let mut caught = Vec::new();
        for (name, kind) in [
            ("TERM", SignalKind::terminate()),
            ("INT", SignalKind::interrupt()),
            ("HUP", SignalKind::hangup()),
            ("QUIT", SignalKind::quit()),
        ] {
            match signal(kind) {
                Ok(mut stream) => {
                    caught.push(Box::pin(async move {
                        stream.recv().await;
                        name
                    }) as std::pin::Pin<Box<dyn std::future::Future<Output = &'static str> + Send>>);
                }
                // One that will not install is not fatal: the others still work.
                Err(err) => crate::log_line(&format!("terminal: {name} 处理器装不上：{err}")),
            }
        }
        if caught.is_empty() {
            return;
        }
        // Whichever fires first is the one that counts.
        let name = futures::future::select_all(caught).await.0;
        crate::log_line(&format!("terminal: 收到 SIG{name}，转给客户端 {client_pid}"));
        // SAFETY: `kill` only reads its arguments; a pid that has gone away is a
        // harmless ESRCH.
        unsafe {
            libc::kill(client_pid as libc::pid_t, libc::SIGTERM);
        }
        let _ = forwarded.send(());
    });
    (task, notify)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child armed for its parent's death must go when the spawning thread
    /// ends — the process stays up, so the only thing that can stop it is the
    /// kernel-delivered signal.
    ///
    /// Linux-only in practice: `die_with_parent` is a no-op elsewhere, and there
    /// is nothing to assert about a documented gap.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_takes_sigterm_when_its_spawning_thread_ends() {
        use std::time::{Duration, Instant};

        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            die_with_parent(&mut command);
            let child = command.spawn().expect("spawn sleep");
            tx.send(child.id()).expect("hand the pid over");
            drop(child);
        });

        let pid = rx.recv_timeout(Duration::from_secs(5)).expect("pid");
        thread.join().expect("spawning thread ends");

        let deadline = Instant::now() + Duration::from_secs(5);
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
            assert!(Instant::now() < deadline, "child {pid} survived its spawning thread");
            std::thread::sleep(Duration::from_millis(25));
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

    /// The catchable half: a signal aimed at the shim must reach the client, or
    /// the client outlives it holding the terminal.
    ///
    /// The client here is a `sleep`; it stops only if the forwarded SIGTERM
    /// arrives. `sleep` exiting with the signal is what the assertion reads.
    #[tokio::test]
    async fn a_signal_aimed_at_us_is_forwarded_to_the_client() {
        use std::time::Duration;

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let (forwarder, forwarded) = forward_death_signals(child.id());
        // Give the handlers a moment to install before the signal can arrive.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // SAFETY: signal delivery to our own process; the handler above takes it.
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        // The forwarder reports it actually sent the signal on to the client.
        tokio::time::timeout(Duration::from_secs(2), forwarded)
            .await
            .expect("the forwarder should report a forwarded signal")
            .expect("forwarder should send");

        let waited = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::task::spawn_blocking(move || child.wait()).await
        })
        .await
        .expect("client should exit once the signal is forwarded")
        .expect("wait thread")
        .expect("wait");

        forwarder.abort();
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(waited.signal(), Some(libc::SIGTERM), "client should have taken the forwarded signal");
        }
    }
}
