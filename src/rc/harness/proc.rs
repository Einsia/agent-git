//! Shared child-process plumbing for the drivers.
//!
//! Both harnesses are NDJSON-over-stdio children. The things that are easy to
//! get wrong are the same for both, so they live here once:
//!
//! * **Own the process tree.** Unix uses `process_group(0)` and group signals;
//!   Windows creates the child suspended, attaches it to a kill-on-close Job,
//!   then resumes it. A harness spawns compilers, test runners, and servers, so
//!   killing only the direct child is never termination proof.
//! * **Drain stderr.** An un-read stderr pipe fills (~64 KiB) and the child
//!   blocks forever. We read it into a bounded tail and surface it on failure.
//! * **Bound the line length.** `BufReader::lines()` has no cap, so a runaway
//!   child can OOM the daemon. We cap and report instead.
//! * **Graceful then forceful.** Close stdin first, then use Unix group signals
//!   or Windows Job termination and wait for both child reap and tree exit.

use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

/// When a harness launch fails, the one fact besides the reason that must travel upward:
/// **whether this failure crossed the OS spawn**.
///
/// That line can only be drawn accurately here. Every layer above (`AnyDriver::launch`,
/// `Session::launch`, `Daemon::spawn_session`) sees nothing but an `anyhow::Error`, so "`claude`
/// is not on PATH at all" and "the process is already running with the handshake half written"
/// are forced into one outcome — and the two are accounted for in opposite ways: the first
/// proves that no process started, so the keyed `session.start` reservation must be released;
/// the second has an unknown result and can only leave a permanent tombstone (see
/// `daemon::SpawnFailure`).
///
/// So the answer comes from **the statement that actually calls `Command::spawn`**, and every
/// layer above propagates it unchanged.
#[derive(Debug)]
pub struct LaunchError {
    error: anyhow::Error,
    spawned: bool,
}

impl LaunchError {
    /// Provable that **no child process was created at all**.
    ///
    /// Only `Command::spawn` failing itself (the executable is not on PATH, permission denied,
    /// the cwd does not exist, fork failed) and "the process-tree fence cannot be built before
    /// spawn" belong in this class.
    pub fn not_spawned(error: anyhow::Error) -> Self {
        Self {
            error,
            spawned: false,
        }
    }

    /// The child **was created**, or whether it was created is itself unknown.
    ///
    /// Killing it afterwards belongs here too: the kill can fail as well, and as long as "a
    /// native harness may still be running on this machine" cannot be ruled out, this must not
    /// be accounted as nothing having happened.
    pub fn spawned(error: anyhow::Error) -> Self {
        Self {
            error,
            spawned: true,
        }
    }

