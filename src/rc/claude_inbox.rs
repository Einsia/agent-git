//! Claude Code's cross-session messaging, used as a native inbox.
//!
//! A Claude Code process registers the session it runs as `<claude config>/sessions/<pid>.json`,
//! naming a Unix socket that accepts newline-delimited JSON frames. A `user` frame becomes a
//! message of that session: it is read between tool calls while a turn runs, and it starts a new
//! turn when the session is idle. The process that owns the transcript records the message
//! itself, so delivering through its socket never adds a second transcript writer.
//!
//! The token that authenticates a frame decides how Claude Code admits it:
//!
//! * the daemon sends with the session's peer token. Claude Code accepts such a message in
//!   prompting modes and holds it for approval on the machine in bypass-permissions mode;
//! * a relay started from agit's own Claude Code Stop hook sends with the child token that the
//!   session exports only to its hooks and tools. Claude Code admits that message as one the
//!   session sent itself, in every mode. The relay receives the token through its inherited
//!   environment and keeps it in memory; no file or log ever holds it.
//!
//! Claude Code applies its hold by the session's live permission mode, which the transcript
//! records late or not at all. So only the device owner's messages may pass through a relay; every
//! other message goes straight to the socket under the peer token, where that live hold applies.
//!
//! The owner's messages pass through a per-session queue, so whichever sender is present takes
//! each one:
//!
//! ```text
//! <agit home>/claude-inbox/<session id>/
//!     relay.lock          held exclusively by the session's live relay; probes share it briefly
//!     pending/<key>.json  written atomically, waiting for a sender
//!     sending/<key>.json  claimed by exactly one sender; kept when delivery is uncertain
//! ```
//!
//! A sender claims a message by renaming it out of `pending/`. Only one rename succeeds, so a
//! message reaches the session at most once even when the daemon and a relay race for it. Each
//! entry records that the owner sent it, when it was queued and which process it is for; a relay
//! sends only the owner's fresh entries for its own process, and removes the queue when it ends
//! with nothing left in it.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// The `native_inbox` value `session.watch` reports for a session with a live socket.
pub const NATIVE_INBOX: &str = "claude_socket";
/// The `native_inbox` value for a live socket that only the owner may send to.
pub const NATIVE_INBOX_UNAVAILABLE: &str = "unavailable";

/// The sender label Claude Code shows beside a delivered message.
const FROM: &str = "agit-rc";
/// The framing below is the one this protocol version defines; another version may frame
/// messages differently, so its sessions are not offered an inbox.
const PEER_PROTOCOL: u64 = 1;
const RECORD_LIMIT: u64 = 64 * 1024;
/// Bounds a queued message: the inbox limit plus attribution, with JSON escaping.
const PENDING_LIMIT: u64 = 256 * 1024;
const RELAY_POLL: Duration = Duration::from_millis(500);
/// A registration that Claude Code is rewriting cannot be read for a moment.
const RELAY_PATIENCE: Duration = Duration::from_secs(10);
/// How long the owner's message waits for a running relay to take it before the daemon sends it
/// itself. It spans several relay polls, so a relay between polls or busy with another message
/// still takes it.
const HANDOFF: Duration = RELAY_POLL.saturating_mul(4);
const HANDOFF_CHECK: Duration = Duration::from_millis(50);
/// A queued entry older than this outlived the hand-off that queued it; a relay drops it rather
/// than deliver it late. It must exceed `HANDOFF`, or a relay drops messages the daemon still
/// waits on.
const PENDING_TTL: Duration = Duration::from_secs(30);
const _: () = assert!(PENDING_TTL.as_millis() > HANDOFF.as_millis());
/// A probe shares the relay lock only for the instant it looks, so a relay retries through it.
const LOCK_RETRY: Duration = Duration::from_millis(10);
const PENDING: &str = "pending";
const SENDING: &str = "sending";
const LOCK: &str = "relay.lock";
const TOKEN_ENV: &str = "CLAUDE_CODE_MESSAGING_TOKEN";
const SOCKET_ENV: &str = "CLAUDE_CODE_MESSAGING_SOCKET";
const BYPASS: &str = "bypassPermissions";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    pid: u32,
    session_id: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    proc_start: Option<String>,
    #[serde(default)]
    peer_protocol: Option<u64>,
    #[serde(default)]
    messaging_socket_path: Option<String>,
}

/// A session that runs in a live Claude Code process listening on its messaging socket.
#[derive(Clone)]
pub(crate) struct Live {
    pid: u32,
    /// When the registry says the process started; with the pid it names one process.
    proc_start: Option<String>,
    session_id: String,
    socket: PathBuf,
    cwd: Option<PathBuf>,
    peer_token: Option<String>,
}

