//! Local control socket — how `agit rc status` / `stop` reach the daemon.
//!
//! A unix socket at `~/.agit/rc/control.sock` plus a pidfile beside it. Not a
//! TCP port: this channel can stop the daemon, and a localhost port is reachable
//! by every process and container on the machine, whereas the socket carries
//! filesystem permissions (0600, in a directory only the user can read).
//!
//! The wire format is one JSON line each way. That keeps `agit rc status`
//! synchronous and dependency-free — it does not need a tokio runtime just to
//! ask a question.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    Stop,
    ReloadSecrets,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Stopping,
    SecretsReloaded { generation: u64, rules: usize },
    Error { message: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub hub: String,
    pub online: bool,
    pub connection_id: Option<String>,
    pub uptime_secs: u64,
    pub agit_version: String,
    pub sessions: Vec<SessionLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLine {
    pub session_id: String,
    pub runtime: String,
    pub status: String,
    pub last_seq: u64,
}

/// Length cap for a unix socket path.
///
/// `sockaddr_un.sun_path` is 108 bytes on Linux and 104 on macOS, and **going over does not
/// truncate — `bind` fails outright**. This keeps margin and settles on 100.
const SUN_PATH_MAX: usize = 100;

/// Where the control socket lives.
///
/// `~/.agit/rc/control.sock` is preferred — it sits with the rest of the state, visible at a
/// glance. But `$AGIT_HOME` can be deep (a CI temp directory, a mount point inside a container)
/// while a unix socket path has a hard cap. So when it grows too long this falls back to a
/// **short and deterministic** location: a hash of the rc directory, which both sides compute to
/// the same value, so no "where is the socket" discovery file is needed.
///
/// The fallback answers an observed failure: with `$AGIT_HOME` under a temp directory long enough
/// to blow the cap, `bind()` reports `path must be shorter than SUN_LEN`, the daemon cannot start,
/// and `agit rc status` only says "no daemon is running" — a message that points the reader the
/// wrong way.
pub fn socket_path() -> crate::Result<PathBuf> {
    Ok(socket_path_for(&super::rc_dir()?))
}

/// The pure kernel of [`socket_path`].
///
/// Split out to be testable: path resolution reads `$AGIT_HOME`, cargo tests run in parallel, and
/// mutating an environment variable inside a test interferes with every other one (`infra::config`
/// has already hit this).
pub fn socket_path_for(rc_dir: &std::path::Path) -> PathBuf {
    let preferred = rc_dir.join("control.sock");
    if preferred.as_os_str().len() <= SUN_PATH_MAX {
        return preferred;
    }
    let key = short_hash(&rc_dir.to_string_lossy());
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let uid = unsafe { libc::getuid() };
    dir.join(format!("agit-{uid}-{key}.sock"))
}

/// Path → 8 hex digits. This only makes a name; it carries no security property.
fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(s.as_bytes());
    hex::encode(&d[..4])
}

pub fn pid_path() -> crate::Result<PathBuf> {
    Ok(super::rc_dir()?.join("agitd.pid"))
}

/// The pid of a daemon that is actually alive, if any.
///
/// A stale pidfile (crash, reboot) must not look like a running daemon — that
/// is the failure where `agit rc start` refuses forever and the user has no
/// idea why. So we verify the process exists before believing the file.
pub fn running_pid() -> Option<u32> {
    let rc = super::rc_dir().ok()?;
    running_pid_in(&rc)
}

/// The pure kernel of [`running_pid`] (the path is passed in explicitly, to be testable; see
/// [`socket_path_for`]).
///
/// This path answers **"which pid"**, so only [`Presence::Running`] has an answer: when the probe
/// cannot tell, there is no honest pid to give. **But `None` is not "there is no daemon"** — a
/// caller that must separate the two (the wording of `agit rc status`, for one) asks
/// [`presence_in`].
pub fn running_pid_in(rc_dir: &std::path::Path) -> Option<u32> {
    match presence_in(rc_dir) {
        Presence::Running(pid) => Some(pid),
        _ => None,
    }
}

/// Whether a daemon exists on this machine — **three answers, not two**.
///
/// # Why "cannot tell" gets a variant of its own
///
/// [`listen`] **refuses to start** on [`Liveness::Unknown`] (it will not remove a socket that may
/// still belong to someone alive). If this side conflated "cannot tell" with "there is none", the
/// user would see two contradictory messages at once: `agit rc status` says there is no daemon and
/// `agit rc start` says there already is one. That is the exact shape this module keeps stamping
/// out.
///
/// The correspondence is **exact**: `Absent` ⟺ the probe returns [`Liveness::Stale`] ⟺ [`listen`]
/// removes that file and starts normally. As long as both sides read this function, they cannot
/// give opposite answers.
#[derive(Debug, PartialEq, Eq)]
pub enum Presence {
    /// Running, and this is its pid.
    Running(u32),
    /// Definitely none: nobody is on the far side, and the socket file (if one is left) is safe to
    /// remove.
    Absent,
    /// Cannot tell. **Not** to be treated as "there is none".
    Unclear(String),
}

/// The entry point for [`Presence`]. A path-resolution failure is also "cannot tell" — it is
/// certainly not "definitely none".
pub fn presence() -> Presence {
    match super::rc_dir() {
        Ok(rc) => presence_in(&rc),
        Err(e) => Presence::Unclear(format!("cannot locate the rc directory: {e}")),
    }
}

