//! The terminal in the bottom-right corner: a real PTY, running on the machine.
//!
//! # Why this uses a PTY and the session channel deliberately does not
//!
//! The two want opposite things:
//!
//! * A **session** wants **structure** — which stretch is a tool call, which is model prose; an
//!   approval has to render as two buttons on a phone; a turn has to be cut into a commit. A byte
//!   stream expresses none of that, so it goes over a structured protocol (see
//!   [`crate::rc::harness`]).
//! * A **terminal** wants exactly that layer of **rendered byte stream**. `vim`, `top`, colored
//!   build output, any program that checks `isatty()` — none of them work without a PTY. What
//!   comes out here does not enter version control either — it is not "the agent's work record",
//!   it is a person typing by hand.
//!
//! So the two channels stay apart: one WebSocket, different verbs, no interference.
//!
//! # Boundary
//!
//! The terminal's cwd must land inside a project bound to the workspace, under the same allowlist
//! the agent uses. This is not a formality — a terminal that can `cd /` hands over the whole
//! machine, and this panel gets shared with other people. Who may open a terminal is the hub's
//! call (owner only, see `features/rc/routes.rs`).

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use tokio::sync::mpsc;

/// A live terminal.
pub struct Terminal {
    pub id: String,
    pub cwd: String,
    pub shell: String,
    /// Keystrokes go through this queue, and a **dedicated thread** does the actual write into
    /// the PTY. The queue is **bounded**.
    ///
    /// The write does not happen at the call site: portable-pty's write is **blocking**, and the
    /// PTY input buffer is tiny (a few kilobytes). A program that does not read stdin (`vim`
    /// busy, a build running, or a process simply stuck) fills it, and that `write_all` stops
    /// there — while the place calling it is the daemon's dispatch, holding the global lock on a
    /// tokio worker. One person pasting a stretch of text into a terminal that is not reading
    /// input stops every RPC on the machine.
    /// Bounded is the half that matters: the writer thread stays parked on that `write_all` for
    /// as long as the foreground program does not read stdin, so an unbounded queue trades
    /// "blocking the caller" for "unbounded heap" — every input copies a whole string, returns
    /// success immediately, and then keeps it forever. A few large pastes get there.
    ///
    /// A full queue says so honestly: a terminal is a stream, and "cannot take input" is a state
    /// terminals have.
    input: std::sync::mpsc::SyncSender<String>,
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    terminator: Arc<Terminator>,
}

#[derive(Debug, Clone, Copy)]
enum ChildPhase {
    Running,
    Terminating,
    MayReap,
    Reaped(Option<i32>),
}

const REAP_POLL_INITIAL: std::time::Duration = std::time::Duration::from_millis(10);
const REAP_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(1);

/// How many bytes one read from the pty takes.
///
/// It is also the **chunk boundary**: a multi-byte character gets cut on it, so the read loop
/// holds back the not-yet-complete character at the end (see `trailing_incomplete`).
const READ_CHUNK_BYTES: usize = 8192;

#[derive(Clone)]
struct ChildLifecycle {
    state: Arc<(Mutex<ChildPhase>, Condvar)>,
    #[cfg(test)]
    waiter_waitid_calls: Arc<AtomicUsize>,
}

impl ChildLifecycle {
    fn new() -> Self {
        Self {
            state: Arc::new((Mutex::new(ChildPhase::Running), Condvar::new())),
            #[cfg(test)]
            waiter_waitid_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn finish(&self, code: Option<i32>) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        *state = ChildPhase::Reaped(code);
        wake.notify_all();
    }

    /// Claim the right to signal the still-unreaped PID. The Unix waiter holds
    /// the same state lock across `wait()` + `Reaped`, so either this wins and
    /// the PID stays anchored until the final signal, or wait wins and this
    /// observes `Reaped` without ever touching a reusable number.
    fn begin_termination(&self) -> bool {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, ChildPhase::Running) {
            *state = ChildPhase::Terminating;
            // The idle reaper may be in its one-second steady-state wait. EOF
            // and an explicit close must not inherit that polling latency.
            wake.notify_all();
            true
        } else {
            false
        }
    }