    /// Did this failure cross the OS spawn? `false` appears only where it is provable that it
    /// did not.
    pub fn reached_spawn(&self) -> bool {
        self.spawned
    }
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl From<LaunchError> for anyhow::Error {
    fn from(failure: LaunchError) -> Self {
        failure.error
    }
}

// Test-only program-name stand-in: swaps `claude` / `codex` for a path that certainly exists
// (or certainly does not) on this machine.
//
// It exists so the **real** "the executable does not exist" failure path can be driven
// deterministically: `claude` is installed on a dev machine and absent on CI, so producing that
// failure out of the environment itself is not reproducible. The stand-in replaces only the
// binary on disk; `Command::spawn`, `Proc`, the driver, `Session` and daemon registration are all
// real. The whole block compiles out of production builds.
#[cfg(test)]
thread_local! {
    static PROGRAM_OVERRIDE: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Replace the harness program name with `program` on the current thread until the returned
/// guard is dropped.
///
/// The guard remembers the enclosing value and puts it back, so one test can swap twice — "not
/// installed" and then "installed" — and a panic unwind never leaves the stand-in behind for the
/// next test.
#[cfg(test)]
pub(crate) fn override_harness_program(program: impl Into<String>) -> HarnessProgramOverride {
    let previous = PROGRAM_OVERRIDE.with(|slot| slot.borrow_mut().replace(program.into()));
    HarnessProgramOverride(previous)
}

#[cfg(test)]
pub(crate) struct HarnessProgramOverride(Option<String>);

#[cfg(test)]
impl Drop for HarnessProgramOverride {
    fn drop(&mut self) {
        let previous = self.0.take();
        PROGRAM_OVERRIDE.with(|slot| *slot.borrow_mut() = previous);
    }
}

// Test-only: make the child produced by the **next** `Proc::spawn` fail once after the handshake
// line has been written.
//
// This constructs the failure on the **other side** of the materialization boundary:
// `Command::spawn` really ran, a process really was created on this machine, and only the
// handshake could not be proven to have succeeded. It has to keep being accounted as "crossed" —
// the `before_launch` side takes only paths that provably started nothing, and relaxing this one
// into it means the keyed `session.start` launches a second time while a native process may be
// running.
//
// It can only be armed here: the handshake write happens inside the driver, `Proc` has not been
// handed to any caller yet, and nothing outside can get a `&mut Proc` to call
// `fail_write_outcomes`. The armed value is taken (not read) in `Proc::spawn`, one-shot, so it
// never leaks into the next launch on the same thread. The whole block compiles out of
// production builds.
#[cfg(test)]
thread_local! {
    static LAUNCH_WRITE_FAILURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Make the first `count` `write_line` calls on the child from the next `Proc::spawn` fail
/// **after the write succeeded**. See [`Proc::fail_write_outcomes`]: this models the strongest
/// ambiguous-write case — the child may well have consumed the line while the caller receives no
/// success proof.
#[cfg(test)]
pub(crate) fn fail_next_launch_writes(count: usize) {
    LAUNCH_WRITE_FAILURES.with(|slot| slot.set(count));
}

/// The result of reading one line.
#[derive(Debug, PartialEq, Eq)]
pub enum Capped {
    /// The stream has ended.
    Eof,
    /// A whole line was read; `buf` holds it (without anything past the cap).
    Line,
    /// This line is over the cap. `buf` holds only its leading segment; the value is **how
    /// long the line actually is**.
    Overlong(usize),
}

/// Read one line with a **memory cap**.
///
/// # Why not `read_until`
///
/// `read_until` reads the whole line into a `Vec` **and only then** lets the caller check the
/// length — that check is always one step late: a child emitting bytes without a newline (a
/// runaway log, a binary file cat'd as text) has already made the daemon swallow the whole line
/// before it is "stopped". The cap is written down and the protection does not exist.
///
/// This reads and discards as it goes: past the cap the line is still read to its end (otherwise
/// the next read starts mid-line and every line after it is misaligned), but nothing more goes
/// into `buf`. Peak memory is therefore `cap`, whatever the length of that line.
async fn read_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Capped> {
    let mut total = 0usize;
    loop {
        let available = match reader.fill_buf().await {
            Ok(a) => a,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            // **A last line without a newline is judged against the cap all the same.**
            //
            // If this arm answered `Line` unconditionally: a child emits `cap + 1` bytes and
            // then closes stdout, and downstream gets the **prefix** truncated to `cap`,
            // forwarded on as an ordinary JSON/notice line — the cap is still there and the
            // guardrail is walked around through this gap. Truncated JSON is worse still: it
            // mostly fails to parse and becomes a notice sitting right at `cap`.
            return Ok(if total == 0 {
                Capped::Eof
            } else if total > cap {
                Capped::Overlong(total)
            } else {
                Capped::Line
            });
        }
        let (take, done) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        if buf.len() < cap {
            let room = cap - buf.len();
            buf.extend_from_slice(&available[..take.min(room)]);
        }
        total += take;
        reader.consume(take);
        if done {
            return Ok(if total > cap {
                Capped::Overlong(total)
            } else {
                Capped::Line
            });
        }
    }
}

/// Max bytes for one NDJSON line. Tool results can be large; beyond this the
/// line is dropped with a notice rather than buffered without limit.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// How many raw bytes stdout and stderr may queue in total while the consumer is paused.
///
/// An item-count cap bounds "many small messages" and not "every one of them sitting at
/// `MAX_LINE_BYTES`". This semaphore is shared by both pipes; once it is full the backpressure
/// travels back to the child instead of letting the daemon pile up to gigabytes.
pub const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

/// How many bytes of pushback one driver may accumulate while awaiting a native control
/// response.
///
/// A quarter of the whole budget (exactly one full `MAX_LINE_BYTES` line): retained lines charge
/// that same budget, and filling it wedges both reader tasks in `queue_line` (see [`Pushback`]),
/// so the response we are waiting for can never be read. The remaining three quarters are the
/// margin that keeps "the response itself can still be read in" true.
pub const PUSHBACK_MAX_BYTES: usize = MAX_PENDING_BYTES / 4;

/// How many items pushback may accumulate.
///
/// **A byte cap does not bound "many very small lines".** A non-empty stderr line takes as
/// little as one permit, yet every `Held` in the `VecDeque` also carries the `QueuedLine` itself,
/// its `OwnedSemaphorePermit` (an `Arc` clone) and allocation metadata — tens of bytes at least.
/// Admitted on `PUSHBACK_MAX_BYTES` alone, the smallest kind of line grows as many items as that
/// budget has bytes, real occupancy is tens of times their nominal byte count, and that is
/// enough to OOM the daemon. So the item count needs a cap of its own.
///
/// One control response waits at most `codex::STEER_RESPONSE_TIMEOUT`, and the lifecycle frames
/// that can actually **be retained** in that window (deltas are evicted first, see
/// [`Pushback::push`]) come nowhere near this magnitude; the container overhead for this many
/// items is hundreds of kilobytes, not hundreds of megabytes.
pub const PUSHBACK_MAX_ITEMS: usize = 4096;

/// Grace period between SIGTERM and SIGKILL on shutdown.
pub const SHUTDOWN_GRACE_MS: u64 = 3000;

/// After SIGKILL, still give init / launchd a short window to reap orphans. The whole driver
/// shutdown must stay inside the daemon's outer `FLEET_EXIT_GRACE`, so this cannot be an
/// unbounded `wait()`.
const SHUTDOWN_KILL_WAIT_MS: u64 = 1000;

pub struct Proc {
    // Keep this field declared before the Windows Job. Struct fields drop in
    // declaration order, and Job ActiveProcesses does not reach zero while a
    // process handle is still referenced.
    child: Child,
    pub stdin: Option<ChildStdin>,
    /// Parsed stdout plus surfaced stderr notices.
    lines: mpsc::Receiver<QueuedLine>,
    #[cfg(unix)]
    pgid: Option<i32>,
    /// Windows has no Unix-style process groups. Keep the Job alive for the
    /// whole harness generation so every descendant remains owned until
    /// shutdown has both reaped the direct child and observed an empty Job.
    #[cfg(windows)]
    job: crate::rc::windows_job::Job,
    #[cfg(test)]
    shutdown_failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Inject an error only after a full line was flushed. This models the
    /// strongest ambiguous-write case: the child definitely can consume the
    /// action while the caller receives no success proof.
    #[cfg(test)]
    write_outcome_failures: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only view of the same budget owned by both pipe readers.
    #[cfg(test)]
    byte_budget: std::sync::Arc<tokio::sync::Semaphore>,
}

pub(crate) struct QueuedLine {
    line: Line,
    /// How much byte budget this item charges. Equal to the permit count in `_bytes`.
    bytes: usize,
    /// While this item is still in the channel **or in a driver's pushback**, its raw bytes
    /// stay charged against the budget.
    _bytes: tokio::sync::OwnedSemaphorePermit,
}

impl QueuedLine {
    pub(crate) fn line(&self) -> &Line {
        &self.line
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// The caller is about to classify/return this line rather than retain it. Releasing its byte
    /// permit here is safe; moving it into another queue must keep the `QueuedLine` intact.
    pub(crate) fn into_line(self) -> Line {
        self.line
    }
}

/// Lines read while awaiting one native control response and not yet handed upward.
///
/// **This needs a byte cap of its own, and one well under `MAX_PENDING_BYTES`.** Lines held here
/// still hold the budget shared by both pipes (deliberately: otherwise the driver becomes an
/// unbounded side channel around backpressure), yet the loop awaiting the response **is itself
/// the only reader** — with the budget full, both reader tasks stop in `queue_line` on
/// `acquire_many_owned` and the response we are waiting for can never be read. A steer /
/// permission-mode switch then waits out `codex::STEER_RESPONSE_TIMEOUT` and ends as "delivery
/// unknown", so the supervisor treats an action that already took effect as an unknown result
/// and kills the whole harness process tree.
///
/// Past the cap, better to drop lines: a gap in the transcript, saying how many lines are
/// missing, beats the whole session being killed by mistake.
///
/// **But "which line to drop" cannot be picked by size.** More than losable deltas flow through
/// this queue: codex's `turn/completed` and its three `requestApproval` methods, claude-code's
/// `result` and `control_request` all pass here. One missing `turn/completed` and the supervisor
/// stays in Running forever; one missing approval request and it stays in AwaitingApproval while
/// the native side waits for our answer. So only the streaming delta / diagnostic lines that
/// [`rebuildable`] recognizes are evicted; a critical frame is either kept (evicting earlier
/// deltas in place to make room for it where necessary) or fail-stops the current generation
/// ([`Line::Fatal`]) — never disguised as an ordinary "the transcript is missing N lines".
pub(crate) struct Pushback {
    items: std::collections::VecDeque<PushbackItem>,
    bytes: usize,
    cap: usize,
    max_items: usize,
}

enum PushbackItem {
    Held(QueuedLine),
    /// A stretch of dropped output, counted by merging in place — otherwise "many lines were
    /// dropped" itself occupies memory.
    ///
    /// Rebuildable lines (`dropped`) and critical frames that could not be held (`lost`) are
    /// counted in the **same** item: with one marker kind per drop kind, alternating them grows
    /// the markers without bound and [`PUSHBACK_MAX_ITEMS`] bounds nothing. The difference shows
    /// in the [`Line`] that is released — `lost > 0` yields [`Line::Fatal`], otherwise just a
    /// notice saying how many lines are missing.
    Gap {
        dropped: usize,
        lost: usize,
    },
}

/// Can this line, once dropped, still be rebuilt from a later frame or from the transcript?
///
/// The test is an **allowlist**, and it admits two kinds only:
///
/// * `Line::Notice` — stderr lines, lines that cannot be parsed, and our own "a line was
///   dropped" notices. Both drivers turn it into `HarnessEvent::Notice` and the supervisor only
///   records it in tracing: dropping it loses a log line, not a state transition.
/// * Token-by-token streaming increments — codex's six delta notifications and claude-code's
///   `content_block_delta`. They all become `HarnessEvent::Delta`, and a Delta is losable by
///   design: the whole item in the transcript overwrites it afterwards.
///
/// Everything else counts as a critical frame: `turn/started` / `turn/completed`, approval
/// requests, JSON-RPC responses, claude's `result` / `control_request` / `user`... **an
/// unrecognized new method name also lands on this side** — that is exactly what the allowlist
/// is for: when the protocol grows a new lifecycle frame, the default is to keep it rather than
/// to drop it silently.
fn rebuildable(line: &Line) -> bool {
    match line {
        // EOF is the codex steer wait loop's proof that it can finish; Fatal is itself the
        // fail-stop marker.
        Line::Eof | Line::Fatal(_) => false,
        Line::Notice(_) => true,
        Line::Json(v) => {
            if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
                // codex. One carrying an `id` is a server request awaiting our answer
                // (approvals look exactly like this) and must not be dropped, whatever the
                // method name.
                return v.get("id").is_none()
                    && matches!(
                        method,
                        "item/agentMessage/delta"
                            | "item/reasoning/textDelta"
                            | "item/reasoning/summaryTextDelta"
                            | "item/plan/delta"
                            | "item/commandExecution/outputDelta"
                            | "item/fileChange/outputDelta"
                    );
            }
            // claude-code: the token-level content_block_delta only. content_block_start /
            // stop, also stream_event, open and close an item and are not in this list.
            v.get("type").and_then(|t| t.as_str()) == Some("stream_event")
                && v.get("event")
                    .and_then(|event| event.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("content_block_delta")
        }
    }
}

impl Pushback {
    pub(crate) fn new() -> Pushback {
        Pushback::with_cap(PUSHBACK_MAX_BYTES)
    }

    pub(crate) fn with_cap(cap: usize) -> Pushback {
        Pushback {
            items: std::collections::VecDeque::new(),
            bytes: 0,
            cap,
            max_items: PUSHBACK_MAX_ITEMS,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// How many items in the queue **actually hold a line of output**.
    ///
    /// This, not `len()`, is what pins the container size. `Gap` is a pair of counters merged in
    /// place; `Eof` is **not free** — the production path queues it as a `QueuedLine`
    /// (`queue_line(.., Line::Eof, 1)`) and it stays in the queue as a `Held`, holding its own
    /// permit. Excluding it here rests on another invariant: **only the stdout read loop queues
    /// `Line::Eof`** (stderr finishes on EOF and queues nothing), so the whole queue carries at
    /// most one EOF and the overhead is **a fixed O(1)** that does not grow with the number of
    /// output lines — what can grow with load and burst the daemon is only the non-EOF `Held`.
    /// **When** those extra entries appear in `len()` depends on when that EOF is inserted (it
    /// arrives at once while stdout is empty), which the scheduler decides; asserting on it
    /// yields a test that is green locally and red on CI.
    #[cfg(test)]
    pub(crate) fn held_len(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i, PushbackItem::Held(q) if !matches!(q.line(), Line::Eof)))
            .count()
    }

    /// Whether accumulating another `incoming` bytes breaks either cap.
    ///
    /// Both matter: the byte cap stops "big lines", the item cap stops "many small lines".
    fn over_cap(&self, incoming: usize) -> bool {
        self.bytes + incoming > self.cap || self.items.len() >= self.max_items
    }

    /// Retain one line. Past the cap it is dropped and **its byte budget is released at
    /// once**, so the reader keeps moving.
    pub(crate) fn push(&mut self, queued: QueuedLine) {
        let bytes = queued.bytes();
        let rebuildable = rebuildable(queued.line());
        // EOF is always kept: codex's steer wait loop takes its `Exited` finishing path from
        // the one in this queue, and dropping it leaves that loop to wait out the whole timeout
        // and report a clean "the process is gone" as delivery unknown. It takes one permit, and
        // nothing is worth evicting for it.
        if !matches!(queued.line(), Line::Eof) {
            // A critical frame is not dropped by size like the rest: the earliest rebuildable
            // delta / diagnostic lines in the queue are evicted first to free bytes and items
            // for it. Dropping a stretch of increments costs a few words in the transcript;
            // dropping a `turn/completed` or an approval request wedges the whole session.
            if !rebuildable {
                while self.over_cap(bytes) && self.evict_one_rebuildable() {}
            }
            // With nothing retained yet (including having just been emptied by eviction) the
            // line is kept regardless; otherwise a single line larger than the cap makes this
            // drop everything — the transcript disappears wholesale, and what is dropped is the
            // line that most needed to be seen.
            if self.bytes != 0 && self.over_cap(bytes) {
                drop(queued);
                // A critical frame reaching here does not fit even after every rebuildable
                // line was evicted. **This is not a transcript gap** — retaining more locks up
                // both pipes' byte budget and starves the loop awaiting the response, while
                // dropping it leaves the supervisor waiting forever for a lifecycle / approval
                // frame that never comes. The only honest ending is to fail-stop this
                // generation.
                self.record_gap(rebuildable);
                return;
            }
        }
        self.bytes += bytes;
        self.items.push_back(PushbackItem::Held(queued));
    }

    /// Evict the earliest rebuildable line in the queue and return its byte budget
    /// **immediately**. With nothing to evict this answers `false` — the caller turns that into
    /// a fail-stop.
    fn evict_one_rebuildable(&mut self) -> bool {
        let Some(idx) = self.items.iter().position(
            |item| matches!(item, PushbackItem::Held(queued) if rebuildable(queued.line())),
        ) else {
            return false;
        };
        let PushbackItem::Held(queued) = std::mem::replace(
            &mut self.items[idx],
            PushbackItem::Gap {
                dropped: 1,
                lost: 0,
            },
        ) else {
            unreachable!("position() just matched a held item");
        };
        self.bytes -= queued.bytes();
        drop(queued);
        self.merge_gap_at(idx);
        true
    }

    /// Merge with an adjacent gap in place. Eviction has to free an **item**, not bytes alone:
    /// without merging, "many lines were dropped" fills the item cap by itself.
    fn merge_gap_at(&mut self, idx: usize) {
        if matches!(self.items.get(idx + 1), Some(PushbackItem::Gap { .. }))
            && let Some(PushbackItem::Gap { dropped, lost }) = self.items.remove(idx + 1)
        {
            self.add_to_gap(idx, dropped, lost);
        }
        if idx > 0
            && matches!(self.items[idx - 1], PushbackItem::Gap { .. })
            && let Some(PushbackItem::Gap { dropped, lost }) = self.items.remove(idx)
        {
            self.add_to_gap(idx - 1, dropped, lost);
        }
    }

    fn add_to_gap(&mut self, idx: usize, dropped: usize, lost: usize) {
        if let PushbackItem::Gap {
            dropped: here,
            lost: here_lost,
        } = &mut self.items[idx]
        {
            *here += dropped;
            *here_lost += lost;
        }
    }

    /// Record one dropped line. When the back of the queue is already a gap it merges in and
    /// adds no item.
    fn record_gap(&mut self, rebuildable: bool) {
        let (dropped, lost) = if rebuildable { (1, 0) } else { (0, 1) };
        match self.items.back_mut() {
            Some(PushbackItem::Gap {
                dropped: here,
                lost: here_lost,
            }) => {
                *here += dropped;
                *here_lost += lost;
            }
            _ => self.items.push_back(PushbackItem::Gap { dropped, lost }),
        }
    }

    /// Release in the original order. A dropped line becomes a notice in its original
    /// position — a silently missing stretch of transcript leaves nobody able to tell "the
    /// model did not say it" from "we lost it". A critical frame that could not be held becomes a
    /// [`Line::Fatal`] in its position, so the driver stops this generation.
    pub(crate) fn pop_front(&mut self) -> Option<Line> {
        match self.items.pop_front()? {
            PushbackItem::Held(queued) => {
                self.bytes -= queued.bytes();
                Some(queued.into_line())
            }
            PushbackItem::Gap { dropped, lost: 0 } => Some(Line::Notice(format!(
                "dropped {dropped} harness output line(s) while waiting for a native control response"
            ))),
            // Once a critical frame is missing this gap is no longer a transcript problem: the
            // state machine no longer matches, so fail-stop is the only option.
            PushbackItem::Gap { dropped, lost } => Some(Line::Fatal(format!(
                "lost {lost} harness lifecycle/approval frame(s) (and dropped {dropped} \
                 rebuildable line(s)) while waiting for a native control response — the session \
                 state can no longer be reconstructed"
            ))),
        }
    }
}

impl Default for Pushback {
    fn default() -> Pushback {
        Pushback::new()
    }
}

#[derive(Debug, Clone)]
pub enum Line {
    Json(serde_json::Value),
    /// A line we could not parse, or a stderr line. Surfaced as a notice rather
    /// than swallowed: silent parse failures are how a driver ends up looking
    /// "hung" for reasons nobody can see.
    Notice(String),
    /// A **lifecycle / approval** frame is permanently lost (see [`Pushback`]) and this
    /// generation's state machine can no longer match the native side. The driver must translate
    /// it into `ProtocolInvariant`: the supervisor kills the whole harness process tree and
    /// finishes, instead of holding a session in Running that will never see its
    /// `turn/completed`.
    ///
    /// Deliberately a variant of `Line` rather than an ignorable flag: a new variant forces
    /// every `match` to handle it explicitly, so nobody forwards it as an ordinary notice in
    /// passing.
    Fatal(String),
    Eof,
}

/// The explanation left in place when an over-cap line is dropped. `pipe` is `output` /
/// `stderr`.
fn overlong_notice(pipe: &str, len: usize) -> String {
    format!(
        "dropped a {} MB {pipe} line (limit {} MB)",
        len / 1_048_576,
        MAX_LINE_BYTES / 1_048_576
    )
}

/// Decode one line of raw bytes into text **and guarantee the decoded form is still within the
/// per-line cap**; over the cap it reports the decoded length and the caller handles it as an
/// over-cap line.
///
/// # Why the cap applies again after decoding
///
/// [`read_capped`] caps the **raw** bytes, but on the way into the queue [`queue_line`] charges
/// the decoded length — that is the memory actually occupied. `from_utf8_lossy` replaces every
/// invalid byte with a U+FFFD, and a U+FFFD encodes to three times the width of one byte in
/// UTF-8: a full `MAX_LINE_BYTES` line of binary (a tool spilling binary onto the pipe is
/// ordinary) decodes to at most three times that, and one such line reserves three quarters of
/// `MAX_PENDING_BYTES`.
///
/// `MAX_LINE_BYTES` and `PUSHBACK_MAX_BYTES` then stop being caps on "how much one line takes",
/// and the margin [`Pushback`] rests on (a full pushback still leaves room for one full-length
/// response) is void on the spot: the driver holds one triple-width line, what remains of
/// `MAX_PENDING_BYTES` cannot hold a second, both reader tasks stop on `acquire_many_owned` and
/// no longer drain the pipes, and the control response being awaited can never be read — once
/// `codex::STEER_RESPONSE_TIMEOUT` runs out it ends as delivery unknown and the supervisor kills
/// a process tree that is in fact healthy.
///
/// So a line over the cap after decoding takes the same path as one over the cap in raw bytes:
/// dropped, with a note left saying how big it was.
fn decode_capped(buf: &[u8]) -> Result<std::borrow::Cow<'_, str>, usize> {
    let decoded = String::from_utf8_lossy(buf);
    let charged = decoded.trim().len();
    if charged > MAX_LINE_BYTES {
        return Err(charged);
    }
    Ok(decoded)
}

async fn queue_line(
    tx: &mpsc::Sender<QueuedLine>,
    budget: &std::sync::Arc<tokio::sync::Semaphore>,
    line: Line,
    bytes: usize,
) -> bool {
    let permits = u32::try_from(bytes.max(1)).expect("a capped line fits in u32 permits");
    let Ok(permit) = budget.clone().acquire_many_owned(permits).await else {
        return false;
    };
    tx.send(QueuedLine {
        line,
        bytes: permits as usize,
        _bytes: permit,
    })
    .await
    .is_ok()
}

impl Proc {
    /// Start a harness child process.
    ///
    /// Failure comes back as [`LaunchError`]: **this is the only place that knows whether the OS
    /// spawn happened**, and every layer above can only forward that fact unchanged.
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &PathBuf,
        env: &[(String, String)],
    ) -> Result<Proc, LaunchError> {
        #[cfg(test)]
        let stub = PROGRAM_OVERRIDE.with(|slot| slot.borrow().clone());
        #[cfg(test)]
        let program: &str = stub.as_deref().unwrap_or(program);
        // Taken, not read: one arming applies to one launch, and even where spawn itself fails
        // it does not leak into the next launch on the same thread.
        #[cfg(test)]
        let injected_write_failures = LAUNCH_WRITE_FAILURES.with(|slot| slot.replace(0));
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // The child leads its own process group so shutdown reaches its whole
        // tree (compilers, servers, test runners) and not just the harness.
        #[cfg(unix)]
        cmd.process_group(0);
        // A fence that cannot be built is **before spawn**: no process exists yet.
        #[cfg(windows)]
        let job = crate::rc::windows_job::Job::new().map_err(|e| {
            LaunchError::not_spawned(anyhow::anyhow!(
                "cannot create a process-tree Job for {program}: {e}"
            ))
        })?;
        #[cfg(windows)]
        crate::rc::windows_job::Job::configure(&mut cmd);

        // **This statement is the OS spawn boundary.** Its error means the kernel created no
        // process for us (ENOENT: not on PATH; EACCES: no execute bit; the cwd does not exist;
        // fork failed), so this launch provably did nothing.
        let mut child = cmd
            .spawn()
            .map_err(|e| LaunchError::not_spawned(anyhow::anyhow!("cannot start {program}: {e}\n  \u{2192} is it installed and on PATH? `which {program}`")))?;

        #[cfg(windows)]
        if let Err(error) = job.attach_and_resume(&child) {
            // The child was born suspended, so it cannot have launched an
            // unowned descendant. If assignment succeeded but resuming failed,
            // the Job owns it; otherwise kill_on_drop/direct kill owns the
            // still-suspended child. Never return a runnable unfenced harness.
            //
            // Accounted as having **crossed** spawn: a process really was created, and both
            // cleanups here swallow their errors with `let _ =` — "it must already be dead"
            // cannot be proven, so this cannot be accounted as "nothing happened".
            let _ = job.terminate();
            let _ = child.start_kill();
            return Err(LaunchError::spawned(anyhow::anyhow!(
                "cannot fence the {program} process tree before launch: {error}"
            )));
        }

        #[cfg(unix)]
        let pgid = child.id().map(|p| p as i32);
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        let (tx, rx) = mpsc::channel::<QueuedLine>(1024);
        let byte_budget = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_PENDING_BYTES));
        #[cfg(test)]
        let test_byte_budget = byte_budget.clone();

        // stdout → parsed lines
        let tx_out = tx.clone();
        let stdout_budget = byte_budget.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(64 * 1024, stdout);
            let mut buf = Vec::with_capacity(8192);
            loop {
                buf.clear();
                match read_capped(&mut reader, &mut buf, MAX_LINE_BYTES).await {
                    Ok(Capped::Eof) => break,
                    Ok(Capped::Overlong(n)) => {
                        let notice = overlong_notice("output", n);
                        let bytes = notice.len();
                        if !queue_line(&tx_out, &stdout_budget, Line::Notice(notice), bytes).await {
                            break;
                        }
                        continue;
                    }
                    Ok(Capped::Line) => {
                        let decoded = match decode_capped(&buf) {
                            Ok(decoded) => decoded,
                            Err(n) => {
                                let notice = overlong_notice("output", n);
                                let bytes = notice.len();
                                if !queue_line(&tx_out, &stdout_budget, Line::Notice(notice), bytes)
                                    .await
                                {
                                    break;
                                }
                                continue;
                            }
                        };
                        let s = decoded.trim();
                        if s.is_empty() {
                            continue;
                        }
                        let msg = match serde_json::from_str::<serde_json::Value>(s) {
                            Ok(v) => Line::Json(v),
                            Err(_) => Line::Notice(s.to_string()),
                        };
                        if !queue_line(&tx_out, &stdout_budget, msg, s.len()).await {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = queue_line(&tx_out, &stdout_budget, Line::Eof, 1).await;
        });

        // stderr → notices (and, critically, drained so the child never blocks)
        //
        // **The same capped read as stdout.** `BufReader::lines()` takes a whole line into
        // memory before handing it over, and the length of "a line" is the child's to decide: a
        // runaway (or compromised) harness only has to keep writing newline-free bytes to stderr
        // and the daemon grows with it until it OOMs.
        //
        // A cap that covers stdout alone is pointless — both pipes come from the same process,
        // and an attacker picks the uncapped one to write to.
        let tx_err = tx;
        let stderr_budget = byte_budget;
        tokio::spawn(async move {
            let mut reader = BufReader::with_capacity(16 * 1024, stderr);
            let mut buf = Vec::with_capacity(4096);
            loop {
                buf.clear();
                let line = match read_capped(&mut reader, &mut buf, MAX_LINE_BYTES).await {
                    Ok(Capped::Eof) | Err(_) => break,
                    Ok(Capped::Overlong(n)) => overlong_notice("stderr", n),
                    Ok(Capped::Line) => match decode_capped(&buf) {
                        Ok(decoded) => decoded.trim().to_string(),
                        Err(n) => overlong_notice("stderr", n),
                    },
                };
                if line.is_empty() {
                    continue;
                }
                let bytes = line.len();
                if !queue_line(&tx_err, &stderr_budget, Line::Notice(line), bytes).await {
                    break;
                }
            }
        });

        Ok(Proc {
            child,
            stdin,
            lines: rx,
            #[cfg(unix)]
            pgid,
            #[cfg(windows)]
            job,
            #[cfg(test)]
            shutdown_failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            write_outcome_failures: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(
                injected_write_failures,
            )),
            #[cfg(test)]
            byte_budget: test_byte_budget,
        })
    }

    /// Write one NDJSON line to the child.
    pub async fn write_line(&mut self, v: &serde_json::Value) -> crate::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            anyhow::bail!("the harness process is no longer accepting input (it exited)");
        };
        let mut s = serde_json::to_string(v)?;
        s.push('\n');
        stdin.write_all(s.as_bytes()).await?;
        stdin.flush().await?;
        #[cfg(test)]
        if self
            .write_outcome_failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            anyhow::bail!("injected ambiguous harness write outcome");
        }
        Ok(())
    }

    pub(crate) async fn next(&mut self) -> Option<QueuedLine> {
        self.lines.recv().await
    }

    #[cfg(test)]
    pub(crate) fn available_pending_bytes(&self) -> usize {
        self.byte_budget.available_permits()
    }

    /// Wait until the direct child **has been waited on** and the whole owned process tree is
    /// gone.
    ///
    /// Waiting for the child alone is not enough: the harness can exit first and leave a
    /// TERM-ignoring server in the same pgid; Tokio then returns that child's ExitStatus again
    /// at once, and a shutdown that waits on the child alone mistakes that for a finished
    /// cleanup. The deadline runs on the **real** clock, the pauses sleep on the **Tokio**
    /// clock — each side has its own test and the two cannot be swapped.
    ///
    /// The deadline: what is awaited is the kernel reaping a process tree, which happens only in
    /// real time. Kept on `tokio::time`, a paused clock advances virtual time to the next timer
    /// as soon as the runtime is idle, the whole grace period burns down in zero real time, and
    /// `ensure!(killed, ...)` then reports how busy the machine is rather than whether the tree
    /// was reaped.
    ///
    /// The pauses: this path ends in a SIGKILL escalation, so every trip has to be able to reach
    /// it. An async timer is the only wait here that cannot fail and still guarantees a real
    /// yield.
    ///
    /// The cost falls on the caller: this polling drives a paused virtual clock forward, so
    /// **tests holding a real child process must not pause the clock**, or a budget kept on the
    /// virtual clock in the same runtime burns through early.
    async fn wait_for_tree_exit(&mut self, within: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        loop {
            let child_reaped = matches!(self.child.try_wait(), Ok(Some(_)));
            #[cfg(unix)]
            let tree_gone = self.pgid.is_none_or(|pgid| !process_group_exists(pgid));
            #[cfg(windows)]
            let tree_gone = self.job.active_processes().is_ok_and(|active| active == 0);
            #[cfg(not(any(unix, windows)))]
            let tree_gone = true;

            if child_reaped && tree_gone {
                #[cfg(unix)]
                {
                    self.pgid = None;
                }
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// SIGTERM the group, wait, then SIGKILL. Closing stdin first lets a
    /// well-behaved harness exit on its own.
    async fn shutdown_with_grace(&mut self, grace: std::time::Duration) -> crate::Result<()> {
        self.stdin.take();
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
        }
        if self.wait_for_tree_exit(grace).await {
            return Ok(());
        }

        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        let _ = self.job.terminate();
        // On Unix this covers exec-before-group setup failures. On Windows it
        // is the direct-child fallback if TerminateJobObject failed.
        // `start_kill` is idempotent after exit/reap.
        let _ = self.child.start_kill();
        let killed = self
            .wait_for_tree_exit(std::time::Duration::from_millis(SHUTDOWN_KILL_WAIT_MS))
            .await;
        anyhow::ensure!(
            killed,
            "harness process tree did not disappear after forced termination"
        );
        Ok(())
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        #[cfg(test)]
        if self
            .shutdown_failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            anyhow::bail!("injected harness shutdown failure");
        }
        self.shutdown_with_grace(std::time::Duration::from_millis(SHUTDOWN_GRACE_MS))
            .await
    }

    #[cfg(test)]
    pub fn fail_shutdowns(
        &mut self,
        count: usize,
    ) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.shutdown_failures
            .store(count, std::sync::atomic::Ordering::SeqCst);
        self.shutdown_failures.clone()
    }

    #[cfg(test)]
    pub fn fail_write_outcomes(&mut self, count: usize) {
        self.write_outcome_failures
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    pub async fn wait(&mut self) -> Option<i32> {
        self.child.wait().await.ok().and_then(|s| s.code())
    }
}