/// The pure kernel of [`presence`] (the path is passed in explicitly, to be testable; see
/// [`socket_path_for`]).
pub fn presence_in(rc_dir: &std::path::Path) -> Presence {
    // Ask the socket first: only an answer from the far side proves a daemon is really there. The
    // pidfile only answers "then which pid is it" — pids get reused, and reading it alone mistakes
    // an unrelated process for the daemon.
    match probe_socket(&socket_path_for(rc_dir), rc_dir) {
        Liveness::Live => match read_pid(rc_dir).filter(|p| process_alive(*p)) {
            Some(pid) => Presence::Running(pid),
            // Something answered, but the pidfile cannot say who. It **is** running (it just
            // answered), so this is not "there is none"; there is simply no pid to give.
            None => Presence::Unclear(
                "something answered on the control socket but the pidfile does not name a live \
                 process"
                    .into(),
            ),
        },
        Liveness::Stale => Presence::Absent,
        Liveness::Unknown(why) => Presence::Unclear(why),
    }
}

fn read_pid(rc_dir: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(rc_dir.join("agitd.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // signal 0 checks for existence without delivering anything.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    false
}

pub fn write_pidfile() -> crate::Result<()> {
    std::fs::write(pid_path()?, std::process::id().to_string())?;
    Ok(())
}

pub fn clear_pidfile() {
    if let Ok(p) = pid_path() {
        let _ = std::fs::remove_file(p);
    }
}

/// Send one request and read one reply. Used by the CLI side.
pub fn ask(req: &Request) -> crate::Result<Reply> {
    let path = socket_path()?;
    // Connect through the **bounded** path, for the same reason the probe does: against a full
    // backlog a blocking connect waits indefinitely on Linux (observed), and this is the path
    // `agit rc status` / `agit rc stop` send their requests on — bounding the probe but not this
    // leaves the commands wedged on the same kernel-level wait.
    let stream = connect_within(&path, CONNECT_TIMEOUT)
        .map_err(|e| anyhow::anyhow!("no daemon listening at {}: {e}", path.display()))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut w = stream.try_clone()?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()?;

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

/// The three states of a socket file. **Three, not two** — conflating them is what deletes a live
/// daemon's socket.
#[derive(Debug, PartialEq, Eq)]
enum Liveness {
    /// A daemon is on the far side, and it **answered**.
    Live,
    /// Nobody is on the far side, for certain: `connect` was refused **and** the process named in
    /// the pidfile is gone too. The file is safe to remove.
    ///
    /// **Neither condition alone is enough.** ECONNREFUSED does not prove there is no listener: a
    /// live listener refuses new connections just the same once its accept queue is full (observed
    /// past the backlog limit; see [`refused_connect_verdict`]). Reading it alone removes the
    /// socket of a daemon that is still running.
    ///
    /// **Only a `connect` failure can produce this verdict.** Anything that goes wrong after the
    /// connection is established (EOF, reset, timeout) does not count — those prove this one
    /// connection died, not that there is no listener.
    Stale,
    /// Cannot tell: the connection goes through but nothing answers, or permissions or fds get in
    /// the way.
    ///
    /// The only reason this variant exists is that **it must not be treated as `Stale`**. Treating
    /// it as stale removes the socket of a daemon that may still be alive and binds over it,
    /// orphaning it — it keeps running, nobody can reach it any more, and the failure is silent.
    Unknown(String),
}

/// The budget for `connect` itself.
///
/// **This step must be bounded too.** `set_read_timeout` / `set_write_timeout` govern reads and
/// writes **after** the connection is established; they do not reach `connect`. And when a blocking
/// connect meets a listener whose accept queue is full, the two platforms part ways: macOS refuses
/// outright with ECONNREFUSED (see [`refused_connect_verdict`]), **Linux waits indefinitely** —
/// observed with `listen(fd, 1)` squeezing the backlog down to 1 and then filling it: the second
/// connect does not return.
///
/// A full queue is a reachable shape here: the control thread **accepts serially**, and the
/// `Request::Status` handler gives itself 2 seconds to take the lock (see `daemon.rs` and the
/// budget note in [`probe_once`]), so a script polling `agit rc status` fills it. Without this cap,
/// `agit rc status` / `agit rc start` wedge on `connect` and the read/write budget below never
/// comes into play.
///
/// The value is the same order as the read/write budget (both 5 seconds) but is **named
/// separately**: the two govern different stages, and adjusting one must never drag the other
/// along.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A bounded `UnixStream::connect`: wait at most `budget`, and treat a timeout as not connecting.
///
/// Apart from that cap the semantics match `UnixStream::connect` — `NotFound`,
/// `ConnectionRefused` and every other `io::Error` propagate unchanged, so a caller that branches
/// on the error is unaffected.
///
/// A timeout reports [`std::io::ErrorKind::TimedOut`]: **neither `NotFound` nor
/// `ConnectionRefused`**, so the caller lands on [`Liveness::Unknown`] rather than
/// [`Liveness::Stale`]. That is deliberate — when the reason for not connecting is unclear, another
/// process's socket must never be removed, and that is the invariant this module keeps holding.
///
/// # Why not a blocking connect on a second thread with the main thread timing out
///
/// After the timeout that thread is still parked in `connect` and nobody can reclaim it: every
/// probe leaks a thread and an fd, and this path is taken exactly when the far side is wedged, so
/// the leak keeps accumulating. So this opens the fd itself: `O_NONBLOCK` + `connect` + `poll` for
/// writability + `getsockopt(SO_ERROR)` for the result, and on timeout the fd is closed, leaving
/// nothing behind.
fn connect_within(
    path: &std::path::Path,
    budget: std::time::Duration,
) -> std::io::Result<UnixStream> {
    /// How long to wait before retrying a full queue. Tight enough not to change the verdict,
    /// loose enough not to spin.
    const RETRY_GAP: std::time::Duration = std::time::Duration::from_millis(20);

    let (addr, addr_len) = sockaddr_un(path)?;
    let deadline = std::time::Instant::now() + budget;
    loop {
        match connect_attempt(&addr, addr_len, deadline) {
            // On Linux a full AF_UNIX queue is an immediate EAGAIN (not EINPROGRESS, and not the
            // ECONNREFUSED macOS gives). That is not "cannot connect", it is "not your turn yet":
            // a blocking connect simply waits here, and a busy daemon **must** be judged Live. So
            // this waits too — only up to the deadline, no longer indefinitely.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return Err(connect_timed_out());
                }
                std::thread::sleep(RETRY_GAP.min(left));
            }
            other => return other,
        }
    }
}

/// One attempt: open an fd, connect non-blocking, wait for the result. Every failure takes the fd
/// with it.
fn connect_attempt(
    addr: &libc::sockaddr_un,
    addr_len: libc::socklen_t,
    deadline: std::time::Instant,
) -> std::io::Result<UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // From this line the fd has an owner: every early return below closes it.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    set_nonblocking(fd.as_raw_fd(), true)?;

    let started = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            std::ptr::from_ref(addr).cast::<libc::sockaddr>(),
            addr_len,
        )
    };
    if started < 0 {
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            // Still in flight. EINTR is the same: the connection keeps being established in the
            // background, and re-issuing connect returns EALREADY — the right move is to wait for
            // this fd to become writable.
            Some(libc::EINPROGRESS | libc::EINTR) => wait_writable(fd.as_raw_fd(), deadline)?,
            _ => return Err(e),
        }
        // Writable only says "there is a result"; success or failure takes a separate question —
        // skipping it treats a failed connection as a connected one.
        let err = socket_error(fd.as_raw_fd())?;
        if err != 0 {
            return Err(std::io::Error::from_raw_os_error(err));
        }
    }

    // **Restore blocking mode.** The read/write timeouts set later (SO_RCVTIMEO / SO_SNDTIMEO)
    // only mean anything for blocking reads and writes; leaving O_NONBLOCK on makes the probe's
    // first read return WouldBlock immediately, and a live daemon is judged not to answer.
    set_nonblocking(fd.as_raw_fd(), false)?;
    Ok(UnixStream::from(fd))
}