impl std::fmt::Debug for Live {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Live")
            .field("pid", &self.pid)
            .field("proc_start", &self.proc_start)
            .field("session_id", &self.session_id)
            .field("socket", &self.socket)
            .field("cwd", &self.cwd)
            .field(
                "peer_token",
                &self.peer_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Live {
    /// The directory the process reported when it registered.
    pub(crate) fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    /// Another discovery names the same process and socket.
    pub(crate) fn same_process(&self, other: &Live) -> bool {
        self.pid == other.pid && self.proc_start == other.proc_start && self.socket == other.socket
    }
}

/// The transcript the live process writes, located from the folder it registered.
///
/// `cwd` is the session's folder as the caller found it; a process registered in another folder
/// writes another file, so the two must agree. Claude Code names the project directory after the
/// physical folder; a second copy under the lexical spelling leaves the written file unknown, so
/// neither is chosen.
pub(crate) fn live_transcript(live: &Live, cwd: &Path) -> crate::Result<PathBuf> {
    use crate::adapter::claude_code::{canonical_cwd, projects_dir, slug_for};
    let process = live
        .cwd()
        .context("the Claude Code process did not register its folder")?;
    ensure!(
        canonical_cwd(process) == canonical_cwd(cwd),
        "the Claude Code process runs this session from another folder"
    );
    let projects = projects_dir()?;
    let name = format!("{}.jsonl", live.session_id);
    let mut found: Option<PathBuf> = None;
    for slug in [slug_for(process), crate::domain::store::slug_for(process)] {
        let path = projects.join(slug).join(&name);
        if found.as_ref() == Some(&path)
            || !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file())
        {
            continue;
        }
        ensure!(
            found.replace(path).is_none(),
            "two copies of this Claude Code transcript leave the one its process writes unknown"
        );
    }
    found.context("cannot locate the transcript this Claude Code process writes")
}

fn read_bounded(path: &Path, limit: u64) -> crate::Result<Vec<u8>> {
    let file = super::native_inbox::open_regular(path)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{} is larger than expected",
        path.display()
    );
    Ok(bytes)
}

fn record(path: &Path) -> Option<Record> {
    serde_json::from_slice(&read_bounded(path, RECORD_LIMIT).ok()?).ok()
}

/// Every registration whose file name and content name the same process.
fn records(registry: &Path) -> Vec<(u32, Record)> {
    let Ok(entries) = std::fs::read_dir(registry) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let pid: u32 = entry
                .file_name()
                .to_str()?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            let record = record(&entry.path())?;
            (record.pid == pid).then_some((pid, record))
        })
        .collect()
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // A process of another user cannot own this user's socket, so `EPERM` counts as absent.
    libc::pid_t::try_from(pid).is_ok_and(|pid| pid > 0 && unsafe { libc::kill(pid, 0) } == 0)
}

#[cfg(not(unix))]
fn process_alive(_: u32) -> bool {
    false
}

#[cfg(unix)]
fn own_socket(path: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() }
    })
}

#[cfg(not(unix))]
fn own_socket(_: &Path) -> bool {
    false
}

fn token_shape(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The socket of a registration whose process still runs and listens.
fn listening(pid: u32, record: &Record) -> Option<&str> {
    let socket = record.messaging_socket_path.as_deref()?;
    (record.peer_protocol == Some(PEER_PROTOCOL)
        && Path::new(socket).is_absolute()
        && process_alive(pid)
        && own_socket(Path::new(socket)))
    .then_some(socket)
}

/// Claude Code names the peer key after the socket path exactly as registered.
fn peer_token(registry: &Path, pid: u32, socket: &str) -> Option<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Key {
        peer_token: String,
    }
    let name = format!(
        "{pid}.{}.key",
        hex::encode(Sha256::digest(socket.as_bytes()))
    );
    let key: Key = serde_json::from_slice(&read_bounded(&registry.join(name), 4096).ok()?).ok()?;
    token_shape(&key.peer_token).then_some(key.peer_token)
}

/// The live process that runs `session_id`, from the registry in `registry`.
///
/// Two live processes naming one session leave the recipient ambiguous, so neither is chosen.
pub(crate) fn discover_in(registry: &Path, session_id: &str) -> Option<Live> {
    let mut found = None;
    for (pid, record) in records(registry) {
        if record.session_id != session_id {
            continue;
        }
        let Some(socket) = listening(pid, &record) else {
            continue;
        };
        let live = Live {
            pid,
            peer_token: peer_token(registry, pid, socket),
            socket: PathBuf::from(socket),
            proc_start: record.proc_start,
            session_id: record.session_id,
            cwd: record.cwd,
        };
        if found.replace(live).is_some() {
            return None;
        }
    }
    found
}