#[cfg(unix)]
fn process_group_exists(pgid: i32) -> bool {
    if unsafe { libc::killpg(pgid, 0) } == 0 {
        return true;
    }
    // EPERM still proves that the group exists. Only ESRCH proves it is gone;
    // every other error therefore stays on the fail-closed side.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A paused clock must not shorten this budget.**
    ///
    /// This loop waits for the kernel to reap a process tree, which happens only in real time,
    /// while a runtime under a paused clock advances virtual time to the next timer as soon as
    /// it is idle. Keeping the deadline on `tokio::time` therefore burns the whole grace period
    /// in zero real time and `shutdown` reports "this tree did not disappear" while what it
    /// actually reports is how busy the machine was.
    ///
    /// The test lands on real elapsed time because that is exactly what gets stolen: the tree
    /// stays alive, the condition is never met, and this trip can only finish by spending the
    /// budget it asked for.
    #[cfg(unix)]
    #[test]
    fn a_paused_clock_does_not_shorten_the_wait_for_a_real_tree() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                let mut proc =
                    Proc::spawn("cat", &[], &std::env::current_dir().unwrap(), &[]).unwrap();
                let pgid = proc.pgid.expect("Unix child has a process group");
                let _guard = ProcessGroupGuard { pgid, armed: true };
                let want = std::time::Duration::from_millis(50);
                let started = std::time::Instant::now();
                // No signal is sent: the tree is alive, so this trip can only end by waiting
                // out the budget.
                let gone = proc.wait_for_tree_exit(want).await;
                let spent = started.elapsed();
                assert!(!gone, "a live tree must not be reported as gone");
                assert!(
                    spent >= want,
                    "the budget must be spent in real time, not virtual: asked {want:?}, spent {spent:?}"
                );
            });
    }

    #[cfg(unix)]
    struct ProcessGroupGuard {
        pgid: i32,
        armed: bool,
    }

    #[cfg(unix)]
    impl Drop for ProcessGroupGuard {
        fn drop(&mut self) {
            if self.armed {
                unsafe {
                    libc::killpg(self.pgid, libc::SIGKILL);
                }
            }
        }
    }

    /// A direct child exiting first does not mean its tree has exited. This background shell
    /// ignores TERM/HUP and the parent shell prints its pid and then ends immediately; an
    /// implementation that returns as soon as it sees the reaped parent never sends SIGKILL and
    /// leaves the test process on the machine.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_reaps_term_resistant_descendants_after_the_leader_exits() {
        let script = concat!(
            "(trap '' TERM HUP; while :; do /bin/sleep 10; done) ",
            "</dev/null >/dev/null 2>&1 & echo PID:$!; exit 0"
        );
        let mut proc = Proc::spawn(
            "/bin/sh",
            &["-c".into(), script.into()],
            &std::env::current_dir().expect("cwd"),
            &[],
        )
        .expect("spawn shell tree");
        let pgid = proc.pgid.expect("Unix child has a process group");
        let mut guard = ProcessGroupGuard { pgid, armed: true };

        let descendant = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(2), proc.next())
                .await
                .expect("child pid line")
                .expect("stdout")
                .into_line()
            {
                Line::Notice(line) => {
                    break line
                        .strip_prefix("PID:")
                        .expect("pid prefix")
                        .parse::<i32>()
                        .expect("background pid");
                }
                Line::Eof => panic!("stdout closed before the background pid arrived"),
                Line::Fatal(message) => panic!("unexpected fail-stop line: {message}"),
                Line::Json(_) => continue,
            }
        };
        assert_eq!(proc.wait().await, Some(0), "leader did not exit cleanly");
        assert!(
            process_group_exists(pgid),
            "test descendant exited too early"
        );
        assert_eq!(
            unsafe { libc::kill(descendant, 0) },
            0,
            "reported descendant is not alive"
        );

        proc.shutdown_with_grace(std::time::Duration::from_millis(50))
            .await
            .expect("shutdown process tree");
        assert!(
            !process_group_exists(pgid),
            "process group survived shutdown"
        );
        guard.armed = false;
    }

    /// Windows needs the same tree guarantee as Unix, but process groups do not
    /// exist there. This recursive test process launches a delayed-writing
    /// grandchild and proves Proc shutdown waits for both the direct harness and
    /// its Job to become empty before it can return success.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shutdown_reaps_the_harness_job_before_returning() {
        const TEST: &str =
            "rc::harness::proc::tests::windows_shutdown_reaps_the_harness_job_before_returning";
        const MODE: &str = "AGIT_WINDOWS_HARNESS_JOB_TEST_MODE";
        const READY: &str = "AGIT_WINDOWS_HARNESS_JOB_TEST_READY";
        const MARKER: &str = "AGIT_WINDOWS_HARNESS_JOB_TEST_MARKER";

        match std::env::var(MODE).as_deref() {
            Ok("grandchild") => {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                std::fs::write(std::env::var_os(MARKER).unwrap(), b"survived").unwrap();
                return;
            }
            Ok("parent") => {
                let mut descendant = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(MODE, "grandchild")
                    .spawn()
                    .unwrap();
                std::fs::write(
                    std::env::var_os(READY).unwrap(),
                    descendant.id().to_string(),
                )
                .unwrap();
                let _ = descendant.wait();
                return;
            }
            _ => {}
        }

        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("descendant-ready");
        let marker = dir.path().join("descendant-write");
        let executable = std::env::current_exe().unwrap();
        let mut proc = Proc::spawn(
            &executable.to_string_lossy(),
            &["--exact".into(), TEST.into(), "--nocapture".into()],
            &std::env::current_dir().expect("cwd"),
            &[
                (MODE.into(), "parent".into()),
                (READY.into(), ready.to_string_lossy().into_owned()),
                (MARKER.into(), marker.to_string_lossy().into_owned()),
            ],
        )
        .expect("spawn Job-owned harness tree");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "harness grandchild did not start"
            );
            assert!(
                !matches!(proc.child.try_wait(), Ok(Some(_))),
                "harness leader exited before its grandchild was ready"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        proc.shutdown_with_grace(std::time::Duration::from_millis(50))
            .await
            .expect("shutdown Job-owned harness tree");
        assert!(matches!(proc.child.try_wait(), Ok(Some(_))));
        assert_eq!(proc.job.active_processes().unwrap(), 0);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(
            !marker.exists(),
            "the harness grandchild survived shutdown and wrote after proof"
        );
    }

    /// Both pipes must contend for the same **byte** budget rather than each counting only
    /// channel items.
    #[tokio::test]
    async fn stdout_and_stderr_share_one_pending_byte_budget() {
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(8));
        let (stdout, mut rx) = mpsc::channel(8);
        let stderr = stdout.clone();

        assert!(queue_line(&stdout, &budget, Line::Notice("12345678".into()), 8).await);
        let waiting = tokio::spawn({
            let budget = budget.clone();
            async move { queue_line(&stderr, &budget, Line::Notice("x".into()), 1).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !waiting.is_finished(),
            "a second pipe bypassed the shared byte budget"
        );

        let first = rx.recv().await.expect("first queued line");
        assert!(
            !waiting.is_finished(),
            "receiving without releasing the queued item must keep its bytes charged"
        );
        drop(first);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .expect("the released byte budget should wake the waiter")
                .expect("waiter task panicked")
        );
    }

    /// **Lines retained in pushback must not lock up both pipes' byte budget.**
    ///
    /// The loop awaiting a native control response (`turn/steer`, `set_permission_mode`) is
    /// itself the only reader. Retained lines still hold the shared budget — filled to the top,
    /// the reader task stops in `queue_line` on `acquire_many_owned` and **the response we are
    /// waiting for can never be read**: once `codex::STEER_RESPONSE_TIMEOUT` runs out it ends as
    /// delivery unknown, the supervisor treats a steer that already took effect as an unknown
    /// result, and the whole harness process tree is killed.
    ///
    /// So past the cap the line is dropped and its bytes are returned at once.
    #[tokio::test]
    async fn pushback_over_its_cap_returns_bytes_so_the_awaited_response_still_arrives() {
        const BUDGET: usize = 8;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(BUDGET));
        let (tx, mut rx) = mpsc::channel(8);

        // Two lines arriving before the response fill the budget.
        assert!(queue_line(&tx, &budget, Line::Notice("aaaa".into()), 4).await);
        assert!(queue_line(&tx, &budget, Line::Notice("bbbb".into()), 4).await);
        assert_eq!(budget.available_permits(), 0);

        // The driver's wait loop reads them out and retains them (neither is the one awaited).
        let mut pushback = Pushback::with_cap(4);
        pushback.push(rx.recv().await.expect("first line"));
        pushback.push(rx.recv().await.expect("second line"));

        // The reader task now has to deliver the response being awaited.
        let response = tokio::spawn({
            let tx = tx.clone();
            let budget = budget.clone();
            async move { queue_line(&tx, &budget, Line::Notice("resp".into()), 4).await }
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), response)
                .await
                .expect("pushback held the whole byte budget — the response can never be read")
                .expect("reader task panicked"),
            "the awaited response was not queued"
        );
        assert!(matches!(
            rx.recv().await.map(|queued| queued.into_line()),
            Some(Line::Notice(text)) if text == "resp"
        ));

        // A dropped line must not vanish silently: a notice saying how many lines went missing
        // stays in its position.
        assert!(matches!(
            pushback.pop_front(),
            Some(Line::Notice(text)) if text == "aaaa"
        ));
        assert!(matches!(
            pushback.pop_front(),
            Some(Line::Notice(text)) if text.contains("dropped 1 harness output line")
        ));
        assert!(pushback.pop_front().is_none());
    }

    /// **EOF can never be dropped by a cap.**
    ///
    /// codex's `turn/steer` wait loop finishes at once on this EOF in pushback (the `Exited`
    /// path in `next_event`). Dropped as an ordinary line once the cap is full, that loop can
    /// only wait out `codex::STEER_RESPONSE_TIMEOUT` and report a clean "the process is already
    /// gone" as delivery unknown — the supervisor then handles a steer that delivered nothing as
    /// an unknown result.
    #[tokio::test]
    async fn pushback_never_drops_eof_even_after_its_cap_is_blown() {
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
        let (tx, mut rx) = mpsc::channel(8);
        assert!(queue_line(&tx, &budget, Line::Notice("aaaa".into()), 4).await);
        assert!(queue_line(&tx, &budget, Line::Notice("bbbb".into()), 4).await);
        assert!(queue_line(&tx, &budget, Line::Eof, 1).await);

        let mut pushback = Pushback::with_cap(4);
        pushback.push(rx.recv().await.expect("first line"));
        pushback.push(rx.recv().await.expect("second line"));
        pushback.push(rx.recv().await.expect("eof"));

        // Everything is replayed before asserting, rather than matched position by position:
        // how many lines ahead of the EOF are dropped is the cap's call, and matching positions
        // makes the failure that actually matters ("EOF was dropped") hit an earlier count
        // assertion first, so the red text speaks about how many lines were dropped instead of
        // "the finishing path is gone".
        let mut replayed = Vec::new();
        while let Some(line) = pushback.pop_front() {
            replayed.push(line);
        }
        assert!(matches!(replayed.first(), Some(Line::Notice(text)) if text == "aaaa"));
        assert!(
            replayed
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("dropped"))),
            "the dropped line vanished without a notice: {replayed:?}"
        );
        assert!(
            matches!(replayed.last(), Some(Line::Eof)),
            "EOF was dropped by the byte cap — the steer wait can no longer take its Exited path: {replayed:?}"
        );
    }

    /// A line within the cap is retained as usual and **still charges** the shared budget —
    /// otherwise the driver becomes an unbounded side channel around backpressure.
    #[tokio::test]
    async fn pushback_within_its_cap_keeps_charging_the_shared_byte_budget() {
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(64));
        let (tx, mut rx) = mpsc::channel(8);
        assert!(queue_line(&tx, &budget, Line::Notice("aaaa".into()), 4).await);

        let mut pushback = Pushback::with_cap(32);
        pushback.push(rx.recv().await.expect("queued line"));
        assert_eq!(pushback.len(), 1);
        assert_eq!(
            budget.available_permits(),
            60,
            "a line retained in pushback must still consume the pipe byte budget"
        );

        assert!(matches!(pushback.pop_front(), Some(Line::Notice(text)) if text == "aaaa"));
        assert_eq!(budget.available_permits(), 64);
    }

    /// One token-by-token increment notification from codex. Dropping it costs a few words in
    /// the transcript, and the whole item that follows overwrites them back.
    fn codex_delta(seq: usize) -> serde_json::Value {
        serde_json::json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "delta": "x".repeat(180) + &seq.to_string()
            }
        })
    }

    /// **A critical frame must not be dropped by the cap on size alone.**
    ///
    /// More than losable deltas flow through this queue: `turn/completed` and the three
    /// `requestApproval` methods take the same path. When a flood of increments arrives while a
    /// `turn/steer` response is awaited, dropping every line past the cap alike and leaving only
    /// a "dropped N lines" notice in its position means the supervisor never learns the turn
    /// ended and the session stays in Running; drop the approval request with it and the session
    /// stays in AwaitingApproval while the native side waits for our answer. Both can only be
    /// killed by hand.
    ///
    /// This runs on a real process: a child really is spawned to emit these frames on stdout,
    /// through the real reader task, the real byte semaphore and a real `Pushback`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_delta_flood_evicts_deltas_and_keeps_the_lifecycle_and_approval_frames() {
        let completed = serde_json::json!({
            "method": "turn/completed",
            "params": {"turn": {"id": "turn-1", "status": "completed"}}
        });
        let approval = serde_json::json!({
            "id": 7,
            "method": "item/commandExecution/requestApproval",
            "params": {"turnId": "turn-1", "itemId": "item-2", "command": "rm -rf /"}
        });
        let mut frames: Vec<serde_json::Value> = (1..=8).map(codex_delta).collect();
        frames.push(completed.clone());
        frames.push(approval.clone());
        // No single quote appears in these JSON frames, so wrapping them in single quotes for
        // sh is safe.
        let printf = frames
            .iter()
            .map(|frame| format!("'{frame}'"))
            .collect::<Vec<_>>()
            .join(" ");
        let mut proc = Proc::spawn(
            "/bin/sh",
            &["-c".into(), format!("printf '%s\\n' {printf}")],
            &std::env::current_dir().expect("cwd"),
            &[],
        )
        .expect("spawn the frame source");

        // The real cap is `PUSHBACK_MAX_BYTES`. Scaled down proportionally so the same branch
        // is reached without filling `MAX_PENDING_BYTES`: two deltas fill it, the remaining six
        // are over the cap, and the two critical frames only fit by evicting deltas.
        let cap = 3 * codex_delta(1).to_string().len();
        let mut pushback = Pushback::with_cap(cap);
        for _ in 0..frames.len() {
            let queued = tokio::time::timeout(std::time::Duration::from_secs(10), proc.next())
                .await
                .expect("the frame source must keep up")
                .expect("stdout closed early");
            pushback.push(queued);
        }

        let mut replayed = Vec::new();
        while let Some(line) = pushback.pop_front() {
            replayed.push(line);
        }
        assert!(
            replayed
                .iter()
                .any(|line| matches!(line, Line::Json(v) if *v == completed)),
            "turn/completed was evicted by the delta flood — the supervisor never learns the \
             turn ended and the session stays Running forever: {replayed:?}"
        );
        assert!(
            replayed
                .iter()
                .any(|line| matches!(line, Line::Json(v) if *v == approval)),
            "the approval request was evicted by the delta flood — the session stays \
             AwaitingApproval while codex waits for an answer that can never come: {replayed:?}"
        );
        assert!(
            !replayed.iter().any(|line| matches!(line, Line::Fatal(_))),
            "deltas were available to evict, so nothing justified failing the generation: \
             {replayed:?}"
        );
        assert!(
            replayed
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("dropped"))),
            "the evicted deltas vanished without a trace: {replayed:?}"
        );
        proc.shutdown().await.expect("stop the frame source");
    }

    /// When a critical frame does not fit even after every rebuildable line is evicted, the
    /// only option is **fail-stop**.
    ///
    /// This is the other half of the case above: disguised as a "the transcript is missing N
    /// lines" notice, the supervisor keeps running a session whose state machine no longer
    /// matches the native side. `Line::Fatal` makes it kill the tree and finish.
    #[tokio::test]
    async fn a_lifecycle_frame_that_cannot_be_held_fails_the_generation_instead() {
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(1024));
        let (tx, mut rx) = mpsc::channel(8);
        let completed = serde_json::json!({
            "method": "turn/completed",
            "params": {"turn": {"id": "turn-1", "status": "completed"}}
        })
        .to_string();
        for _ in 0..2 {
            assert!(
                queue_line(
                    &tx,
                    &budget,
                    Line::Json(serde_json::from_str(&completed).expect("valid frame")),
                    completed.len()
                )
                .await
            );
        }

        let mut pushback = Pushback::with_cap(1);
        pushback.push(rx.recv().await.expect("first lifecycle frame"));
        pushback.push(rx.recv().await.expect("second lifecycle frame"));

        let mut replayed = Vec::new();
        while let Some(line) = pushback.pop_front() {
            replayed.push(line);
        }
        assert!(
            matches!(replayed.last(), Some(Line::Fatal(text)) if text.contains("lifecycle")),
            "a lost lifecycle frame was disguised as an ordinary transcript gap — the supervisor \
             keeps running a session whose state no longer matches the native side: {replayed:?}"
        );
    }

    /// **The item count needs a cap too; bytes alone do not stop it.**
    ///
    /// A non-empty stderr line takes as little as one permit, yet the `VecDeque` still stores
    /// the `QueuedLine` itself, its `OwnedSemaphorePermit` and allocation metadata for every
    /// `Held`. Admitted on the `PUSHBACK_MAX_BYTES` byte budget alone, the smallest kind of line
    /// grows as many items as that budget has bytes — real occupancy is tens of times the
    /// nominal byte count, enough to OOM the daemon. So this pins **how many items the container
    /// holds**, not `bytes`.
    ///
    /// A real process, real pipes, a real `Pushback::new()` (the production cap, not a
    /// scaled-down one).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_flood_of_one_byte_stderr_lines_cannot_grow_the_pushback_container() {
        const LINES: usize = 20_000;
        let mut proc = Proc::spawn(
            "/bin/sh",
            &["-c".into(), format!("yes x | head -n {LINES} 1>&2")],
            &std::env::current_dir().expect("cwd"),
            &[],
        )
        .expect("spawn the stderr flood");

        let mut pushback = Pushback::new();
        let mut seen = 0usize;
        while seen < LINES {
            let queued = tokio::time::timeout(std::time::Duration::from_secs(30), proc.next())
                .await
                .expect("the stderr flood must keep flowing")
                .expect("both pipes closed before the flood finished");
            if matches!(queued.line(), Line::Notice(_)) {
                seen += 1;
            }
            pushback.push(queued);
        }

        // `LINES` smallest-possible lines come nowhere near `PUSHBACK_MAX_BYTES` — what stops
        // them has to be the item cap.
        //
        // What is pinned is **the number of items holding output**. `len()` carries a few extra
        // `Gap` counters and `Eof` markers, and when they appear depends on when stdout's EOF is
        // inserted: stdout is empty here, so its EOF arrives at once, lands at the back, and the
        // next dropped line after it has to open another gap. Whether the extra entries number
        // two or three is pure scheduling, and asserting on `len()` yields a test that is green
        // locally and red on CI. A Gap is a counter merged in place; EOF is not free — it holds
        // its own permit as a `Held` — but **only the stdout read loop queues an EOF** (stderr
        // finishes on EOF), so the whole queue carries at most one and the overhead is a fixed
        // O(1). What can grow with load and burst the daemon is only the non-EOF `Held`.
        assert!(
            pushback.held_len() <= PUSHBACK_MAX_ITEMS,
            "pushback holds {} lines from {LINES} one-byte stderr lines: the byte cap does not \
             bound the container, and eight million such entries OOM the daemon",
            pushback.held_len()
        );
        // This also pins "the extra entries can only be a constant number of
        // counters/markers" — otherwise the assertion above is escaped by turning `Held` into
        // some other shape. At most one EOF (only stdout queues it) plus a few gap counters;
        // `+ 5` is a deliberately loose upper bound, there to stop "extra entries grow with
        // load", not to count exactly.
        assert!(
            pushback.len() <= PUSHBACK_MAX_ITEMS + 5,
            "besides the held lines the container grew by {} entries — gap counters and EOF \
             markers must stay O(1)",
            pushback.len() - pushback.held_len()
        );
        // Dropped lines still leave a trace, merged into one counter — otherwise "many lines
        // were dropped" bursts the item count by itself.
        let mut replayed = Vec::new();
        while let Some(line) = pushback.pop_front() {
            replayed.push(line);
        }
        assert!(
            replayed
                .iter()
                .any(|line| matches!(line, Line::Notice(text) if text.contains("dropped"))),
            "the dropped stderr lines vanished without a trace"
        );
        proc.shutdown().await.expect("stop the stderr flood");
    }

    /// With the two drop kinds alternating, **the counter items themselves** must not grow
    /// either.
    ///
    /// With one marker kind each for rebuildable lines and for critical frames that cannot be
    /// held, an alternation like `delta, critical frame, delta, critical frame...` never merges
    /// at the back and the markers queue up one after another — the item cap is then walked
    /// around by the very growth it exists to stop.
    #[tokio::test]
    async fn alternating_drop_kinds_still_collapse_into_one_counter() {
        const ROUNDS: usize = 200;
        let budget = std::sync::Arc::new(tokio::sync::Semaphore::new(64 * 1024));
        let (tx, mut rx) = mpsc::channel(4);
        let completed = serde_json::json!({
            "method": "turn/completed",
            "params": {"turn": {"id": "turn-1", "status": "completed"}}
        });

        let mut pushback = Pushback::with_cap(1);
        for round in 0..=ROUNDS {
            for frame in [codex_delta(round), completed.clone()] {
                let bytes = frame.to_string().len();
                assert!(queue_line(&tx, &budget, Line::Json(frame), bytes).await);
                pushback.push(rx.recv().await.expect("queued frame"));
            }
        }

        // The steady state is `[gap, the retained critical frame, gap]` — independent of
        // `ROUNDS`.
        assert!(
            pushback.len() <= 4,
            "{} entries after {ROUNDS} rounds of alternating drop kinds: the merge counters grow \
             one per drop, so the entry cap bounds nothing",
            pushback.len()
        );
    }

    /// **The byte budget one line reserves must not exceed the per-line cap — on both pipes.**
    ///
    /// `read_capped` caps the **raw** bytes (`MAX_LINE_BYTES`), but what is counted on the way
    /// into the queue is the length after `String::from_utf8_lossy`: every invalid byte is
    /// replaced by a U+FFFD whose UTF-8 encoding is three times the width of one byte, so a
    /// full-length line of binary (a tool spilling binary onto stdout/stderr is ordinary) can
    /// reserve up to three times `MAX_LINE_BYTES`, while both pipes together only have
    /// `MAX_PENDING_BYTES`.
    ///
    /// The consequence is not "a little more memory" but that the margin [`Pushback`] rests on
    /// is void on the spot: `push` takes the first line unconditionally (the cap is judged only
    /// while `bytes != 0`), so a driver awaiting a response can hold one triple-width line and
    /// the remaining budget cannot hold another like it — both reader tasks stop in `queue_line`
    /// on `acquire_many_owned`, the pipes are no longer drained, and the `turn/steer` receipt
    /// being awaited can never be read. Once `codex::STEER_RESPONSE_TIMEOUT` runs out it ends as
    /// delivery unknown and the supervisor kills a harness process tree that is in fact healthy,
    /// while the steer arrived long ago.
    ///
    /// `the_production_pushback_cap_leaves_room_for_a_full_length_response` asserts arithmetic
    /// between constants (`PUSHBACK_MAX_BYTES` + `MAX_LINE_BYTES` still fits inside
    /// `MAX_PENDING_BYTES`) and stays green however far the accounted numbers inflate. So this
    /// takes **the permit count actually charged to that line**, through the production reader
    /// task.
    #[cfg(unix)]
    #[tokio::test]
    async fn one_line_never_reserves_more_than_the_line_cap() {
        // The raw bytes are within the cap; lossy decoding triples them and crosses it.
        const RAW: usize = MAX_LINE_BYTES / 2;
        let dir = tempfile::tempdir().expect("tempdir");
        let payload = dir.path().join("binary-line");
        let mut bytes = vec![0xFFu8; RAW];
        bytes.push(b'\n');
        std::fs::write(&payload, &bytes).expect("write the payload");
        let quoted = payload.to_string_lossy().into_owned();

        // stdout and stderr each decode at their **own** call site, so both are covered.
        for (pipe, redirect) in [("output", ""), ("stderr", " 1>&2")] {
            let mut proc = Proc::spawn(
                "/bin/sh",
                &["-c".into(), format!("exec cat '{quoted}'{redirect}")],
                &std::env::current_dir().expect("cwd"),
                &[],
            )
            .expect("spawn the binary-line writer");

            // The EOF of the empty pipe can arrive first (in the stderr round stdout closes
            // straight away).
            let queued = loop {
                let queued = tokio::time::timeout(std::time::Duration::from_secs(20), proc.next())
                    .await
                    .expect("the reader never delivered the line")
                    .expect("both pipes closed without delivering the line");
                if !matches!(queued.line(), Line::Eof) {
                    break queued;
                }
            };
            assert!(
                queued.bytes() <= MAX_LINE_BYTES,
                "one {RAW} B {pipe} line reserved {} B of the {MAX_PENDING_BYTES} B shared \
                 budget — past the {MAX_LINE_BYTES} B line cap, so a driver holding one in \
                 pushback starves the reader that must deliver the awaited response",
                queued.bytes()
            );
            // And it is not silently swallowed: the notice saying how big it was stays in its
            // position.
            let expected = format!("dropped a {} MB {pipe} line", RAW * 3 / 1_048_576);
            match queued.into_line() {
                Line::Notice(text) => assert!(
                    text.starts_with(&expected),
                    "the over-inflated {pipe} line vanished without a trace: {text:.120}"
                ),
                other => panic!("expected a drop notice on {pipe}, got {other:?}"),
            }
            proc.shutdown_with_grace(std::time::Duration::from_millis(50))
                .await
                .expect("stop the binary-line writer");
        }
    }

    /// **The production cap itself must leave room for a whole response.**
    ///
    /// The other cases all scale the cap down with `with_cap`; the `PUSHBACK_MAX_BYTES` that
    /// `Pushback::new()` uses is reached by none of them. Raise it to `MAX_PENDING_BYTES` and
    /// those cases all stay green while the deadlock comes back unchanged: filling pushback is
    /// filling the budget, the reader task stops in `queue_line`, and the steer /
    /// permission-mode response we are waiting for can never be read in.
    ///
    /// The bytes held are at most `max(cap, MAX_LINE_BYTES)` — within the cap they add up to
    /// `cap`, and the `bytes == 0` exception can hold one full-length line on top of that. What
    /// remains must at least fit one more full-length response.
    ///
    /// **This is arithmetic between constants only**: it holds on the premise that one line
    /// really does charge at most `MAX_LINE_BYTES` permits, and
    /// `one_line_never_reserves_more_than_the_line_cap` takes that premise on the production
    /// reader task. Without it, this stays green however far the accounting inflates.
    #[test]
    fn the_production_pushback_cap_leaves_room_for_a_full_length_response() {
        let worst_case_held = PUSHBACK_MAX_BYTES.max(MAX_LINE_BYTES);
        assert!(
            worst_case_held + MAX_LINE_BYTES <= MAX_PENDING_BYTES,
            "a full pushback ({worst_case_held} B) leaves {} B of the {MAX_PENDING_BYTES} B pipe \
             budget — not enough for one {MAX_LINE_BYTES} B response line, so the awaited native \
             receipt can never be read",
            MAX_PENDING_BYTES - worst_case_held
        );
    }

    /// **The over-cap verdict must hold for a last line without a newline too.**
    ///
    /// If the EOF arm answered `Line` unconditionally: the child emits `cap + 1` bytes and then
    /// closes the pipe, and downstream gets the **prefix** truncated to the cap, forwarded on as
    /// ordinary output — the cap is still there and the guardrail is walked around through this
    /// gap. The real shape is ordinary: a tool spilling binary as text, or a harness killed
    /// after running away.
    #[tokio::test]
    async fn an_overlong_last_line_without_a_newline_is_still_dropped() {
        const CAP: usize = 64;
        let data = [b'x'; CAP + 1]; // no trailing \n
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_capped(&mut reader, &mut buf, CAP).await.expect("read"),
            Capped::Overlong(CAP + 1),
            "a truncated prefix must not be handed over as a whole line"
        );
        assert!(buf.len() <= CAP, "peak memory must still be the cap");
        // The stream really has ended after it.
        buf.clear();
        assert_eq!(
            read_capped(&mut reader, &mut buf, CAP).await.expect("read"),
            Capped::Eof
        );
    }

    /// A last line within the cap (with no newline) is handed over as usual — the case above
    /// must not catch it too.
    #[tokio::test]
    async fn a_short_last_line_without_a_newline_still_arrives() {
        let data = b"hello".to_vec();
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_capped(&mut reader, &mut buf, 64).await.expect("read"),
            Capped::Line
        );
        assert_eq!(buf, b"hello");
    }

    /// **The cap takes effect before the read, not after it.**
    ///
    /// `read_until` reads the whole line into memory **and only then** lets the caller check the
    /// length — a child emitting bytes without a newline (a runaway log, a binary file cat'd)
    /// has already made the daemon swallow the whole line before it is "stopped". This drives one
    /// line far past the cap and pins **the peak buffer**.
    #[tokio::test]
    async fn an_overlong_line_never_lands_in_memory() {
        const CAP: usize = 64;
        // One line far past `CAP`, followed by an ordinary one.
        let mut data = vec![b'x'; 100 * 1024];
        data.push(b'\n');
        data.extend_from_slice(b"{\"ok\":true}\n");

        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();

        let got = read_capped(&mut reader, &mut buf, CAP).await.expect("read");
        assert_eq!(
            got,
            Capped::Overlong(100 * 1024 + 1),
            "the reported length is the real length"
        );
        assert!(
            buf.len() <= CAP,
            "the buffer swallowed {} bytes — the cap did nothing",
            buf.len()
        );

        // **And the stream is not misaligned**: the overlong line is read to its end and the
        // next line arrives as usual.
        buf.clear();
        let got = read_capped(&mut reader, &mut buf, CAP).await.expect("read");
        assert_eq!(got, Capped::Line);
        assert_eq!(String::from_utf8_lossy(&buf).trim(), "{\"ok\":true}");

        buf.clear();
        assert_eq!(
            read_capped(&mut reader, &mut buf, CAP).await.expect("read"),
            Capped::Eof
        );
    }

    /// A last line with no newline is handed over too, not swallowed as EOF.
    #[tokio::test]
    async fn a_final_line_without_a_newline_is_still_a_line() {
        let data = b"{\"a\":1}\n{\"b\":2}";
        let mut reader = tokio::io::BufReader::new(&data[..]);
        let mut buf = Vec::new();

        assert_eq!(
            read_capped(&mut reader, &mut buf, 1024)
                .await
                .expect("read"),
            Capped::Line
        );
        buf.clear();
        assert_eq!(
            read_capped(&mut reader, &mut buf, 1024)
                .await
                .expect("read"),
            Capped::Line
        );
        assert_eq!(String::from_utf8_lossy(&buf), "{\"b\":2}");
    }
}