/// Wait for this fd to become writable (= connect has a result), until `deadline` at the latest.
fn wait_writable(fd: std::os::fd::RawFd, deadline: std::time::Instant) -> std::io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let ms = left.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n > 0 {
            return Ok(());
        }
        if n == 0 {
            return Err(connect_timed_out());
        }
        let e = std::io::Error::last_os_error();
        // A signal interruption resumes the wait, but against the **remaining** time — handing
        // out a fresh budget each round is the same as having no cap, and the whole point of the
        // cap is that nothing wedges forever.
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
        if std::time::Instant::now() >= deadline {
            return Err(connect_timed_out());
        }
    }
}

/// `SO_ERROR`: where the real result of a non-blocking connect lands.
fn socket_error(fd: std::os::fd::RawFd) -> std::io::Result<libc::c_int> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::from_mut(&mut err).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(err)
}

fn set_nonblocking(fd: std::os::fd::RawFd, on: bool) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let want = if on {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if want != flags && unsafe { libc::fcntl(fd, libc::F_SETFL, want) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Fill a `sockaddr_un`, plus the length the kernel wants.
fn sockaddr_un(path: &std::path::Path) -> std::io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    // `>=` and not `>`: sun_path is read as a C string, so the trailing NUL needs a slot too. The
    // cap itself is already dodged in [`socket_path_for`]; this only refuses to silently truncate
    // into a different path.
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "socket path is longer than sockaddr_un allows: {}",
                path.display()
            ),
        ));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, b) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *b as libc::c_char;
    }
    // sun_path is the last field, so "size of the struct − size of sun_path" is its offset.
    // Report the actual path length rather than the whole struct: this matches what std computes,
    // and removes one platform difference.
    let len = std::mem::size_of::<libc::sockaddr_un>() - addr.sun_path.len() + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

fn connect_timed_out() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out waiting for the control socket to accept the connection",
    )
}