    fn allow_reap(&self) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*state, ChildPhase::Terminating) {
            *state = ChildPhase::MayReap;
            wake.notify_all();
        }
    }

    fn wait_code(&self) -> Option<i32> {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let ChildPhase::Reaped(code) = *state {
                return code;
            }
            state = wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(test)]
    fn waiter_waitid_calls(&self) -> usize {
        self.waiter_waitid_calls.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub(crate) struct TerminalCleanup(Arc<(Mutex<bool>, Condvar)>);

impl TerminalCleanup {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), Condvar::new())))
    }

    fn finish(&self) {
        let (lock, wake) = &*self.0;
        let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
        *done = true;
        wake.notify_all();
    }

    pub(crate) fn wait_timeout(&self, timeout: std::time::Duration) -> bool {
        let (lock, wake) = &*self.0;
        let done = lock.lock().unwrap_or_else(|e| e.into_inner());
        if *done {
            return true;
        }
        let (done, _) = wake
            .wait_timeout_while(done, timeout, |done| !*done)
            .unwrap_or_else(|e| e.into_inner());
        *done
    }
}

struct Terminator {
    started: AtomicBool,
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    child_pid: Option<u32>,
    lifecycle: ChildLifecycle,
    finished: TerminalCleanup,
    #[cfg(test)]
    forced_kill: Arc<AtomicBool>,
}

impl Terminator {
    fn start(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        if !self.lifecycle.begin_termination() {
            // Natural exit won the state lock and published Reaped first.
            self.finished.finish();
            return;
        }
        let killer = self.killer.clone();
        let child_pid = self.child_pid;
        let lifecycle = self.lifecycle.clone();
        let finished = self.finished.clone();
        #[cfg(test)]
        let forced_kill = self.forced_kill.clone();
        std::thread::spawn(move || {
            let _forced = terminate_direct_child(killer, child_pid, &lifecycle);
            #[cfg(test)]
            forced_kill.store(_forced, Ordering::Release);
            finished.finish();
        });
    }
}

impl Drop for Terminator {
    fn drop(&mut self) {
        // Covers the narrow construction-error path too. All fallible PTY
        // handle setup is done before spawn, but a future early return must not
        // silently restore the old "drop Child without kill/wait" behavior.
        self.start();
    }
}

#[cfg(unix)]
fn child_exited_without_reaping(pid: u32) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    rc == 0 && unsafe { info.assume_init().si_pid() } == pid as i32
}

fn exit_code(status: portable_pty::ExitStatus) -> Option<i32> {
    if status.signal().is_some() {
        None
    } else {
        i32::try_from(status.exit_code()).ok()
    }
}