pub(crate) fn discover(session_id: &str) -> Option<Live> {
    discover_in(
        &crate::adapter::claude_code::sessions_dir().ok()?,
        session_id,
    )
}

/// Whether a live Claude Code process runs this session.
///
/// Only a positive answer is exact: a session missing from the registry can still run in a
/// process that does not register itself. Listings ask once per session, so one registry read
/// serves every question for a moment.
pub(crate) fn session_is_live(session_id: &str) -> bool {
    use std::{collections::HashSet, sync::Mutex};
    type Snapshot = (Instant, PathBuf, HashSet<String>);
    static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);
    const FRESH: Duration = Duration::from_secs(2);
    let Ok(registry) = crate::adapter::claude_code::sessions_dir() else {
        return false;
    };
    let mut snapshot = SNAPSHOT.lock().unwrap_or_else(|error| error.into_inner());
    if !snapshot
        .as_ref()
        .is_some_and(|(at, dir, _)| *dir == registry && at.elapsed() < FRESH)
    {
        let live = records(&registry)
            .into_iter()
            .filter(|(pid, record)| listening(*pid, record).is_some())
            .map(|(_, record)| record.session_id)
            .collect();
        *snapshot = Some((Instant::now(), registry, live));
    }
    snapshot
        .as_ref()
        .is_some_and(|(_, _, live)| live.contains(session_id))
}

/// Whether a Claude Code transcript ever ran with permission checks bypassed.
///
/// A transcript that records no permission mode is treated as one that did: nothing in it shows
/// the session was ever supervised.
pub(crate) fn transcript_ran_unchecked(path: &Path) -> crate::Result<bool> {
    use std::io::BufRead;
    // Only Claude Code's own records count: the top-level `permissionMode` of a user prompt or a
    // permission-mode checkpoint. The same key inside a tool's input or output is data, and a
    // substring match on it would let a transcript that never recorded its mode look checked.
    const CHECKED: [&str; 6] = [
        "default",
        "acceptEdits",
        "auto",
        "plan",
        "dontAsk",
        "manual",
    ];
    const KEY: &[u8] = b"permissionMode";
    let mut reader = std::io::BufReader::new(super::native_inbox::open_regular(path)?);
    let mut line = Vec::new();
    let mut observed = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if !line.windows(KEY.len()).any(|window| window == KEY) {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(&line) else {
            continue;
        };
        if !matches!(
            record.get("type").and_then(serde_json::Value::as_str),
            Some("user" | "permission-mode")
        ) {
            continue;
        }
        match record
            .get("permissionMode")
            .and_then(serde_json::Value::as_str)
        {
            Some(BYPASS) => return Ok(true),
            Some(mode) if CHECKED.contains(&mode) => observed = true,
            _ => {}
        }
    }
    Ok(!observed)
}

fn frames(
    token: Option<&str>,
    session_id: &str,
    msg_id: &str,
    content: &str,
) -> crate::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(token) = token {
        serde_json::to_writer(
            &mut bytes,
            &serde_json::json!({"type": "auth", "token": token}),
        )?;
        bytes.push(b'\n');
    }
    serde_json::to_writer(
        &mut bytes,
        &serde_json::json!({
            "type": "user",
            "session_id": session_id,
            "msg_id": msg_id,
            "from": FROM,
            "priority": "next",
            "message": {"role": "user", "content": content},
        }),
    )?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Why a message did not reach its session.
#[derive(Debug)]
pub(crate) enum SendError {
    /// Nothing reached the socket; sending again cannot duplicate the message.
    NotDelivered(anyhow::Error),
    /// The message may have reached the session, through the socket or a relay that took it from
    /// the queue; sending again may duplicate it.
    Uncertain(anyhow::Error),
}

async fn send(
    socket: &Path,
    token: Option<&str>,
    session_id: &str,
    msg_id: &str,
    content: &str,
) -> Result<(), SendError> {
    let bytes = frames(token, session_id, msg_id, content).map_err(SendError::NotDelivered)?;
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt;
        const IO_TIMEOUT: Duration = Duration::from_secs(5);
        // Claude Code closes a connection that completes no line in time, so the frames exist
        // before the connection does.
        let mut stream =
            match tokio::time::timeout(IO_TIMEOUT, tokio::net::UnixStream::connect(socket)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => return Err(SendError::NotDelivered(error.into())),
                Err(_) => {
                    return Err(SendError::NotDelivered(anyhow::anyhow!(
                        "the Claude Code messaging socket did not accept a connection"
                    )));
                }
            };
        let written = tokio::time::timeout(IO_TIMEOUT, async {
            stream.write_all(&bytes).await?;
            stream.flush().await?;
            stream.shutdown().await
        })
        .await;
        match written {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(SendError::Uncertain(error.into())),
            Err(_) => Err(SendError::Uncertain(anyhow::anyhow!(
                "the Claude Code messaging socket stopped reading"
            ))),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, bytes);
        Err(SendError::NotDelivered(anyhow::anyhow!(
            "Claude Code messaging needs a Unix socket"
        )))
    }
}