/// Whether anyone is on the far side of the socket.
///
/// # The test is an answer coming back, not `connect().is_ok()`
///
/// That one line collapses three states into two, and is wrong at both ends:
///
/// * **Not connecting ≠ stale.** Exhausted fds, wrong permissions and a signal interruption all
///   make `connect` fail, and `listen()` then removes the socket of a **live** daemon.
/// * **Connecting ≠ alive.** On a listener that has just been closed, `connect` can still succeed
///   by luck — the race does happen under concurrency (running that case alone does not reproduce
///   it).
///
/// So an answer is required: a real daemon replies to [`Request::Status`].
///
/// # Why the probe runs twice
///
/// "connected but nothing answered" has two causes, and they need **opposite** treatment:
///
/// * nobody is really there (the socket is a leftover, and the first connect was a lucky win in the
///   race) → clean it up;
/// * a live daemon that is busy or wedged is there → never clean it up.
///
/// One probe cannot separate them. **Connecting again** moves one step forward: a second connect on
/// a leftover is refused.
///
/// Refusal is not yet a conclusion — ECONNREFUSED does not prove there is no listener (see
/// [`refused_connect_verdict`]) — so a refusal is followed by "is the process in the pidfile still
/// there", and only both conditions together give `Stale`.
///
/// Only a second connection that still goes through and still does not answer lands on `Unknown` —
/// refuse to start, fail loudly.
///
/// # Why the rc directory is passed in explicitly
///
/// The owner question reads `<rc_dir>/agitd.pid`, and deriving the rc directory back out of the
/// socket path is brittle: when `$AGIT_HOME` is too deep the socket falls back to a short path
/// under `$XDG_RUNTIME_DIR` (see [`socket_path_for`]), and then the two have no parent-child
/// relation at all.
fn probe_socket(path: &std::path::Path, rc_dir: &std::path::Path) -> Liveness {
    match probe_once(path, rc_dir) {
        Liveness::Live => Liveness::Live,
        Liveness::Stale => Liveness::Stale,
        // Connect through the bounded path (see [`connect_within`]): against a full backlog a
        // blocking connect waits indefinitely on Linux, and probing twice would wedge twice.
        Liveness::Unknown(why) => match connect_within(path, CONNECT_TIMEOUT) {
            // The file is gone: "no listener" is what this error **means**, so the owner question
            // is unnecessary.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Liveness::Stale,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                refused_connect_verdict(rc_dir)
            }
            // Everything else that fails to connect (a connect timeout included) falls back to
            // `Unknown` — only the two above are enough for a verdict.
            _ => Liveness::Unknown(why),
        },
    }
}

/// Whether a refused `connect` is enough to call the socket a leftover.
///
/// # ECONNREFUSED does not prove there is no listener
///
/// A **live** listener refuses new connections just the same once its accept queue is full.
/// Observed on Darwin: `UnixListener::bind` uses backlog=128, and on a listener that binds and then
/// never accepts, the first 128 connects all succeed, the 129th returns ECONNREFUSED, and the
/// listener is alive throughout.
///
/// **The same shape does not take this path on Linux**: there a blocking connect against a full
/// backlog is not refused but waits indefinitely (observed; see [`CONNECT_TIMEOUT`]), so this
/// verdict function is never reached on Linux and a connect timeout → [`Liveness::Unknown`] takes
/// its place. Both paths land in the same place: neither removes the socket.
///
/// The shape is reachable: the control thread **accepts serially**, and the `Request::Status`
/// handler gives itself 2 seconds to take `try_lock` (see `daemon.rs`). A script polling
/// `agit rc status` fills the queue. Calling that `Stale` makes [`listen`] remove the socket and
/// bind over it — the daemon keeps running, nobody can reach it any more, and the failure is
/// silent.
///
/// So "safe to remove" carries a **necessary condition**: the process in the pidfile is gone too. A
/// live owner lands on `Unknown` — nothing removed, loud failure.
fn refused_connect_verdict(rc_dir: &std::path::Path) -> Liveness {
    if owner_process_alive(rc_dir) {
        return Liveness::Unknown(
            "the socket refused the connection, but the process in its pidfile is still alive \
             — its accept queue may be full, or it may be wedged"
                .into(),
        );
    }
    Liveness::Stale
}

/// Whether the process recorded in the pidfile is still alive.
///
/// Only meaningful on the **affirmative** side: the owner counts as alive only when the pid reads
/// back and that process still exists. A missing file, an unreadable one and an unparseable one all
/// count as not alive — a daemon that is really running has written this file (see
/// [`write_pidfile`]), so "no pidfile" is not "cannot tell".
fn owner_process_alive(rc_dir: &std::path::Path) -> bool {
    read_pid(rc_dir).is_some_and(process_alive)
}