#[cfg(unix)]
fn reap_unix_child(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: u32,
    lifecycle: &ChildLifecycle,
) {
    let mut poll = REAP_POLL_INITIAL;
    loop {
        let (lock, wake) = &*lifecycle.state;
        let mut phase = lock.lock().unwrap_or_else(|e| e.into_inner());
        match *phase {
            ChildPhase::Running => {
                #[cfg(test)]
                lifecycle
                    .waiter_waitid_calls
                    .fetch_add(1, Ordering::Relaxed);
                if child_exited_without_reaping(pid) {
                    // waitid proved this cannot block. Keep the state lock until
                    // the code is published so a concurrent kill can never
                    // observe an unguarded, already-reusable PID.
                    let code = child.wait().ok().and_then(exit_code);
                    *phase = ChildPhase::Reaped(code);
                    wake.notify_all();
                    return;
                }

                let (_phase, elapsed) = wake
                    .wait_timeout(phase, poll)
                    .unwrap_or_else(|e| e.into_inner());
                if elapsed.timed_out() {
                    poll = poll.saturating_mul(2).min(REAP_POLL_MAX);
                }
            }
            ChildPhase::MayReap => {
                drop(phase);
                lifecycle.finish(child.wait().ok().and_then(exit_code));
                return;
            }
            ChildPhase::Reaped(_) => return,
            ChildPhase::Terminating => {
                // Termination is event-driven: allow_reap notifies after the
                // graceful signal / possible SIGKILL. Do not keep issuing
                // waitid while another thread owns the PID.
                let _phase = wake
                    .wait_while(phase, |phase| matches!(*phase, ChildPhase::Terminating))
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

#[cfg(unix)]
fn terminate_direct_child(
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    child_pid: Option<u32>,
    lifecycle: &ChildLifecycle,
) -> bool {
    // portable-pty's split killer sends HUP on Unix. Unlike its Child::kill,
    // it cannot wait/escalate, so the terminator owns that bounded escalation.
    if let Ok(mut killer) = killer.lock() {
        let _ = killer.kill();
    }
    let mut forced = false;
    if let Some(pid) = child_pid {
        let grace = std::time::Instant::now() + std::time::Duration::from_millis(250);
        while !child_exited_without_reaping(pid) && std::time::Instant::now() < grace {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if !child_exited_without_reaping(pid) {
            // The lifecycle remains Terminating, so the only waiter is forbidden
            // from reaping until after this syscall. The numeric PID therefore
            // cannot be reused underneath the SIGKILL.
            forced = unsafe { libc::kill(pid as i32, libc::SIGKILL) == 0 };
        }
    }
    lifecycle.allow_reap();
    // `finished` must mean the direct child was actually reaped, not merely
    // that an inner grace period elapsed. Daemon shutdown owns its own bounded
    // outer wait, so an exotic uninterruptible child is reported as timeout
    // instead of a false cleanup success.
    let _ = lifecycle.wait_code();
    forced
}

#[cfg(not(unix))]
fn terminate_direct_child(
    killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
    _child_pid: Option<u32>,
    lifecycle: &ChildLifecycle,
) -> bool {
    if let Ok(mut killer) = killer.lock() {
        let _ = killer.kill();
    }
    lifecycle.allow_reap();
    let _ = lifecycle.wait_code();
    false
}

/// What a terminal emits.
pub enum TerminalEvent {
    Output {
        id: String,
        /// Which workspace this terminal belongs to.
        ///
        /// **It rides on the event; it is not looked up on the daemon side.** The byte backflow
        /// is one pump task shared across every terminal (exactly one, so all terminals share a
        /// single backflow path), and it cannot reach `Daemon::terminals`. Hard-coding a literal
        /// into the stream id (`format!("term:{}", "workspace")`) puts every terminal of every
        /// workspace on the machine onto one stream, the hub resolves its workspace to an empty
        /// string, and the bytes fan out to a channel nobody subscribes to: the terminal panel in
        /// the web interface is black from start to finish.
        workspace_id: String,
        data: String,
    },
    Exited {
        id: String,
        workspace_id: String,
        code: Option<i32>,
    },
}

/// How many trailing bytes belong to a multi-byte character that is **not yet complete** (0..=3).
///
/// A UTF-8 lead byte carries its own length: `110xxxxx` needs two, `1110xxxx` needs three,
/// `11110xxx` needs four. Scan back from the end to the nearest lead byte; if the length it
/// claims is more than the bytes left after it, the read cut that character — hold it for the
/// next chunk.
fn trailing_incomplete(buf: &[u8]) -> usize {
    for back in 1..=3.min(buf.len()) {
        let b = buf[buf.len() - back];
        // A continuation byte (10xxxxxx) is not a lead byte; keep scanning back.
        if b & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let need = if b & 0b1000_0000 == 0 {
            1
        } else if b & 0b1110_0000 == 0b1100_0000 {
            2
        } else if b & 0b1111_0000 == 0b1110_0000 {
            3
        } else if b & 0b1111_1000 == 0b1111_0000 {
            4
        } else {
            // Not a valid lead byte at all: bad data, left to lossy to become a
            // replacement character.
            return 0;
        };
        return if need > back { back } else { 0 };
    }
    0
}

impl Terminal {
    /// Open a login shell in `cwd`.
    ///
    /// portable-pty reads and writes are both **blocking**, so the read loop goes on a dedicated
    /// `spawn_blocking` thread and hands the bytes back to the async side over a channel. A
    /// blocking read on a tokio worker drags the whole runtime down.
    pub fn open(
        id: String,
        workspace_id: String,
        cwd: &Path,
        cols: u16,
        rows: u16,
        out: mpsc::Sender<TerminalEvent>,
    ) -> crate::Result<Terminal> {
        let sys = native_pty_system();
        let pair = sys
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("cannot allocate a pty: {e}"))?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(cwd);
        // Tell the shell it is inside a terminal, and mark it as opened by agit — a user who
        // wants to tell the difference in their own rc file has something to test.
        cmd.env("TERM", "xterm-256color");
        cmd.env("AGIT_RC_TERMINAL", "1");

        // All fallible master handle setup happens **before** spawn. Otherwise, when resource
        // exhaustion makes take/clone fail, the shell is already alive with no Terminal/Drop to
        // collect it.
        let mut writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow::anyhow!("pty writer unavailable: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow::anyhow!("pty reader unavailable: {e}"))?;

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("cannot start {shell}: {e}"))?;
        let child_pid = child.process_id();
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let lifecycle = ChildLifecycle::new();
        let finished = TerminalCleanup::new();
        let terminator = Arc::new(Terminator {
            started: AtomicBool::new(false),
            killer,
            child_pid,
            lifecycle: lifecycle.clone(),
            finished,
            #[cfg(test)]
            forced_kill: Arc::new(AtomicBool::new(false)),
        });
        // **wait has exactly one owner.** reader EOF, close, daemon shutdown and Drop may race
        // freely; they only read the latch / signal. The dedicated thread always reaps the direct
        // child. A natural `exit` therefore never becomes a zombie, and the force-kill path never
        // misses the wait after the kill.
        let waiter_lifecycle = lifecycle.clone();
        std::thread::spawn(move || {
            #[cfg(unix)]
            if let Some(pid) = child_pid {
                reap_unix_child(child, pid, &waiter_lifecycle);
                return;
            }
            waiter_lifecycle.finish(child.wait().ok().and_then(exit_code));
        });
        // The slave must be dropped after spawn, or the read side gets no EOF when the child
        // exits.
        drop(pair.slave);

        // Writes get a dedicated thread for the same reason reads do: portable-pty's write
        // blocks. 64 chunks: a normal paste is far below that, and in front of a program that
        // does not read stdin that is all the memory it holds.
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<String>(64);
        std::thread::spawn(move || {
            while let Ok(data) = input_rx.recv() {
                if writer.write_all(data.as_bytes()).is_err() || writer.flush().is_err() {
                    break;
                }
            }
        });
        let rid = id.clone();
        let rws = workspace_id.clone();
        let reader_lifecycle = lifecycle;
        let reader_terminator = terminator.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; READ_CHUNK_BYTES];
            // The **not-yet-complete character** at the end of the previous chunk.
            //
            // With `from_utf8_lossy` per chunk, a multi-byte character straddling two chunks is
            // cut in half and each half becomes a U+FFFD — not a transient misalignment, but
            // something written permanently into the byte stream sent to the web interface. The
            // `READ_CHUNK_BYTES` boundary landing in the middle of a Chinese character is not
            // unlikely; `ls` on a Chinese-named directory hits it.
            let mut carry: Vec<u8> = vec![];
            loop {
                match reader.read(&mut buf) {
                    // EOF or a read error: **hand over the held tail** before leaving.
                    // Breaking straight out means the last straddling character (and
                    // whatever precedes it in the same chunk) never reaches the web
                    // interface — the screen is missing its last stretch.
                    Ok(0) | Err(_) => {
                        if !carry.is_empty() {
                            let data = String::from_utf8_lossy(&carry).to_string();
                            let _ = out.blocking_send(TerminalEvent::Output {
                                id: rid.clone(),
                                workspace_id: rws.clone(),
                                data,
                            });
                        }
                        break;
                    }
                    Ok(n) => {
                        carry.extend_from_slice(&buf[..n]);
                        // Hold back only the **not-yet-complete character at the end**;
                        // everything else goes out.
                        //
                        // The test cannot be "is the whole chunk valid UTF-8": with a
                        // genuinely bad byte in the middle of a chunk, `valid_up_to()`
                        // stops there and everything after it (including the characters
                        // that are already complete) waits for the next chunk — which
                        // starts with that same bad byte, so it waits forever. "Lossy the
                        // whole chunk" instead cuts the not-yet-complete character at the
                        // end in half.
                        //
                        // So look back at most three bytes and ask "is the end a multi-byte
                        // character that has just started". If so, hold it; the rest goes
                        // through lossy as usual — a program running in the terminal may
                        // emit arbitrary bytes, and the ones that should become replacement
                        // characters should.
                        let hold = trailing_incomplete(&carry);
                        let good = carry.len() - hold;
                        if good == 0 {
                            continue;
                        }
                        let data = String::from_utf8_lossy(&carry[..good]).to_string();
                        carry.drain(..good);
                        if out
                            .blocking_send(TerminalEvent::Output {
                                id: rid.clone(),
                                workspace_id: rws.clone(),
                                data,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            // EOF goes through the same idempotent cleanup: if the shell already exited on its
            // own, this anchors the not-yet-waited PID until the single waiter collects the
            // direct child; when an explicit close/Drop started first, this does nothing.
            reader_terminator.start();
            // Only once the output and the incomplete UTF-8 tail are queued does this wait for
            // the reaper's real final state. reader is the only sender of Exited, which
            // guarantees it never overtakes the last Output chunk.
            let code = reader_lifecycle.wait_code();
            let _ = out.blocking_send(TerminalEvent::Exited {
                id: rid,
                workspace_id: rws,
                code,
            });
        });

        Ok(Terminal {
            id,
            cwd: cwd.to_string_lossy().to_string(),
            shell,
            input: input_tx,
            master: Arc::new(Mutex::new(pair.master)),
            terminator,
        })
    }

    /// Queue this input. **This does not wait for the actual write into the PTY.**
    ///
    /// "queued" and "the shell read it" are two different things: a terminal is a stream, and
    /// there is no per-chunk receipt. To know whether it arrived, look at the bytes flowing back
    /// — that is a terminal's real receipt.
    pub fn write(&self, data: &str) -> crate::Result<()> {
        use std::sync::mpsc::TrySendError;
        self.input.try_send(data.to_string()).map_err(|e| match e {
            TrySendError::Full(_) => anyhow::anyhow!(
                "that terminal is not reading input right now (the program in it is busy); try again in a moment"
            ),
            TrySendError::Disconnected(_) => anyhow::anyhow!("that terminal has exited"),
        })?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> crate::Result<()> {
        let m = self
            .master
            .lock()
            .map_err(|_| anyhow::anyhow!("pty poisoned"))?;
        m.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| anyhow::anyhow!("resize failed: {e}"))
    }

    pub fn kill(&self) {
        self.terminator.start();
    }

    pub(crate) fn cleanup_handle(&self) -> TerminalCleanup {
        self.terminator.finished.clone()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_output(rx: &mut mpsc::Receiver<TerminalEvent>, needle: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = String::new();
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Some(TerminalEvent::Output { data, .. })) => {
                    seen.push_str(&data);
                    if seen.contains(needle) {
                        return;
                    }
                }
                Ok(Some(TerminalEvent::Exited { code, .. })) => {
                    panic!("terminal exited with {code:?} before {needle:?}: {seen:?}");
                }
                Ok(None) => panic!("terminal event stream closed before {needle:?}: {seen:?}"),
                Err(_) => continue,
            }
        }
        panic!("terminal never printed {needle:?}: {seen:?}");
    }

    async fn wait_for_exit(rx: &mut mpsc::Receiver<TerminalEvent>) -> Option<i32> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Some(TerminalEvent::Exited { code, .. })) => return code,
                Ok(Some(TerminalEvent::Output { .. })) | Err(_) => continue,
                Ok(None) => panic!("terminal event stream closed before exit"),
            }
        }
        panic!("terminal never reported exit");
    }

    #[cfg(unix)]
    fn assert_already_reaped(pid: u32) {
        let mut status = 0;
        let got = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        assert_eq!(
            got, -1,
            "test thread reaped child {pid}; the reaper did not"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "waitpid failed for a reason other than already-reaped"
        );
    }

    /// A shell that exits on its own must still be waited by the single reaper; reader picks up
    /// the real exit code after the last output chunk and only then sends Exited. A wrong
    /// implementation escapes this by reporting `code: None` and leaving a zombie that this
    /// waitpid can then reap by hand.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_naturally_exited_terminal_is_reaped_and_reports_its_code() {
        let (tx, mut rx) = mpsc::channel(64);
        let dir = std::env::temp_dir();
        let t =
            Terminal::open("reap-natural".into(), "ws-1".into(), &dir, 80, 24, tx).expect("pty");
        let pid = t.terminator.child_pid.expect("child pid");
        t.write("exit 23\n").expect("exit command");

        assert_eq!(wait_for_exit(&mut rx).await, Some(23));
        assert_already_reaped(pid);
    }

    /// close must cover a shell that ignores HUP/TERM: kill the direct child after the grace
    /// period, the dedicated waiter still owns the wait, and the thread calling into the daemon
    /// makes no blocking syscall anywhere along the way.
    ///
    /// The marker deliberately never appears contiguously in the input. A PTY echoes the whole
    /// command by default, so taking the `READY` inside the command as proof that the shell has
    /// installed its trap lets close send HUP before the trap takes effect, bypassing the very
    /// grace-period / SIGKILL branch this is meant to cover.
    #[cfg(unix)]
    #[tokio::test]
    async fn killing_a_term_resistant_terminal_still_reaps_it() {
        let (tx, mut rx) = mpsc::channel(64);
        let dir = std::env::temp_dir();
        let t = Terminal::open("reap-kill".into(), "ws-1".into(), &dir, 80, 24, tx).expect("pty");
        let pid = t.terminator.child_pid.expect("child pid");
        let cleanup = t.cleanup_handle();
        let forced_kill = t.terminator.forced_kill.clone();
        let marker = "FORCE_KILL_READY";
        let command = concat!(
            "stty -echo; trap '' HUP TERM; ",
            "printf '%s%s%s\\n' 'FORCE_' 'KILL_' 'READY'; ",
            "while :; do sleep 1; done\n"
        );
        assert!(
            !command.contains(marker),
            "PTY echo must not be able to forge the readiness marker"
        );
        t.write(command).expect("resistant loop");
        wait_for_output(&mut rx, marker).await;

        let started = std::time::Instant::now();
        t.kill();
        assert_eq!(wait_for_exit(&mut rx).await, None);
        assert!(
            tokio::task::spawn_blocking(move || {
                cleanup.wait_timeout(std::time::Duration::from_secs(2))
            })
            .await
            .expect("cleanup waiter"),
            "terminator did not finish"
        );
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(200),
            "term-resistant shell exited before the graceful-kill window elapsed"
        );
        assert!(
            forced_kill.load(Ordering::Acquire),
            "test never reached the SIGKILL escalation"
        );
        assert_already_reaped(pid);
    }

    /// An idle terminal's reaper must not call waitid at the `REAP_POLL_INITIAL` rate forever:
    /// without backoff the syscall volume is "number of terminals / `REAP_POLL_INITIAL`", which
    /// grows linearly with the number of terminals. Once backoff reaches its steady state, each
    /// terminal issues at most about two per second (the sampling window may straddle two timer
    /// boundaries). EOF / close wake through the Condvar and are not held back by the
    /// `REAP_POLL_MAX` cap.
    #[cfg(unix)]
    #[tokio::test]
    async fn many_idle_terminals_have_a_bounded_reaper_poll_rate() {
        const IDLE_TERMINALS: usize = 32;
        let (tx, _rx) = mpsc::channel(4096);
        let dir = std::env::temp_dir();
        let mut terminals = Vec::with_capacity(IDLE_TERMINALS);
        for i in 0..IDLE_TERMINALS {
            terminals.push(
                Terminal::open(format!("idle-{i}"), "ws-1".into(), &dir, 80, 24, tx.clone())
                    .expect("idle pty"),
            );
        }

        // Enough probes to have doubled REAP_POLL_INITIAL all the way up to
        // REAP_POLL_MAX and entered the steady-state wait. Waiting on the
        // counter instead of sleeping a guessed warm-up keeps this an
        // upper-bound test even on a heavily loaded runner.
        let warm_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while terminals
            .iter()
            .any(|terminal| terminal.terminator.lifecycle.waiter_waitid_calls() < 8)
        {
            assert!(
                tokio::time::Instant::now() < warm_deadline,
                "idle terminal reapers did not reach steady state"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let before: usize = terminals
            .iter()
            .map(|terminal| terminal.terminator.lifecycle.waiter_waitid_calls())
            .sum();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let after: usize = terminals
            .iter()
            .map(|terminal| terminal.terminator.lifecycle.waiter_waitid_calls())
            .sum();
        let calls = after - before;
        assert!(
            calls <= IDLE_TERMINALS * 2,
            "{IDLE_TERMINALS} idle terminals issued {calls} waitid calls in 1.1s"
        );

        let cleanups: Vec<_> = terminals.iter().map(Terminal::cleanup_handle).collect();
        for terminal in &terminals {
            terminal.kill();
        }
        assert!(
            tokio::task::spawn_blocking(move || cleanups
                .into_iter()
                .all(|cleanup| { cleanup.wait_timeout(std::time::Duration::from_secs(3)) }))
            .await
            .expect("cleanup waiter"),
            "an idle terminal was not reaped promptly after close"
        );
    }

    /// Removing a Terminal from the map does not depend on every call site remembering to kill
    /// it; Drop goes through the same idempotent terminator.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_terminal_triggers_the_same_reaper() {
        let (tx, mut rx) = mpsc::channel(64);
        let dir = std::env::temp_dir();
        let t = Terminal::open("reap-drop".into(), "ws-1".into(), &dir, 80, 24, tx).expect("pty");
        let pid = t.terminator.child_pid.expect("child pid");
        let cleanup = t.cleanup_handle();
        t.write("echo READY; while :; do sleep 1; done\n")
            .expect("running shell");
        wait_for_output(&mut rx, "READY").await;

        drop(t);
        assert!(
            tokio::task::spawn_blocking(move || {
                cleanup.wait_timeout(std::time::Duration::from_secs(2))
            })
            .await
            .expect("cleanup waiter"),
            "Drop did not finish terminal cleanup"
        );
        assert_already_reaped(pid);
    }

    /// A multi-byte character straddling two chunks must not be cut into two U+FFFD.
    ///
    /// The read buffer is `READ_CHUNK_BYTES`, and the boundary landing in the middle of a
    /// Chinese character is nothing rare — `ls` on a Chinese-named directory hits it. Lossy per
    /// chunk turns that character permanently into two replacement characters in the stream sent
    /// to the web interface.
    ///
    /// This uses `cat` on a file rather than `printf '\u4f60'`: `\u` is a shell-builtin dialect
    /// (zsh has it, the bash 3.2 that ships with macOS does not), and what `$SHELL` is on this
    /// machine is not this test's business. `cat` is the same everywhere.
    #[tokio::test]
    async fn a_character_split_across_two_reads_survives_whole() {
        let dir = tempfile::tempdir().expect("tmp");
        // UTF-8 fixture: far past a single `READ_CHUNK_BYTES` read, so a boundary is
        // guaranteed to land in the middle of a multi-byte character. ASCII makes this test
        // vacuous, so the payload stays CJK.
        let text = "你好世界".repeat(3000);
        std::fs::write(dir.path().join("zh.txt"), &text).expect("write");

        let (tx, mut rx) = mpsc::channel(256);
        let t = Terminal::open("t2".into(), "ws-1".into(), dir.path(), 80, 24, tx).expect("pty");
        // **Absolute path.** `Terminal::open` does set cwd to this temp directory, but what
        // it starts is the user's login shell, and one `cd ~` in an rc file overturns a relative
        // path — that failure shows up only on some people's machines, the hardest class of
        // failure to track down.
        t.write(&format!(
            "cat {}; exit\n",
            dir.path().join("zh.txt").display()
        ))
        .unwrap();

        let mut seen = String::new();
        // Only the **outer** deadline counts. An inner wait timing out means "not here yet",
        // not "never coming": with the whole suite running in parallel, starting an interactive
        // shell can take several seconds, and treating that timeout as the end lets this test
        // receive nothing under load — a test that is green only on an idle machine is worse
        // than no test.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some(TerminalEvent::Output { data, .. })) => {
                    seen.push_str(&data);
                    if seen.matches("你好世界").count() >= 3000 {
                        break;
                    }
                }
                // The shell exited: drain the bytes already queued on the channel before
                // finishing.
                Ok(Some(TerminalEvent::Exited { .. })) | Ok(None) => {
                    while let Ok(Some(TerminalEvent::Output { data, .. })) =
                        tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
                    {
                        seen.push_str(&data);
                    }
                    break;
                }
                Err(_) => continue,
            }
        }
        t.kill();
        assert!(
            seen.matches("你好世界").count() >= 2500,
            "only {} copies arrived; a multi-byte character was cut at a chunk boundary",
            seen.matches("你好世界").count()
        );
        assert!(
            !seen.contains('\u{fffd}'),
            "a replacement character means some character was cut in half"
        );
    }

    /// Only the not-yet-complete character at the end is held back; everything else goes out as
    /// usual.
    #[test]
    fn only_a_truncated_trailing_character_is_held_back() {
        // Complete: nothing is held. Every CJK literal below is a UTF-8 fixture: ASCII has no
        // multi-byte character to cut, so ASCII makes these cases vacuous.
        assert_eq!(trailing_incomplete("你好".as_bytes()), 0);
        assert_eq!(trailing_incomplete(b"abc"), 0);
        assert_eq!(trailing_incomplete(b""), 0);

        // `"你"` spans several bytes in UTF-8: with only its first byte, or its first two, the
        // tail is held.
        let ni = "你".as_bytes();
        assert_eq!(trailing_incomplete(&ni[..1]), 1);
        assert_eq!(trailing_incomplete(&ni[..2]), 2);
        assert_eq!(trailing_incomplete(ni), 0);

        // Content in front changes nothing; only the end matters.
        let mut buf = b"hello ".to_vec();
        buf.extend_from_slice(&ni[..2]);
        assert_eq!(trailing_incomplete(&buf), 2);

        // **A bad byte in the middle of a chunk must not hold back what follows it.** The test
        // looks only at the end, so the bad byte goes through lossy into a replacement character
        // instead of making the whole chunk wait for the next one.
        let mut bad = vec![0xff, 0xfe];
        bad.extend_from_slice("好".as_bytes());
        assert_eq!(trailing_incomplete(&bad), 0);

        // The end is not a valid lead byte at all: bad data, left to lossy, not held back.
        assert_eq!(trailing_incomplete(b"ok\xff"), 0);
    }

    /// Pouring input into a terminal that **does not read input** must not block the caller.
    ///
    /// This is the global-lock problem: `terminal.input` is called from the daemon's dispatch,
    /// holding the global lock on a tokio worker. The PTY input buffer is only a few kilobytes,
    /// and a program that does not read stdin fills it — a blocking write stops every RPC on the
    /// machine.
    #[tokio::test]
    async fn writing_into_a_terminal_that_never_reads_does_not_block_the_caller() {
        let (tx, _rx) = mpsc::channel(1);
        let dir = std::env::temp_dir();
        let t = Terminal::open("t3".into(), "ws-1".into(), &dir, 80, 24, tx).expect("pty");
        // Put the shell to sleep so it does not read stdin, then pour in far more than the PTY
        // buffer holds.
        t.write("sleep 30\n").unwrap();
        let big = "x".repeat(64 * 1024);
        let started = tokio::time::Instant::now();
        for _ in 0..8 {
            t.write(&big).unwrap();
        }
        let took = started.elapsed();
        t.kill();
        assert!(
            took < std::time::Duration::from_secs(2),
            "a write must not block the caller: {took:?} — this path holds the daemon's global lock"
        );
    }
    /// A PTY really can be allocated, and the shell really is inside a terminal (`test -t 0`
    /// holds only on a tty). This pins "why a PTY" itself — without one, any program that checks
    /// isatty takes the non-interactive branch.
    #[tokio::test]
    async fn a_terminal_really_is_a_tty() {
        let (tx, mut rx) = mpsc::channel(64);
        let dir = std::env::temp_dir();
        let t = Terminal::open("t1".into(), "ws-1".into(), &dir, 80, 24, tx).expect("pty");
        t.write("test -t 0 && echo IS_A_TTY; exit\n").unwrap();

        let mut seen = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await {
                Ok(Some(TerminalEvent::Output { data, .. })) => {
                    seen.push_str(&data);
                    if seen.contains("IS_A_TTY") {
                        break;
                    }
                }
                Ok(Some(TerminalEvent::Exited { .. })) | Ok(None) => break,
                Err(_) => break,
            }
        }
        t.kill();
        assert!(
            seen.contains("IS_A_TTY"),
            "the shell must be on a tty; otherwise the PTY did not take effect: {seen:?}"
        );
    }
}