#[derive(Serialize, Deserialize)]
struct Pending {
    msg_id: String,
    content: String,
    /// Set only for the device owner's message; a relay delivers no other.
    #[serde(default)]
    owner: bool,
    /// When the entry became visible to senders, in milliseconds since the Unix epoch.
    #[serde(default)]
    enqueued_at: u64,
    /// The process the entry is addressed to.
    #[serde(default)]
    pid: u32,
    #[serde(default)]
    proc_start: Option<String>,
}

impl Pending {
    fn expired(&self, now: u64) -> bool {
        u128::from(now.abs_diff(self.enqueued_at)) > PENDING_TTL.as_millis()
    }

    fn addressed_to(&self, live: &Live) -> bool {
        self.pid == live.pid && self.proc_start == live.proc_start
    }

    /// Why a relay must not send this entry to `holder`, if it must not.
    fn refusal(&self, holder: &Live, now: u64) -> Option<&'static str> {
        if !self.owner {
            Some("it does not carry the owner mark")
        } else if self.expired(now) {
            Some("it outlived its hand-off")
        } else if !self.addressed_to(holder) {
            Some("it is addressed to another process")
        } else {
            None
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// Whether `path` still names the open `file`.
#[cfg(unix)]
fn names(path: &Path, file: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::symlink_metadata(path), file.metadata()) {
        (Ok(named), Ok(open)) => named.dev() == open.dev() && named.ino() == open.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn names(_: &Path, _: &std::fs::File) -> bool {
    true
}

/// A note about a relay decision. The relay runs detached with its output discarded, so only a
/// debug build run by hand shows it.
fn debug_note(note: std::fmt::Arguments<'_>) {
    if cfg!(debug_assertions) {
        eprintln!("agit hooks relay: {note}");
    }
}

/// One session's message queue.
pub(crate) struct Queue {
    dir: PathBuf,
}

/// Held by the session's live relay. Dropping it lets the next relay of the session start.
pub(crate) struct RelayLock(std::fs::File);

impl Drop for RelayLock {
    fn drop(&mut self) {
        // A forked child can retain the open file description until exec; unlocking explicitly
        // keeps the next relay from waiting on it.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

impl Queue {
    /// Where a session's queue lives. The name is an exact UUID, so it stays one path component.
    pub(crate) fn path_for(session_id: &str) -> crate::Result<PathBuf> {
        ensure!(
            super::native_inbox::valid_id(session_id),
            "an exact Claude Code session UUID is required"
        );
        Ok(super::state_dir("claude-inbox")?.join(session_id))
    }

    pub(crate) fn open(dir: PathBuf) -> crate::Result<Self> {
        let queue = Self { dir };
        queue.prepare()?;
        Ok(queue)
    }

    /// Create the queue's directories; an ending relay may have removed them.
    fn prepare(&self) -> crate::Result<()> {
        for path in [
            self.dir.clone(),
            self.dir.join(PENDING),
            self.dir.join(SENDING),
        ] {
            crate::infra::config::create_state_dir(&path)?;
            ensure!(
                std::fs::symlink_metadata(&path)?.is_dir(),
                "the Claude Code inbox must be a directory, not a link"
            );
        }
        Ok(())
    }

    fn message(&self, state: &str, key: &str) -> PathBuf {
        self.dir.join(state).join(format!("{key}.json"))
    }

    fn lock_file(&self) -> crate::Result<std::fs::File> {
        self.prepare()?;
        let mut options = crate::infra::config::state_file_options();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(self.dir.join(LOCK))?;
        ensure!(
            file.metadata()?.is_file(),
            "the Claude Code relay lock is not a regular file"
        );
        Ok(file)
    }

    /// The relay lock, or `None` while another relay of this session holds it.
    pub(crate) fn try_lock(&self) -> crate::Result<Option<RelayLock>> {
        let file = self.lock_file()?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {}
            Err(error) if contended(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let lock = RelayLock(file);
        // An ending relay removes the lock file it holds; a lock on a file that no longer has the
        // name excludes nobody, so it counts as held and the caller opens the name again.
        Ok(names(&self.dir.join(LOCK), &lock.0).then_some(lock))
    }

    /// Whether a relay of this session runs. The probe shares the lock and releases it at once,
    /// so probes never read one another as a relay.
    pub(crate) fn relay_active(&self) -> crate::Result<bool> {
        let file = self.lock_file()?;
        match fs2::FileExt::try_lock_shared(&file) {
            Ok(()) => {
                let _ = fs2::FileExt::unlock(&file);
                Ok(false)
            }
            Err(error) if contended(&error) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    /// Make the message for `live` visible to senders. The file appears complete or not at all.
    /// `owner` records whether the device owner sent it; a relay delivers no other message.
    ///
    /// Once the file is in `pending/` a relay may already have sent it, so a failure after that
    /// point is `Uncertain`: retrying the same message could deliver it twice.
    pub(crate) fn enqueue(
        &self,
        key: &str,
        msg_id: &str,
        content: &str,
        live: &Live,
        owner: bool,
    ) -> Result<(), SendError> {
        let prepare = || -> crate::Result<tempfile::NamedTempFile> {
            ensure!(
                valid_key(key),
                "a Claude Code inbox key must be a SHA-256 digest"
            );
            let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
            serde_json::to_writer(
                &mut file,
                &Pending {
                    msg_id: msg_id.to_owned(),
                    content: content.to_owned(),
                    owner,
                    enqueued_at: now_millis(),
                    pid: live.pid,
                    proc_start: live.proc_start.clone(),
                },
            )?;
            file.as_file().sync_all()?;
            Ok(file)
        };
        let file = prepare().map_err(SendError::NotDelivered)?;
        file.persist(self.message(PENDING, key))
            .map_err(|error| SendError::NotDelivered(error.error.into()))?;
        super::native_inbox::sync_directory(&self.dir.join(PENDING)).map_err(SendError::Uncertain)
    }

    /// Pending keys, oldest first.
    fn pending(&self) -> crate::Result<Vec<String>> {
        let mut keys = Vec::new();
        for entry in std::fs::read_dir(self.dir.join(PENDING))? {
            let entry = entry?;
            let Some(key) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(".json"))
                .filter(|key| valid_key(key))
                .map(str::to_owned)
            else {
                continue;
            };
            keys.push((entry.metadata().and_then(|m| m.modified()).ok(), key));
        }
        keys.sort();
        Ok(keys.into_iter().map(|(_, key)| key).collect())
    }

    fn is_pending(&self, key: &str) -> crate::Result<bool> {
        match std::fs::symlink_metadata(self.message(PENDING, key)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Read a pending message without taking it.
    fn peek(&self, key: &str) -> crate::Result<Pending> {
        Ok(serde_json::from_slice(&read_bounded(
            &self.message(PENDING, key),
            PENDING_LIMIT,
        )?)?)
    }

    /// Take one pending message; `None` means another sender took it first.
    fn claim(&self, key: &str) -> crate::Result<Option<Pending>> {
        let pending = self.message(PENDING, key);
        let claimed = self.message(SENDING, key);
        match std::fs::rename(&pending, &claimed) {
            Ok(()) => {}
            // Only a message whose own file is gone was taken; a missing `sending/` leaves it
            // pending.
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && std::fs::symlink_metadata(&pending).is_err() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        // The claim is durable before anything is sent: after a crash the message must not
        // reappear in `pending/` and go out a second time.
        super::native_inbox::sync_directory(&self.dir.join(SENDING))?;
        super::native_inbox::sync_directory(&self.dir.join(PENDING))?;
        let message = read_bounded(&claimed, PENDING_LIMIT)
            .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?));
        if message.is_err() {
            // No sender can deliver an entry it cannot read.
            let _ = std::fs::remove_file(&claimed);
        }
        message.map(Some)
    }

    /// Return a claimed message that certainly did not reach the session.
    fn release(&self, key: &str) -> crate::Result<()> {
        std::fs::rename(self.message(SENDING, key), self.message(PENDING, key))?;
        super::native_inbox::sync_directory(&self.dir.join(PENDING))?;
        super::native_inbox::sync_directory(&self.dir.join(SENDING))?;
        Ok(())
    }

    fn finish(&self, key: &str) -> crate::Result<()> {
        std::fs::remove_file(self.message(SENDING, key))?;
        Ok(())
    }

    /// Remove the queue once nothing waits in it. Only the holder of the relay lock calls this,
    /// so no running relay loses its queue, and each step removes only an empty directory, so a
    /// message queued meanwhile keeps the rest.
    fn remove_if_idle(&self) {
        let _ = std::fs::remove_dir(self.dir.join(PENDING))
            .and_then(|()| std::fs::remove_dir(self.dir.join(SENDING)))
            .and_then(|()| std::fs::remove_file(self.dir.join(LOCK)))
            .and_then(|()| std::fs::remove_dir(&self.dir));
    }
}

/// Send a message straight to the session under its peer token. Claude Code admits it in
/// prompting modes and holds it for approval on the machine in bypass-permissions mode.
pub(crate) async fn send_direct(live: &Live, msg_id: &str, content: &str) -> Result<(), SendError> {
    send(
        &live.socket,
        live.peer_token.as_deref(),
        &live.session_id,
        msg_id,
        content,
    )
    .await
}

/// Hand the owner's enqueued message to its session. A running relay has a moment to take it and
/// send it as the session; otherwise the daemon claims it back and sends it under the peer token,
/// which Claude Code may hold for approval on the machine.
pub(crate) async fn forward(queue: &Queue, live: &Live, key: &str) -> Result<(), SendError> {
    // The message is visible to relays, so a failure before the daemon claims it back is
    // uncertain.
    if queue.relay_active().map_err(SendError::Uncertain)? {
        let deadline = Instant::now() + HANDOFF;
        while queue.is_pending(key).map_err(SendError::Uncertain)? && Instant::now() < deadline {
            tokio::time::sleep(HANDOFF_CHECK).await;
        }
    }
    let message = match queue.claim(key) {
        Ok(Some(message)) => message,
        // A relay took it.
        Ok(None) => return Ok(()),
        Err(error) => return Err(SendError::Uncertain(error)),
    };
    let sent = send_direct(live, &message.msg_id, &message.content).await;
    // A message that may have arrived stays claimed, so no sender ever repeats it.
    if !matches!(sent, Err(SendError::Uncertain(_))) {
        let _ = queue.finish(key);
    }
    sent
}

/// The messaging credentials a Claude Code session exports to its own hooks. The socket path
/// names one live process, so it binds the token to that process.
pub(crate) struct Credentials {
    token: String,
    socket: PathBuf,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("token", &"<redacted>")
            .field("socket", &self.socket)
            .finish()
    }
}

impl Credentials {
    pub(crate) fn from_env() -> Option<Self> {
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|token| token_shape(token))?;
        let socket = PathBuf::from(std::env::var_os(SOCKET_ENV)?);
        socket.is_absolute().then_some(Self { token, socket })
    }
}

/// Delivers the owner's queued messages for one session as messages the session sent itself.
struct Relay {
    registry: PathBuf,
    queue: Queue,
    session_id: String,
    credentials: Credentials,
    poll: Duration,
    patience: Duration,
}

impl Relay {
    /// The process that runs this session, when it is the process whose token the relay holds.
    fn holder(&self) -> Option<Live> {
        discover_in(&self.registry, &self.session_id)
            .filter(|live| live.socket == self.credentials.socket)
    }

    /// Whether `pid` still runs this session. A registration that cannot be read while it is
    /// rewritten says nothing; one naming another session means the process switched sessions,
    /// and frames naming this one would be dropped.
    fn still_held(&self, pid: u32) -> bool {
        process_alive(pid)
            && own_socket(&self.credentials.socket)
            && record(&self.registry.join(format!("{pid}.json")))
                .is_none_or(|record| record.session_id == self.session_id)
    }

    /// The relay lock, retried through probes that share it for an instant. A relay that holds
    /// it keeps it for longer than the retries last.
    async fn acquire(&self) -> crate::Result<Option<RelayLock>> {
        let deadline = Instant::now() + self.poll;
        loop {
            if let Some(lock) = self.queue.try_lock()? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(LOCK_RETRY.min(self.poll)).await;
        }
    }

    async fn run(self) -> crate::Result<()> {
        let mut lock = None;
        let served = self.serve(&mut lock).await;
        // Whichever way the relay ends, an idle queue goes with it, unless another relay holds it.
        let lock = match lock {
            Some(lock) => Some(lock),
            None => self.queue.try_lock().ok().flatten(),
        };
        if lock.is_some() {
            self.queue.remove_if_idle();
        }
        served
    }

    async fn serve(&self, lock: &mut Option<RelayLock>) -> crate::Result<()> {
        let started = Instant::now();
        let holder = loop {
            if let Some(holder) = self.holder() {
                break holder;
            }
            if started.elapsed() >= self.patience {
                return Ok(());
            }
            tokio::time::sleep(self.poll).await;
        };
        // Taken only once the session is confirmed, so a held lock always means a working relay.
        *lock = self.acquire().await?;
        if lock.is_none() {
            return Ok(());
        }
        while self.still_held(holder.pid) {
            for key in self.queue.pending()? {
                let now = now_millis();
                // The owner's fresh entry for another process stays for the daemon that queued
                // it, which claims it back; once expired it is dropped like any refused entry.
                if self.queue.peek(&key).is_ok_and(|message| {
                    message.owner && !message.expired(now) && !message.addressed_to(&holder)
                }) {
                    continue;
                }
                let Ok(Some(message)) = self.queue.claim(&key) else {
                    continue;
                };
                if let Some(reason) = message.refusal(&holder, now) {
                    debug_note(format_args!("dropped a queued message because {reason}"));
                    self.queue.finish(&key)?;
                    continue;
                }
                match send(
                    &self.credentials.socket,
                    Some(&self.credentials.token),
                    &self.session_id,
                    &message.msg_id,
                    &message.content,
                )
                .await
                {
                    Ok(()) => self.queue.finish(&key)?,
                    // The session stopped listening; the message waits for its next relay.
                    Err(SendError::NotDelivered(_)) => {
                        self.queue.release(&key)?;
                        return Ok(());
                    }
                    Err(SendError::Uncertain(_)) => {}
                }
            }
            tokio::time::sleep(self.poll).await;
        }
        Ok(())
    }
}

/// `agit hooks relay`: deliver this session's queued messages while its process runs it.
pub(crate) fn run_relay(session_id: &str) -> crate::Result<()> {
    let credentials =
        Credentials::from_env().context("Claude Code messaging credentials are absent")?;
    let relay = Relay {
        registry: crate::adapter::claude_code::sessions_dir()?,
        queue: Queue::open(Queue::path_for(session_id)?)?,
        session_id: session_id.to_owned(),
        credentials,
        poll: RELAY_POLL,
        patience: RELAY_PATIENCE,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(relay.run())
}

/// Whether a session in `permission_mode`, as its Stop hook reports the live mode, needs a
/// relay. Claude Code admits peer messages without approval in every other mode, and a hook that
/// reports no mode leaves the need unknown.
pub(crate) fn relay_serves(permission_mode: Option<&str>) -> bool {
    permission_mode == Some(BYPASS)
}

/// Start the relay of the session whose Stop hook runs now, unless one already serves it.
///
/// Best effort and immediate: the relay runs detached, and without one the daemon still delivers
/// with the peer token.
pub(crate) fn start_relay(session_id: &str) {
    let _ = try_start_relay(session_id);
}

fn try_start_relay(session_id: &str) -> crate::Result<()> {
    if !cfg!(unix) || Credentials::from_env().is_none() || !remote_control_configured() {
        return Ok(());
    }
    let queue = Queue::open(Queue::path_for(session_id)?)?;
    if queue.relay_active()? {
        return Ok(());
    }
    let mut command = crate::infra::background::command(std::env::current_exe()?);
    command
        .args(["hooks", "relay", "--session", session_id])
        .current_dir(Path::new("/"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and touches no state shared with the parent.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command.spawn()?;
    Ok(())
}

/// Workspace messages reach only a machine that bound a workspace for remote control, under
/// either namespace `rc_dir` selects.
fn remote_control_configured() -> bool {
    crate::infra::config::agit_home().is_ok_and(|home| {
        ["rc", "desktop-rc"]
            .iter()
            .any(|namespace| home.join(namespace).join(super::mirror::FILE).is_file())
    })
}

/// A Claude Code process fixture: a registration and a listening socket that reports every
/// connection's frames.
#[cfg(all(test, unix))]
pub(crate) mod fixture {
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::path::Path;

    pub(crate) fn register(
        registry: &Path,
        pid: u32,
        session_id: &str,
        socket: &Path,
        peer_token: Option<&str>,
    ) {
        std::fs::create_dir_all(registry).unwrap();
        let socket = socket.to_str().unwrap();
        std::fs::write(
            registry.join(format!("{pid}.json")),
            serde_json::json!({
                "pid": pid, "sessionId": session_id, "cwd": "/", "peerProtocol": 1,
                "kind": "interactive", "entrypoint": "claude-desktop",
                "messagingSocketPath": socket, "status": "idle",
            })
            .to_string(),
        )
        .unwrap();
        if let Some(token) = peer_token {
            let digest = hex::encode(Sha256::digest(socket.as_bytes()));
            std::fs::write(
                registry.join(format!("{pid}.{digest}.key")),
                serde_json::json!({"peerToken": token}).to_string(),
            )
            .unwrap();
        }
    }

    pub(crate) fn listen(socket: &Path) -> tokio::sync::mpsc::UnboundedReceiver<Vec<Value>> {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let (frames, received) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
                let lines = String::from_utf8(bytes)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                if frames.send(lines).is_err() {
                    return;
                }
            }
        });
        received
    }

    /// A relay of `session_id` that holds the child `token`, as `agit hooks relay` runs one.
    pub(crate) fn relay(
        registry: &Path,
        queue: &Path,
        session_id: &str,
        socket: &Path,
        token: &str,
    ) -> tokio::task::JoinHandle<crate::Result<()>> {
        let relay = super::Relay {
            registry: registry.to_owned(),
            queue: super::Queue::open(queue.to_owned()).unwrap(),
            session_id: session_id.to_owned(),
            credentials: super::Credentials {
                token: token.to_owned(),
                socket: socket.to_owned(),
            },
            poll: std::time::Duration::from_millis(20),
            patience: std::time::Duration::from_secs(5),
        };
        tokio::spawn(relay.run())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Only Claude Code's own records decide whether a transcript ran with checks. A substring
    /// scan would read a tool's input as the session's mode, or miss a spaced bypass record, and
    /// let a non-owner reach a session that ran without checks.
    #[test]
    fn transcript_mode_counts_only_claude_records() {
        let dir = tempfile::tempdir().unwrap();
        let verdict = |lines: &[serde_json::Value], raw: &str| {
            let path = dir.path().join("t.jsonl");
            let mut text: String = lines.iter().map(|line| format!("{line}\n")).collect();
            text.push_str(raw);
            std::fs::write(&path, text).unwrap();
            transcript_ran_unchecked(&path).unwrap()
        };
        let tool = serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "input": {"permissionMode": "default"}}]}});
        assert!(
            verdict(std::slice::from_ref(&tool), ""),
            "a tool's input is not the session's mode"
        );
        let checked = serde_json::json!({"type": "user", "permissionMode": "default"});
        assert!(!verdict(&[tool, checked.clone()], ""));
        assert!(
            verdict(
                std::slice::from_ref(&checked),
                "{\"type\": \"permission-mode\", \"permissionMode\": \"bypassPermissions\"}\n"
            ),
            "whitespace in a bypass record still counts"
        );
    }
    use serde_json::json;

    /// A relay authenticates with the token its session exported, so Claude Code admits the
    /// message as the session's own even in bypass-permissions mode. It sends only the owner's
    /// fresh entries for its own process: an entry without the owner mark or past its TTL is
    /// dropped unsent, and the owner's fresh entry for another process stays for the daemon that
    /// queued it. A relay that resent a claimed message, sent under the peer token, delivered an
    /// unmarked, stale or misaddressed entry, outlived its session, or left its idle queue behind
    /// fails here.
    #[tokio::test]
    async fn the_relay_sends_the_owners_fresh_messages_once_as_the_session_and_ends_with_it() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("sessions");
        let socket = root.path().join("claude.sock");
        let mut connections = fixture::listen(&socket);
        let mut claude = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        fixture::register(
            &registry,
            claude.id(),
            &session,
            &socket,
            Some("ffffffffffffffffffffffffffffffff"),
        );
        let live = discover_in(&registry, &session).unwrap();
        let dir = root.path().join("queue");
        let queue = Queue::open(dir.clone()).unwrap();
        queue
            .enqueue(
                &"ab".repeat(32),
                "message-1",
                "Please rerun CI.",
                &live,
                true,
            )
            .unwrap();
        let now = now_millis();
        let stale = now - u64::try_from(PENDING_TTL.as_millis()).unwrap() - 1000;
        let elsewhere = "03".repeat(32);
        for (key, entry) in [
            (
                "01".repeat(32),
                json!({"msg_id": "unmarked", "content": "Not the owner's.",
                    "enqueued_at": now, "pid": live.pid}),
            ),
            (
                "02".repeat(32),
                json!({"msg_id": "stale", "content": "Queued long ago.", "owner": true,
                    "enqueued_at": stale, "pid": live.pid}),
            ),
            (
                elsewhere.clone(),
                json!({"msg_id": "elsewhere", "content": "For another process.", "owner": true,
                    "enqueued_at": now, "pid": live.pid + 1}),
            ),
        ] {
            std::fs::write(queue.message(PENDING, &key), entry.to_string()).unwrap();
        }
        let token = "0123456789abcdef0123456789abcdef";
        let running = fixture::relay(&registry, &dir, &session, &socket, token);
        let frames = tokio::time::timeout(Duration::from_secs(5), connections.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frames,
            vec![
                json!({"type": "auth", "token": token}),
                json!({
                    "type": "user", "session_id": session, "msg_id": "message-1",
                    "from": "agit-rc", "priority": "next",
                    "message": {"role": "user", "content": "Please rerun CI."},
                }),
            ]
        );
        assert!(
            queue.relay_active().unwrap(),
            "a running relay holds the session lock"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while queue.pending().unwrap() != [elsewhere.clone()]
                || std::fs::read_dir(dir.join(SENDING)).unwrap().count() != 0
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the relay drops the unmarked and the stale entry and leaves the other process's");
        // The daemon that queued it claims it back once the hand-off ends.
        assert!(queue.claim(&elsewhere).unwrap().is_some());
        queue.finish(&elsewhere).unwrap();

        claude.kill().unwrap();
        claude.wait().unwrap();
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the relay ends with its session")
            .unwrap()
            .unwrap();
        assert!(!dir.exists(), "the relay removes its idle queue");
        assert!(
            connections.try_recv().is_err(),
            "only the owner's fresh message is sent, and once"
        );
    }
}