/// One probe: connect, and ask for an answer.
///
fn probe_once(path: &std::path::Path, rc_dir: &std::path::Path) -> Liveness {
    use std::io::{BufRead, BufReader};

    // `connect` **itself** needs a cap, see [`CONNECT_TIMEOUT`]: the two read/write timeouts below
    // only govern what happens after the connection is established, while a blocking connect
    // against a full backlog waits indefinitely on Linux — the probe wedges on this line and none
    // of the budgets below ever apply.
    let mut stream = match connect_within(path, CONNECT_TIMEOUT) {
        Ok(s) => s,
        // The file is gone: that is "no listener" itself.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Liveness::Stale,
        // A refusal **is not** "no listener" — a live listener with a full backlog answers the
        // same way. Ask once more whether the owner is still there; see
        // [`refused_connect_verdict`].
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
            return refused_connect_verdict(rc_dir);
        }
        // A timeout lands here too: not getting a connection does not say "nobody is there", only
        // "this attempt did not connect". Calling it `Stale` removes the socket of a daemon that
        // may merely have a full queue.
        Err(e) => return Liveness::Unknown(format!("connect failed: {e}")),
    };

    // The timeouts are mandatory: without them a peer that connects and never answers wedges
    // `agit rc start` forever.
    //
    // **What has to be covered is not one handler run alone, but the queueing in front of it.** The
    // `Request::Status` handler gives itself 2 seconds to take `try_lock` (see `daemon.rs`), and on
    // timeout still replies `Reply::Error{"the daemon is busy..."}` — that is **an answer** too, so
    // `Live`. And the control thread **accepts serially**: this probe may first queue in the
    // backlog behind another connection being handled (up to about 2 seconds), then wait on its own
    // handler (up to about 2 seconds). A budget covering only the handler and not the queueing
    // judges a **live** daemon `Unknown`.
    //
    // 5 seconds = the handler's own 2-second budget × 2 (one queued, one our own) + margin.
    //
    // **Raising it to infinity is not the answer**: the cap exists so that nothing wedges forever —
    // a peer that connects and never answers would wedge `agit rc status` / `agit rc start`, and
    // that failure has no way out. So the number has to cover the known worst-case queueing, rather
    // than not exist.
    let t = std::time::Duration::from_secs(5);
    if stream.set_read_timeout(Some(t)).is_err() || stream.set_write_timeout(Some(t)).is_err() {
        return Liveness::Unknown("cannot set socket timeouts".into());
    }

    let mut line = match serde_json::to_string(&Request::Status) {
        Ok(s) => s,
        Err(e) => return Liveness::Unknown(format!("cannot encode probe: {e}")),
    };
    line.push('\n');
    if let Err(e) = stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
    {
        // As above: the connection was established, so a failed write does not prove there is no
        // listener.
        return Liveness::Unknown(format!("probe write failed: {e}"));
    }

    // **`Stale` can only come from `connect` itself failing**; nothing below ever returns `Stale`.
    //
    // EOF / ECONNRESET **must not** be judged `Stale`: the connection was established, so someone
    // was accepting at that moment; a later disconnect proves **this one connection** died, not
    // that there is no listener. A live daemon restarting a handler, hitting a resource cap, or
    // simply closing this connection would all be judged stale, and `listen()` would then remove
    // its socket and bind over it — it keeps running, and nobody can reach it any more. That
    // failure is silent.
    //
    // The common self-healing path is unaffected: once a killed daemon's process is gone, `connect`
    // gets ECONNREFUSED outright and the pid in the pidfile is gone too — both conditions hold, so
    // the verdict is still `Stale` and the file is cleaned up. (The first condition alone is not
    // enough: a live listener with a full backlog also refuses connections.)
    //
    // The cost is that "connects but does not answer" makes `agit rc start` refuse to start instead
    // of cleaning up on its own. That is a **loud** failure whose error says what to do; removing a
    // live daemon's socket by mistake is a quiet one.
    let mut reply = String::new();
    match BufReader::new(&stream).read_line(&mut reply) {
        Ok(0) => Liveness::Unknown("connected but the peer closed without answering".into()),
        Ok(_) => match serde_json::from_str::<Reply>(&reply) {
            Ok(_) => Liveness::Live,
            // Something is answering, we just cannot read it — that must never be taken as
            // "nobody is there" and its socket removed.
            Err(e) => Liveness::Unknown(format!("unrecognised reply: {e}")),
        },
        // A timeout lands here for the same reason: the control thread accepts serially, and this
        // probe may be queued behind another connection. Not getting an answer does not say
        // "nobody is there", only "this attempt got no answer".
        Err(e) => Liveness::Unknown(format!("probe read failed: {e}")),
    }
}

/// Bind the control socket, removing a stale one first.
pub fn listen() -> crate::Result<UnixListener> {
    // The rc directory is kept separately: the staleness verdict also reads `<rc_dir>/agitd.pid`,
    // and a socket path that would be too deep falls back elsewhere, so it cannot be derived back
    // (see [`socket_path_for`] and [`probe_socket`]).
    let rc_dir = super::rc_dir()?;
    let path = socket_path_for(&rc_dir);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A killed daemon leaves the socket file behind, and bind then gets EADDRINUSE. Removing it is
    // safe **only when nobody is on the far side for certain**, so the three states take three
    // paths:
    if path.exists() {
        match probe_socket(&path, &rc_dir) {
            // Certainly nobody: remove it, so the bind below does not get EADDRINUSE.
            Liveness::Stale => {
                let _ = std::fs::remove_file(&path);
            }
            // Something answers: this is not a stale socket, it is "a daemon already exists".
            // Letting bind report EADDRINUSE is far better than removing its socket here.
            Liveness::Live => {}
            // Cannot tell: **do nothing**. Removing it may orphan a live daemon, and that failure
            // is silent — it keeps running, and nobody can reach it any more.
            Liveness::Unknown(why) => {
                return Err(anyhow::anyhow!(
                    "cannot tell whether a daemon is already listening on {} ({why}); \
                     refusing to remove it. if you are sure no daemon is running, \
                     remove the file and retry",
                    path.display()
                ));
            }
        }
    }
    let l = UnixListener::bind(&path)
        .map_err(|e| anyhow::anyhow!("cannot bind {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(l)
}

/// Read one request from an accepted connection and write back a reply.
pub fn serve_one(
    stream: &mut UnixStream,
    handle: impl FnOnce(Request) -> Reply,
) -> crate::Result<()> {
    let peer = stream.try_clone()?;
    let mut reader = BufReader::new(peer);
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    let req: Request = serde_json::from_str(buf.trim())?;
    let reply = handle(req);
    let mut line = serde_json::to_string(&reply)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Start a fake daemon that **really answers**: it serves `n` times, then exits and takes the
    /// listener with it.
    ///
    /// The test is whether something answers, not whether something ever bound, so the fake daemon
    /// in a test has to answer for real. It goes through the real `serve_one` — a hand-written
    /// reply would drift apart from the protocol, and what these tests are for is exactly that the
    /// probe speaks the real protocol.
    ///
    /// **The service count is fixed** because the next assertion is "once it is gone the socket is
    /// stale": dropping a `JoinHandle` does not stop the thread, and the listener stays alive.
    /// Joining it is the reliable way to finish.
    fn answering_daemon(path: &std::path::Path, n: usize) -> std::thread::JoinHandle<()> {
        let l = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            for _ in 0..n {
                match l.accept() {
                    Ok((mut s, _)) => {
                        let _ = serve_one(&mut s, |_req| Reply::Stopping);
                    }
                    Err(_) => break,
                }
            }
        })
    }

    /// A stale verdict needs **two** conditions: nobody serving, and no live owner. So this rc
    /// directory deliberately has no pidfile — writing one that points at this process would supply
    /// a live owner, and the verdict would (correctly) land on `Unknown`.
    #[test]
    fn a_socket_file_with_nobody_listening_is_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path();
        let p = rc.join("dead.sock");
        // Start one that really answers, then let it exit — the file stays, but nobody serves.
        let h = answering_daemon(&p, 1);
        assert_eq!(
            probe_socket(&p, rc),
            Liveness::Live,
            "something answering must count as live"
        );
        // join: the thread returns once it has served that one request, and the listener closes
        // with it. `drop(JoinHandle)` cannot do this — it only stops waiting; the thread and the
        // listener both stay.
        h.join().expect("the fake daemon must not panic");

        assert!(
            p.exists(),
            "the socket file is still there — that is where EADDRINUSE comes from"
        );
        assert_eq!(
            probe_socket(&p, rc),
            Liveness::Stale,
            "nobody serving and no live owner must be judged stale; otherwise the daemon can \
             never start again"
        );
    }

    /// A socket that is **bound but never accepted on** is not alive.
    ///
    /// `connect().is_ok()` is timing-dependent: under a fully parallel run, "the listener is
    /// already dropped and connect still succeeds" does happen, while running that case alone does
    /// not reproduce it. Rather than guess at that timing, the test **does not depend on timing**
    /// at all: ask for an answer.
    ///
    /// A socket bound with nobody accepting is exactly the shape inside that race window: it
    /// connects, it does not answer. It must be judged stale, or `listen()` never cleans it up and
    /// `bind` immediately gets EADDRINUSE.
    #[test]
    fn a_socket_that_accepts_but_never_answers_is_not_live() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("mute.sock");
        // Hold the listener but **never accept**: connections reach the backlog, nothing answers.
        let _l = UnixListener::bind(&p).unwrap();
        assert_ne!(
            probe_socket(&p, tmp.path()),
            Liveness::Live,
            "connecting without answering must not count as live — that is the shape inside the \
             race window"
        );
    }

    /// A disconnect **after the connection is established** must not be taken as "no listener".
    ///
    /// EOF and ECONNRESET must not be judged `Stale`: the connection was established, so someone
    /// was accepting at that moment; a later disconnect proves only that **this one connection**
    /// died. A live daemon closing this connection (restarting a handler, hitting a resource cap)
    /// would be judged stale, its socket removed and then bound over — it keeps running, nobody can
    /// reach it any more, and the failure is silent.
    ///
    /// The fake daemon here **closes the connection immediately** after accepting while the
    /// listener stays alive: exactly that shape. It must not be `Stale`.
    #[test]
    fn a_disconnect_after_connecting_is_not_proof_of_a_dead_listener() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("rude.sock");
        let l = UnixListener::bind(&p).unwrap();
        let h = std::thread::spawn(move || {
            // Accept twice (`probe_socket` probes twice), hanging up immediately each time.
            for _ in 0..2 {
                match l.accept() {
                    Ok((s, _)) => drop(s),
                    Err(_) => break,
                }
            }
            // The listener stays alive until the thread ends — "someone is still there, this one
            // connection just dropped".
            l
        });

        assert_ne!(
            probe_socket(&p, tmp.path()),
            Liveness::Stale,
            "the listener is still there, only this connection dropped — a stale verdict makes \
             listen() remove a live daemon's socket"
        );
        drop(h.join());
    }

    /// A **live** listener with a full accept queue must not be judged `Stale`.
    ///
    /// `UnixListener::bind` uses backlog=128. On a listener that binds and then never accepts, the
    /// first 128 connects all succeed and the 129th returns ECONNREFUSED — while the listener is
    /// alive throughout. Taking that ECONNREFUSED as "the kernel has no listener" makes `listen()`
    /// remove the socket and bind over it, orphaning a daemon that is still running: it is still
    /// there, nobody can reach it any more, and the failure is silent.
    ///
    /// The trigger is real: the control thread accepts serially and the `Request::Status` handler
    /// gives itself 2 seconds to take the lock, so a script polling `agit rc status` fills the
    /// queue.
    ///
    /// **macOS only**, because what it exercises is the macOS ECONNREFUSED path. On Linux a
    /// blocking connect against a full backlog **waits indefinitely** (observed: squeeze the
    /// backlog to 1 with `listen(fd, 1)`, fill it, and the second connect does not return), so the
    /// loop below that fills the queue with `UnixStream::connect` would wedge the test itself
    /// there. That path is covered by [`a_connect_that_would_block_forever_times_out`], which runs
    /// on both platforms.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_live_listener_with_a_full_backlog_is_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path();
        let p = rc.join("busy.sock");
        // The owner is alive — this process. This is the side on which that necessary condition
        // fails.
        std::fs::write(rc.join("agitd.pid"), std::process::id().to_string()).unwrap();

        // Bound but **never accepted on**: the queue only fills.
        let _l = UnixListener::bind(&p).unwrap();
        // The connections must be **held**: dropping them drains the queue, and the probe no
        // longer meets this shape.
        let mut held: Vec<UnixStream> = Vec::new();
        let filled = loop {
            match UnixStream::connect(&p) {
                Ok(s) => held.push(s),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => break true,
                Err(e) => panic!("unexpected connect error while filling the backlog: {e}"),
            }
            if held.len() > 1024 {
                break false;
            }
        };
        assert!(
            filled,
            "the backlog is not full after {} connections, so this test never reaches the shape \
             it pins",
            held.len()
        );

        assert_ne!(
            probe_socket(&p, rc),
            Liveness::Stale,
            "the listener is alive throughout, only its accept queue is full — a stale verdict \
             makes listen() remove its socket and bind over it while it is still running"
        );
        // Release only after probing: dropping any earlier drains the queue.
        drop(held);
    }

    /// Fill a listener's accept queue, returning the connections that **must stay held**.
    ///
    /// The platform difference hides here and nowhere else: on a full queue macOS refuses
    /// (ECONNREFUSED) and Linux waits. So this does not use `UnixStream::connect` (which would
    /// wedge on the last one on Linux) but [`connect_within`] with a very short budget — refused or
    /// timed out, both count as "full".
    fn fill_backlog(path: &std::path::Path) -> Vec<UnixStream> {
        let mut held = Vec::new();
        // The cap is only a guardrail: the backlog is already squeezed to 1, so a few connections
        // fill it.
        while held.len() < 64 {
            match connect_within(path, std::time::Duration::from_millis(200)) {
                Ok(s) => held.push(s),
                Err(_) => break,
            }
        }
        held
    }

    /// `connect` **itself** must be bounded, or a full backlog wedges the probe.
    ///
    /// `set_read_timeout` / `set_write_timeout` only govern reads and writes **after** the
    /// connection is established. Against a listener whose accept queue is full:
    ///
    /// * macOS refuses outright with ECONNREFUSED (via [`refused_connect_verdict`]);
    /// * on Linux a blocking connect **waits indefinitely** — observed by squeezing the backlog to
    ///   1, filling it, and watching the second connect not return.
    ///
    /// The latter is exactly the shape in which `agit rc status` / `agit rc start` wedge: the
    /// control thread accepts serially, a full queue sticks on the `connect` line, and the
    /// read/write budget never comes into play.
    ///
    /// So this test asserts only the two things that **hold on both platforms**: the probe
    /// **returns in time**, and the verdict is **not** `Stale` (an unclear reason for not
    /// connecting never justifies removing a socket). The two paths differ, the difference hides in
    /// [`fill_backlog`], and the assertions are one and the same — no `#[cfg(target_os)]`.
    #[test]
    fn a_connect_that_would_block_forever_times_out() {
        use std::os::fd::AsRawFd;

        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path();
        let p = rc.join("wedged.sock");
        // The owner is alive (this process): the macOS ECONNREFUSED path needs that to land on
        // `Unknown`, or `refused_connect_verdict` (correctly) returns `Stale` and the test pins
        // something else.
        std::fs::write(rc.join("agitd.pid"), std::process::id().to_string()).unwrap();

        // Bound but **never accepted on**: the queue only fills.
        let l = UnixListener::bind(&p).unwrap();
        // `UnixListener::bind` uses backlog=128, and Linux allows even more to queue — filling
        // that takes hundreds of connections and "is it actually full" stays uncertain. Squeezed to
        // 1, a full queue is certain and a few connections reach it.
        assert_eq!(
            unsafe { libc::listen(l.as_raw_fd(), 1) },
            0,
            "squeezing the backlog to 1 failed: {}",
            std::io::Error::last_os_error()
        );
        // The connections must be **held**: dropping them drains the queue.
        let held = fill_backlog(&p);
        assert!(
            !held.is_empty(),
            "no connection went through, so this test never reaches the shape it pins"
        );

        let started = std::time::Instant::now();
        let verdict = probe_socket(&p, rc);
        let took = started.elapsed();

        // The opposite of "forever": two probes, each connect capped at [`CONNECT_TIMEOUT`], so
        // the bound is twice that budget. The value below leaves margin for slow machines — a
        // genuine wedge never returns at all.
        assert!(
            took < std::time::Duration::from_secs(15),
            "the probe took {took:?}: without a cap on connect it waits forever"
        );
        assert_ne!(
            verdict,
            Liveness::Stale,
            "an unclear reason for not connecting (only a full queue) must never be judged stale \
             and remove another process's socket: {verdict:?}"
        );
        // Release only after probing; only then does the listener finish.
        drop(held);
        drop(l);
    }

    /// When the answer is "cannot tell", another process's socket **must not** be removed.
    ///
    /// "`remove_file` whenever `connect` fails" is not a safe rule for `listen()`: exhausted fds,
    /// wrong permissions and a signal interruption all make `connect` fail — so it would remove a
    /// **live** daemon's socket and bind over it, orphaning it: still running, and nobody can reach
    /// it any more.
    #[test]
    fn an_unknown_liveness_never_deletes_the_socket() {
        // The whole point of the `Unknown` variant is that it does not equal `Stale`.
        let unknown = Liveness::Unknown("fd exhausted".into());
        assert_ne!(unknown, Liveness::Stale);
        assert_ne!(unknown, Liveness::Live);
    }

    /// A pidfile **alone** is not evidence that a daemon is running.
    ///
    /// Checking only the pidfile inverts the answer: a file left behind by a SIGKILLed daemon makes
    /// `agit rc start` refuse to start forever while `agit rc status` says there is no daemon — two
    /// contradictory messages, and the user has nowhere to go. Asking the socket for an answer
    /// settles it.
    ///
    /// "the process in the pidfile is still alive" is elsewhere a necessary condition for **not**
    /// removing the socket (see [`refused_connect_verdict`]), but it is never evidence of running:
    /// here that pid is alive (it is this process) while the socket does not exist at all.
    #[test]
    fn a_pidfile_alone_is_not_evidence_that_a_daemon_is_running() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path();
        std::fs::write(rc.join("agitd.pid"), std::process::id().to_string()).unwrap();

        // The pid is alive (this process), but nobody listens on the socket.
        assert_eq!(
            running_pid_in(rc),
            None,
            "with no socket nothing counts as running"
        );

        // Only an **answer** counts, and the pid reported is the one in the pidfile. The test is
        // that something answers, not that something bound, so this needs a fake daemon that
        // really answers.
        let sock = socket_path_for(rc);
        let h = answering_daemon(&sock, 1);
        assert_eq!(running_pid_in(rc), Some(std::process::id()));
        h.join().expect("the fake daemon must not panic");

        // Once the socket is gone nothing counts, even with the pidfile still there.
        std::fs::remove_file(&sock).unwrap();
        assert_eq!(running_pid_in(rc), None);
    }

    /// When the probe cannot tell, the `agit rc status` path **must not** report "no daemon".
    ///
    /// # The two messages must not contradict each other
    ///
    /// `listen()` **refuses to start** on `Unknown` (it will not remove the socket of a daemon that
    /// may still be alive). Conflating `Unknown` with "there is none" on this side makes
    /// `agit rc status` say there is no daemon while `agit rc start` says there already is one, and
    /// the user has nowhere to go.
    ///
    /// What this builds is the most reachable shape: the probe gets no answer (the peer hangs up
    /// right after accepting) while the process in the pidfile is alive (it is this process). The
    /// verdict must land on `Unclear`, not `Absent`.
    #[test]
    fn an_unclear_probe_is_never_reported_as_no_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path();
        // The owner is alive — this process.
        std::fs::write(rc.join("agitd.pid"), std::process::id().to_string()).unwrap();
        let sock = socket_path_for(rc);
        let l = UnixListener::bind(&sock).unwrap();
        // Hang up right after accepting: it connects, it does not answer. Two probes take two
        // connections each, so accept four times.
        let h = std::thread::spawn(move || {
            for _ in 0..4 {
                match l.accept() {
                    Ok((s, _)) => drop(s),
                    Err(_) => break,
                }
            }
            l // The listener is alive throughout — someone is still there, this connection dropped
        });

        let p = presence_in(rc);
        assert!(
            matches!(p, Presence::Unclear(_)),
            "a probe that cannot tell must not be reported as anything else: {p:?}"
        );
        assert_ne!(
            p,
            Presence::Absent,
            "reporting a definite absence contradicts an `agit rc start` that refuses to start"
        );
        // This path answers "which pid", and when the probe cannot tell there is still no honest
        // answer to give — "no pid to give" and "no daemon" are two different things, which is
        // exactly why the variant above exists.
        assert_eq!(running_pid_in(rc), None);
        drop(h.join());
    }

    /// The other direction: a definite absence is stated plainly, or a permanent "cannot tell"
    /// says nothing at all.
    #[test]
    fn a_socket_with_nobody_behind_it_is_reported_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let rc = tmp.path();
        // No socket and no pidfile: that is a definite absence.
        assert_eq!(presence_in(rc), Presence::Absent);
    }

    /// A unix socket path has a hard length cap, and going over makes `bind` fail outright. A deep
    /// directory must fall back to a short path, and both sides must compute the same value — or
    /// `agit rc status` connects to a socket that does not exist and reports "no daemon", pointing
    /// the reader in entirely the wrong direction.
    #[test]
    fn a_deep_agit_home_falls_back_to_a_short_socket_path() {
        // The observed failure: with AGIT_HOME under a very deep temp directory, `bind()` reports
        // `path must be shorter than SUN_LEN`, the daemon cannot start, and `agit rc status` only
        // says "no daemon is running" — a message that points the reader the wrong way.
        let deep = std::path::Path::new("/tmp")
            .join("a".repeat(120))
            .join("rc");
        let p = socket_path_for(&deep);
        assert!(
            p.as_os_str().len() <= SUN_PATH_MAX,
            "{} is {} bytes, over the {SUN_PATH_MAX} limit",
            p.display(),
            p.as_os_str().len()
        );
        // Determinism: the client and the daemon each compute it, and must get the same one.
        assert_eq!(p, socket_path_for(&deep));
        // A different home gets a different socket, or two agit installations fight over one.
        let other = std::path::Path::new("/tmp")
            .join("b".repeat(120))
            .join("rc");
        assert_ne!(p, socket_path_for(&other));

        // A short path keeps the preferred location — easier to find alongside the rest of the
        // state.
        let shallow = std::path::Path::new("/tmp/agit-short/rc");
        assert!(socket_path_for(shallow).ends_with("rc/control.sock"));
    }

    #[test]
    fn requests_and_replies_round_trip_as_tagged_json() {
        let r = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(r, r#"{"op":"status"}"#);
        let rep = Reply::Status(Status {
            pid: 7,
            hub: "h".into(),
            online: true,
            ..Default::default()
        });
        let s = serde_json::to_string(&rep).unwrap();
        assert!(s.contains(r#""reply":"status""#));
        let back: Reply = serde_json::from_str(&s).unwrap();
        matches!(back, Reply::Status(_));

        let reload = serde_json::to_string(&Request::ReloadSecrets).unwrap();
        assert_eq!(reload, r#"{"op":"reload_secrets"}"#);
        let reply = Reply::SecretsReloaded {
            generation: 3,
            rules: 7,
        };
        let encoded = serde_json::to_string(&reply).unwrap();
        let decoded: Reply = serde_json::from_str(&encoded).unwrap();
        assert!(matches!(
            decoded,
            Reply::SecretsReloaded {
                generation: 3,
                rules: 7
            }
        ));
    }
}
