//! Business callers receive content even when the isolated privacy worker is unavailable.

use super::projector::{MAX_UNIT_BYTES, Mode, Outcome, Reason, Status};
use super::worker::{self, Request};
use anyhow::{Context, ensure};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const LIVE_BUDGET: Duration = Duration::from_millis(750);
const DURABLE_BASE: Duration = Duration::from_secs(5);
const DURABLE_PER_MIB: Duration = Duration::from_secs(1);
const DURABLE_CAP: Duration = Duration::from_secs(60);
/// How long a new worker may take to load a repository's dictionary and policy. It is spent
/// before, and apart from, any caller's budget.
const STARTUP_DEADLINE: Duration = Duration::from_secs(10);
const RETRY_DELAY: Duration = Duration::from_secs(10);
/// How long a caller can go without an answer through ordinary trouble: a worker back-off and
/// one budget.
pub(crate) const RETRY_WINDOW: Duration =
    Duration::from_millis((RETRY_DELAY.as_millis() + LIVE_BUDGET.as_millis()) as u64);
const BATCH_BYTES: usize = 128 * 1024;

/// How long a caller lets the worker take, from the moment the worker is ready.
#[derive(Clone, Copy)]
pub(crate) enum Budget {
    /// Remote-control text a viewer is waiting for.
    Live,
    /// Saving, importing, pushing, memory and share or export: seconds, more for more bytes.
    Durable,
}

impl Budget {
    fn for_input(self, bytes: usize) -> Duration {
        match self {
            Self::Live => LIVE_BUDGET,
            Self::Durable => {
                let mib = u32::try_from(bytes.div_ceil(1024 * 1024)).unwrap_or(u32::MAX);
                DURABLE_BASE
                    .saturating_add(DURABLE_PER_MIB.saturating_mul(mib))
                    .min(DURABLE_CAP)
            }
        }
    }
}

/// Why one request to the worker produced no reply for its caller.
#[derive(Debug, thiserror::Error)]
enum Failure {
    /// An earlier request is still in flight.
    #[error("privacy worker is busy")]
    Busy,
    /// The caller's deadline passed; the worker keeps the request and its late reply is discarded.
    #[error("privacy worker deadline expired")]
    Deadline,
    /// The reply was missing or malformed; the worker cannot be trusted with another request.
    #[error("privacy worker failed: {0:#}")]
    Protocol(anyhow::Error),
}

struct Process {
    child: Child,
    #[cfg(windows)]
    job: Option<crate::rc::windows_job::Job>,
    send: mpsc::SyncSender<Zeroizing<Vec<u8>>>,
    receive: mpsc::Receiver<crate::Result<worker::Response>>,
    io: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
    /// The repository the worker last loaded its dictionary and policy for.
    prepared: Option<Option<std::path::PathBuf>>,
}

/// The executable that hosts the worker: this one. A debug build lets a test that runs privacy
/// code inside its own test binary, which cannot host the worker, name the agit executable
/// instead; a release build ignores the variable.
fn worker_executable() -> std::io::Result<std::path::PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("AGIT_TEST_PRIVACY_WORKER") {
        return Ok(path.into());
    }
    std::env::current_exe()
}

impl Process {
    fn start() -> crate::Result<Self> {
        let mut command = Command::new(worker_executable()?);
        command.arg(worker::WORKER);
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> crate::Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        let job = {
            let job = crate::rc::windows_job::Job::new()?;
            job.limit_memory(1024 * 1024 * 1024)?;
            crate::rc::windows_job::Job::configure_std(&mut command);
            job
        };
        let mut child = command.spawn()?;
        #[cfg(windows)]
        if let Err(error) = job.attach_std(&child) {
            let _ = child.kill();
            let _ = job.terminate_and_close();
            return Err(error.into());
        }
        let mut input = child.stdin.take().context("privacy input is unavailable")?;
        let mut output = child
            .stdout
            .take()
            .context("privacy output is unavailable")?;
        let (send, requests) = mpsc::sync_channel::<Zeroizing<Vec<u8>>>(1);
        let (responses, receive) = mpsc::sync_channel(1);
        let io = std::thread::Builder::new()
            .name("privacy-io".into())
            .spawn(move || {
                #[cfg(unix)]
                unsafe {
                    let mut blocked = std::mem::zeroed();
                    libc::sigemptyset(&mut blocked);
                    libc::sigaddset(&mut blocked, libc::SIGPIPE);
                    libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
                }
                while let Ok(bytes) = requests.recv() {
                    let result = (|| -> crate::Result<worker::Response> {
                        worker::write_frame(&mut input, &bytes)?;
                        let response = worker::read_frame(&mut output)?;
                        let response: worker::Response = serde_json::from_slice(&response)?;
                        ensure!(
                            response.outcome.content.len() <= MAX_UNIT_BYTES + 1024 * 1024,
                            "privacy output exceeds its byte budget"
                        );
                        Ok(response)
                    })();
                    let failed = result.is_err();
                    if responses.send(result).is_err() || failed {
                        break;
                    }
                }
                drop(input);
                let _ = std::io::copy(&mut output, &mut std::io::sink());
            });
        let io = match io {
            Ok(io) => io,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                #[cfg(windows)]
                let _ = job.terminate_and_close();
                return Err(error.into());
            }
        };
        Ok(Self {
            child,
            #[cfg(windows)]
            job: Some(job),
            send,
            receive,
            io: Some(io),
            next_id: 0,
            prepared: None,
        })
    }

    fn request(
        &mut self,
        repo: Option<&Path>,
        context: &str,
        text: &str,
        mode: Mode,
        remaining: Duration,
    ) -> Result<Outcome, Failure> {
        // A reply that arrived after its caller stopped waiting belongs to no one now.
        while let Ok(stale) = self.receive.try_recv() {
            if let Err(error) = stale {
                return Err(Failure::Protocol(error));
            }
        }
        self.next_id += 1;
        let id = self.next_id;
        let request = Request {
            id,
            deadline_ms: u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX),
            repo: repo.map(Path::to_owned),
            mode,
            text: text.to_owned(),
            context: context.to_owned(),
        };
        let bytes =
            Zeroizing::new(serde_json::to_vec(&request).map_err(|e| Failure::Protocol(e.into()))?);
        self.send.try_send(bytes).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Failure::Busy,
            mpsc::TrySendError::Disconnected(_) => {
                Failure::Protocol(anyhow::anyhow!("privacy worker is gone"))
            }
        })?;
        let started = Instant::now();
        loop {
            match self
                .receive
                .recv_timeout(remaining.saturating_sub(started.elapsed()))
            {
                Ok(Ok(response)) if response.id == id => return Ok(response.outcome),
                Ok(Ok(_)) => continue,
                Ok(Err(error)) => return Err(Failure::Protocol(error)),
                Err(mpsc::RecvTimeoutError::Timeout) => return Err(Failure::Deadline),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(Failure::Protocol(anyhow::anyhow!("privacy worker is gone")));
                }
            }
        }
    }

    fn terminate(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            let _ = job.terminate_and_close();
        }
        // A reaped worker's id may already name another process group.
        #[cfg(unix)]
        if matches!(self.child.try_wait(), Ok(None)) {
            unsafe {
                libc::killpg(self.child.id() as i32, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
        let (closed, receiver) = mpsc::sync_channel(0);
        drop(receiver);
        self.send = closed;
    }

    /// Ends the worker's input and waits for it to exit, which closes its journal. An idle worker
    /// exits as soon as its input ends. One still working on a request whose caller gave up gets
    /// the least time the watchdog gives any request and is then killed: nobody reads that
    /// result, and SQLite recovers whatever the kill interrupts.
    fn close(&mut self) {
        let until = Instant::now() + worker::WATCHDOG_FLOOR;
        let (closed, receiver) = mpsc::sync_channel(0);
        drop(receiver);
        self.send = closed;
        // The io thread's replies end once the worker's output does, when the worker exits.
        loop {
            match self
                .receive
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.terminate();
                    break;
                }
            }
        }
        let _ = self.child.wait();
    }

    fn reaped(&mut self) -> bool {
        let process_done = matches!(self.child.try_wait(), Ok(Some(_)));
        let io_done = self.io.as_ref().is_none_or(|io| io.is_finished());
        if io_done && let Some(io) = self.io.take() {
            let _ = io.join();
        }
        process_done && io_done
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Default)]
struct State {
    process: Option<Process>,
    failed_until: Option<Instant>,
}

/// Wait until a worker that failed may be started again, at most one back-off.
pub(crate) fn wait_for_retry() {
    let until = shared().lock().ok().and_then(|state| state.failed_until);
    if let Some(until) = until {
        std::thread::sleep(
            until
                .saturating_duration_since(Instant::now())
                .min(RETRY_DELAY),
        );
    }
}

fn shared() -> &'static Mutex<State> {
    static STATE: Mutex<State> = Mutex::new(State {
        process: None,
        failed_until: None,
    });
    &STATE
}

/// Explicit publication needs one frozen dictionary and complete results, not foreground fallback.
pub(crate) struct PublicationHydrator {
    process: Process,
    repo: std::path::PathBuf,
    started: Instant,
}

impl PublicationHydrator {
    pub fn new(repo: &Path, preserve_protocol_ids: bool) -> crate::Result<Self> {
        let mut session = Self {
            process: Process::start()?,
            repo: repo.to_owned(),
            started: Instant::now(),
        };
        let frozen = session.request(
            if preserve_protocol_ids {
                "true"
            } else {
                "false"
            },
            Mode::FreezeHydration,
        )?;
        ensure!(
            frozen.content == "true",
            "publication dictionary could not be frozen"
        );
        Ok(session)
    }

    fn request(&mut self, text: &str, mode: Mode) -> crate::Result<Outcome> {
        let remaining = Duration::from_secs(120)
            .saturating_sub(self.started.elapsed())
            .min(Duration::from_secs(25));
        ensure!(
            !remaining.is_zero(),
            "publication hydration timed out; retry before publishing"
        );
        let outcome = self
            .process
            .request(Some(&self.repo), "", text, mode, remaining)
            .map_err(anyhow::Error::new)
            .context("publication hydration failed; restore the source dictionary or retry before publishing")?;
        ensure!(
            outcome.status == Status::Complete && outcome.unresolved == 0,
            "publication hydration is incomplete; restore the source dictionary or unlock the original publication"
        );
        Ok(outcome)
    }

    pub fn jsonl(&mut self, text: &str) -> crate::Result<Zeroizing<String>> {
        let mut output = Zeroizing::new(String::new());
        let mut cursor = 0;
        while cursor < text.len() {
            let mut end = cursor;
            for line in text[cursor..].split_inclusive('\n') {
                if end > cursor && end + line.len() - cursor > BATCH_BYTES {
                    break;
                }
                end += line.len();
                if end - cursor >= BATCH_BYTES {
                    break;
                }
            }
            ensure!(
                end - cursor <= MAX_UNIT_BYTES,
                "publication event exceeds the hydration byte budget"
            );
            let outcome = self.request(&text[cursor..end], Mode::HydrateFrozenJsonl)?;
            validate(&text[cursor..end], &outcome, Mode::HydrateJsonl)?;
            crate::domain::privacy_layer::PrivateLayer::check_session_size(
                output
                    .len()
                    .checked_add(outcome.content.len())
                    .context("publication hydration size overflow")?,
                0,
            )?;
            output.push_str(&outcome.content);
            cursor = end;
        }
        Ok(output)
    }

    pub fn metadata(&mut self, metadata: &mut crate::domain::meta::Meta) -> crate::Result<()> {
        let fields = observation_fields(metadata);
        let input = Zeroizing::new(serde_json::to_string(&fields)?);
        let hydrated = self.jsonl(&input)?;
        let values: Vec<String> = serde_json::from_str(&hydrated)?;
        ensure!(
            fields.len() == values.len(),
            "publication hydration changed metadata fields"
        );
        for (field, value) in fields.into_iter().zip(values) {
            *field = value;
        }
        Ok(())
    }

    pub fn used_values(&mut self) -> crate::Result<std::collections::BTreeSet<String>> {
        let values = Zeroizing::new(self.request("", Mode::HydrationValues)?.content);
        Ok(serde_json::from_str(&values)?)
    }
}

impl Drop for PublicationHydrator {
    fn drop(&mut self) {
        self.process.terminate();
        let _ = self.process.child.wait();
    }
}

/// Whether `outcome` passed protection in full, so that its content may be committed or
/// published. A unit-test binary is not the agit executable and cannot host the worker, so there
/// an unprocessed outcome also counts and unit tests keep exercising their own logic; the
/// integration tests drive the real worker, where only a complete outcome counts.
pub fn passed(outcome: &Outcome) -> bool {
    (outcome.status == Status::Complete && outcome.unprocessed == 0)
        || (cfg!(test) && outcome.status == Status::Skipped)
}

/// The leading part of `outcome.content` that the worker processed, and how many input bytes it
/// stands for. The rest of the content, if any, is the input unchanged and must not be committed
/// or published. As in [`passed`], a unit-test binary counts an unprocessed outcome as processed.
pub fn processed<'a>(outcome: &'a Outcome, input: &str) -> (&'a str, usize) {
    if cfg!(test) && outcome.status == Status::Skipped {
        return (&outcome.content, input.len());
    }
    let unprocessed = if outcome.status == Status::Skipped {
        input.len()
    } else {
        outcome.unprocessed.min(input.len())
    };
    let content = outcome.content.len().saturating_sub(unprocessed);
    (&outcome.content[..content], input.len() - unprocessed)
}

/// Content the privacy worker did not protect in full is not committed; the operation waits and
/// can be retried.
#[derive(Debug)]
pub struct Pending {
    pub reason: Option<Reason>,
}

impl Pending {
    pub fn of(outcome: &Outcome) -> Self {
        Self {
            reason: outcome.reason,
        }
    }
}

impl std::fmt::Display for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "privacy check pending ({}); nothing was committed, and the next attempt tries again",
            reason_name(self.reason)
        )
    }
}

impl std::error::Error for Pending {}

/// `text` protected for a commit, or [`Pending`] while it has not passed protection in full.
pub fn protect_for_commit(repo: Option<&Path>, text: &str, mode: Mode) -> Result<String, Pending> {
    let outcome = transform(repo, text, mode);
    if !passed(&outcome) {
        return Err(Pending::of(&outcome));
    }
    Ok(outcome.content)
}

/// The name a reason is reported and counted under.
pub fn reason_name(reason: Option<Reason>) -> &'static str {
    match reason {
        Some(Reason::Deadline) => "deadline",
        Some(Reason::LockBusy) => "lock_busy",
        Some(Reason::Backoff) => "backoff",
        Some(Reason::SpawnFailed) => "spawn_failed",
        Some(Reason::ProtocolError) => "protocol_error",
        Some(Reason::Oversize) => "oversize",
        Some(Reason::StoreBusy) => "store_busy",
        Some(Reason::SourceIncomplete) => "source_incomplete",
        Some(Reason::FindingsCap) => "findings_cap",
        Some(Reason::DeviceRules) => "device_rules",
        Some(Reason::LegacyRules) => "legacy_rules",
        None => "unknown",
    }
}

fn skipped(text: &str, reason: Reason) -> Outcome {
    let mut outcome = Outcome::skipped(text);
    outcome.reason = Some(reason);
    outcome
}

/// Protection for content that is saved, imported, pushed, collected into memory, shared or
/// exported: a durable budget that grows with the input.
pub fn transform(repo: Option<&Path>, text: &str, mode: Mode) -> Outcome {
    transform_with(repo, "", text, mode, Budget::Durable)
}

/// Protection for remote-control text a viewer is waiting for: the live budget.
pub(crate) fn transform_live(repo: Option<&Path>, text: &str, mode: Mode) -> Outcome {
    transform_with(repo, "", text, mode, Budget::Live)
}

/// [`transform_live`] for text that continues `context`, text a stream already released: the
/// worker's rules match across the context, and only `text` is protected and returned.
pub fn transform_in(repo: Option<&Path>, context: &str, text: &str, mode: Mode) -> Outcome {
    transform_with(repo, context, text, mode, Budget::Live)
}

fn transform_with(
    repo: Option<&Path>,
    context: &str,
    text: &str,
    mode: Mode,
    budget: Budget,
) -> Outcome {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        context
            .is_empty()
            .then(|| worker::inline(repo, text, mode))
            .flatten()
            .unwrap_or_else(|| transform_inner(repo, context, text, mode, budget))
    }))
    .unwrap_or_else(|_| skipped(text, Reason::ProtocolError));
    if outcome.status != Status::Complete || outcome.unprocessed > 0 {
        crate::telemetry::observe(crate::telemetry::Observation::PrivacyPending {
            budget: match budget {
                Budget::Live => "live",
                Budget::Durable => "durable",
            },
            reason: reason_name(outcome.reason),
        });
    }
    outcome
}

fn transform_inner(
    repo: Option<&Path>,
    context: &str,
    text: &str,
    mode: Mode,
    budget: Budget,
) -> Outcome {
    if matches!(
        mode,
        Mode::HydrateText | Mode::HydrateJsonl | Mode::HydrateEnvelopes
    ) && !text.contains("{{AGIT_SECRET_")
    {
        return Outcome {
            content: text.into(),
            status: Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            reason: None,
        };
    }
    let Ok(mut state) = shared().try_lock() else {
        return skipped(text, Reason::LockBusy);
    };
    if let Some(until) = state.failed_until {
        if Instant::now() < until {
            return skipped(text, Reason::Backoff);
        }
        if state
            .process
            .as_mut()
            .is_some_and(|process| !process.reaped())
        {
            return skipped(text, Reason::Backoff);
        }
        state.process = None;
        state.failed_until = None;
    }
    let mut spawn_ms = 0;
    if state.process.is_none() {
        let spawning = Instant::now();
        match Process::start() {
            Ok(process) => state.process = Some(process),
            Err(_) => {
                state.failed_until = Some(Instant::now() + RETRY_DELAY);
                return skipped(text, Reason::SpawnFailed);
            }
        }
        spawn_ms = spawning.elapsed().as_millis();
    }
    // Loading the repository's dictionary and policy has its own deadline; the caller's budget
    // starts once the worker is ready.
    let target = repo.map(Path::to_path_buf);
    let process = state.process.as_mut().expect("worker started");
    if process.prepared.as_ref() != Some(&target) {
        let preparing = Instant::now();
        match process.request(repo, "", "", Mode::Prepare, STARTUP_DEADLINE) {
            Ok(ready) => {
                process.prepared = Some(target);
                let phases = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                    &ready.content,
                )
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(name, ms)| Some((name, u128::from(ms.as_u64()?))))
                .collect();
                crate::telemetry::observe(crate::telemetry::Observation::PrivacyStartup {
                    spawn_ms,
                    ready_ms: preparing.elapsed().as_millis(),
                    phases,
                });
            }
            Err(Failure::Busy | Failure::Deadline) => return skipped(text, Reason::SpawnFailed),
            Err(Failure::Protocol(_)) => {
                process.terminate();
                state.failed_until = Some(Instant::now() + RETRY_DELAY);
                return skipped(text, Reason::SpawnFailed);
            }
        }
    }
    let protects = matches!(
        mode,
        Mode::ProtectText | Mode::ProtectJsonl | Mode::ProtectEnvelopes
    );
    let budget = budget.for_input(text.len());
    let started = Instant::now();
    let mut outcome = Outcome {
        content: String::with_capacity(text.len()),
        status: Status::Complete,
        replacements: 0,
        unresolved: 0,
        consumed: None,
        unprocessed: 0,
        reason: None,
    };
    let mut stopped = None;
    let mut cursor = 0;
    while cursor < text.len() {
        if budget.saturating_sub(started.elapsed()).is_zero() {
            stopped = Some(Reason::Deadline);
            break;
        }
        let end = if matches!(
            mode,
            Mode::ProtectJsonl
                | Mode::ProtectEnvelopes
                | Mode::HydrateJsonl
                | Mode::HydrateEnvelopes
        ) {
            let mut end = cursor;
            for line in text[cursor..].split_inclusive('\n') {
                if end > cursor && end + line.len() - cursor > BATCH_BYTES {
                    break;
                }
                end += line.len();
                if end - cursor >= BATCH_BYTES {
                    break;
                }
            }
            end
        } else {
            text.len()
        };
        // The worker counts the context against the same bound, so the check here does too.
        let carried = if cursor == 0 { context.len() } else { 0 };
        let oversized = carried + end - cursor > MAX_UNIT_BYTES;
        if oversized && !matches!(mode, Mode::ProtectJsonl | Mode::ProtectText) {
            stopped = Some(Reason::Oversize);
            if protects {
                break;
            }
            outcome.content.push_str(&text[cursor..end]);
            outcome.status = Status::Partial;
            cursor = end;
            continue;
        }
        let process = state.process.as_mut().expect("worker started");
        let remaining = || budget.saturating_sub(started.elapsed());
        let result = if oversized && matches!(mode, Mode::ProtectText) {
            protect_windows(process, repo, context, text, &remaining)
        } else if oversized {
            protect_oversized_line(process, repo, &text[cursor..end], &remaining)
        } else {
            process
                .request(
                    repo,
                    if cursor == 0 { context } else { "" },
                    &text[cursor..end],
                    mode,
                    remaining(),
                )
                .and_then(|batch| {
                    validate(&text[cursor..end], &batch, mode).map_err(Failure::Protocol)?;
                    Ok(batch)
                })
        };
        match result {
            Ok(batch) => {
                if matches!(mode, Mode::ProtectStream) {
                    return batch;
                }
                // A batch the worker answered without processing it ends the processed prefix.
                if protects && batch.status == Status::Skipped {
                    stopped = batch.reason.or(Some(Reason::StoreBusy));
                    break;
                }
                outcome.content.push_str(&batch.content);
                outcome.replacements += batch.replacements;
                outcome.unresolved += batch.unresolved;
                if batch.status != Status::Complete {
                    outcome.status = Status::Partial;
                    outcome.reason = outcome
                        .reason
                        .or(batch.reason)
                        .or(Some(Reason::SourceIncomplete));
                }
                cursor = end;
            }
            // A request an earlier one keeps from reaching the worker is contention, not this
            // input's failure.
            Err(Failure::Busy) => {
                stopped = Some(Reason::LockBusy);
                break;
            }
            // The worker keeps the request it was given; its late reply is recognized by id and
            // discarded, so a deadline costs neither the worker nor a back-off.
            Err(Failure::Deadline) => {
                stopped = Some(Reason::Deadline);
                break;
            }
            Err(Failure::Protocol(_)) => {
                state.process.as_mut().expect("worker started").terminate();
                state.failed_until = Some(Instant::now() + RETRY_DELAY);
                stopped = Some(Reason::ProtocolError);
                break;
            }
        }
    }
    if cursor < text.len() {
        outcome.content.push_str(&text[cursor..]);
        if cursor == 0 {
            outcome.status = Status::Skipped;
        } else if protects {
            // The status describes what the worker processed; `unprocessed` marks the rest.
            outcome.unprocessed = text.len() - cursor;
        } else {
            outcome.status = Status::Partial;
        }
        outcome.reason = stopped.or(outcome.reason);
    } else if stopped.is_some() {
        outcome.reason = outcome.reason.or(stopped);
    }
    outcome
}

/// How much of an oversized string one request carries, and the room an oversized record keeps
/// below the request bound once its largest strings are taken out.
const WINDOW_BYTES: usize = 4 * 1024 * 1024;
const WINDOW_CONTEXT_BYTES: usize = 16 * 1024;
/// The smallest window a part with too many findings is divided into.
const SMALLEST_WINDOW: usize = 64 * 1024;

/// What a record keeps in place of a string no request can carry and no window can divide.
pub(crate) const OMITTED_TEXT: &str = "[content omitted: too large for the privacy check]";
/// What a long text keeps in place of a part with too many findings for the privacy check to
/// finish or divide.
pub(crate) const CROWDED_TEXT: &str = "[content omitted: too many findings for the privacy check]";

/// One JSONL line too large for a request, protected in parts. Its base64 media payloads stay
/// here, since media is never scanned, and every other string too large to travel with the record
/// is protected on its own, window by window, each window with the text before it as context. The
/// line keeps its bytes unless something in it was replaced.
fn protect_oversized_line(
    process: &mut Process,
    repo: Option<&Path>,
    line: &str,
    remaining: &dyn Fn() -> Duration,
) -> Result<Outcome, Failure> {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let newline = &line[body.len()..];
    let Ok(mut record) = serde_json::from_str::<serde_json::Value>(body) else {
        // Not a record: the whole line is text.
        let mut protected = protect_windows(process, repo, "", body, remaining)?;
        if passed(&protected) {
            protected.content.push_str(newline);
        }
        return Ok(protected);
    };
    let mut strings = Vec::new();
    oversized_strings(&record, &mut String::new(), None, false, &mut strings);
    let encoded = |text: &str| serde_json::Value::from(text).to_string().len();
    // Media always stays here; then the largest strings, until the rest fits one request.
    strings.sort_by_key(|part| (!part.media, std::cmp::Reverse(part.bytes)));
    let mut size = body.len();
    let mut parts = Vec::new();
    for (index, part) in strings.into_iter().enumerate() {
        if !part.media && size <= MAX_UNIT_BYTES - WINDOW_CONTEXT_BYTES {
            break;
        }
        let mut stub = format!("agit-oversized-part-{index}");
        while body.contains(&stub) {
            stub.push('-');
        }
        let slot = record
            .pointer_mut(&part.pointer)
            .expect("the pointer was collected from this record");
        let original = std::mem::replace(slot, serde_json::Value::from(stub.as_str()));
        size = size - part.bytes + encoded(&stub);
        parts.push((part, stub, original));
    }
    let stubbed = format!("{record}{newline}");
    if size > MAX_UNIT_BYTES - WINDOW_CONTEXT_BYTES || stubbed.len() > MAX_UNIT_BYTES {
        return Ok(skipped(line, Reason::Oversize));
    }
    let batch = process.request(repo, "", &stubbed, Mode::ProtectJsonl, remaining())?;
    validate(&stubbed, &batch, Mode::ProtectJsonl).map_err(Failure::Protocol)?;
    if !passed(&batch) {
        return Ok(skipped(
            line,
            batch.reason.unwrap_or(Reason::SourceIncomplete),
        ));
    }
    let mut protected = serde_json::from_str::<serde_json::Value>(
        batch.content.strip_suffix('\n').unwrap_or(&batch.content),
    )
    .map_err(|error| Failure::Protocol(error.into()))?;
    let mut replacements = batch.replacements;
    for (part, stub, original) in parts {
        let slot = protected
            .pointer_mut(&part.pointer)
            .filter(|slot| slot.as_str() == Some(stub.as_str()))
            .ok_or_else(|| {
                Failure::Protocol(anyhow::anyhow!("an oversized part did not return"))
            })?;
        *slot = if part.media {
            original
        } else {
            let text = original.as_str().unwrap_or_default();
            // The field name leads the first window, as it would lead the value in the record.
            let lead = part
                .field
                .as_deref()
                .filter(|field| field.len() <= 128 && field.is_ascii())
                .map(|field| format!("{field} = "))
                .unwrap_or_default();
            let windows = protect_windows(process, repo, &lead, text, remaining)?;
            if windows.reason == Some(Reason::Oversize) {
                // No cut in any window that fits a request leaves a finding whole: the string
                // is omitted rather than holding the session back for good.
                replacements += 1;
                serde_json::Value::from(OMITTED_TEXT)
            } else if !passed(&windows) {
                return Ok(skipped(
                    line,
                    windows.reason.unwrap_or(Reason::SourceIncomplete),
                ));
            } else {
                replacements += windows.replacements;
                serde_json::Value::from(windows.content)
            }
        };
    }
    Ok(Outcome {
        content: if replacements == 0 {
            line.to_owned()
        } else {
            format!("{protected}{newline}")
        },
        status: Status::Complete,
        replacements,
        unresolved: 0,
        consumed: None,
        unprocessed: 0,
        reason: None,
    })
}

/// A string inside an oversized record: where it is, the field that holds it, its size as
/// encoded in the record, and whether it is a media payload.
struct OversizedString {
    pointer: String,
    field: Option<String>,
    bytes: usize,
    media: bool,
}

/// Strings in `value` large enough to matter for an oversized record. `in_source` says `value`
/// sits in a base64 source object, whose `data` is media.
fn oversized_strings(
    value: &serde_json::Value,
    pointer: &mut String,
    field: Option<&str>,
    in_source: bool,
    out: &mut Vec<OversizedString>,
) {
    const SMALLEST: usize = 1024;
    match value {
        serde_json::Value::String(text) if text.len() >= SMALLEST => out.push(OversizedString {
            pointer: pointer.clone(),
            field: field.map(str::to_owned),
            bytes: value.to_string().len(),
            media: super::projector::media_payload(in_source && field == Some("data"), text),
        }),
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                let at = pointer.len();
                pointer.push_str(&format!("/{index}"));
                oversized_strings(value, pointer, None, false, out);
                pointer.truncate(at);
            }
        }
        serde_json::Value::Object(values) => {
            let source = super::projector::base64_source(values);
            for (key, value) in values {
                let at = pointer.len();
                pointer.push('/');
                pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
                oversized_strings(value, pointer, Some(key), source, out);
                pointer.truncate(at);
            }
        }
        _ => {}
    }
}

/// `text` protected in windows that each fit one request. Every window carries as context the
/// text before it that a rule can still need, the way a live stream does
/// ([`crate::domain::redact::context_start`]), and `lead` before the first, so a rule matches
/// across window edges; when that context no longer fits a request, the text is not protected. A
/// window ends where the worker's stream cut says no finding is split, and the last one is
/// protected whole. A window with no safe cut grows, and one with too many findings for a scan
/// shrinks to [`SMALLEST_WINDOW`]. A part that can neither shrink nor grow, the smallest window
/// still too crowded or a window with no cut below a crowded size, becomes [`CROWDED_TEXT`], so
/// the text always moves on and nothing in that part is published. The context's findings count
/// toward a scan too, so the window after a crowded part can be omitted with it.
fn protect_windows(
    process: &mut Process,
    repo: Option<&Path>,
    lead: &str,
    text: &str,
    remaining: &dyn Fn() -> Duration,
) -> Result<Outcome, Failure> {
    let mut outcome = Outcome {
        content: String::with_capacity(text.len()),
        status: Status::Complete,
        replacements: 0,
        unresolved: 0,
        consumed: None,
        unprocessed: 0,
        reason: None,
    };
    let keys = crate::domain::redact::PrivateKeys::new(text);
    let mut start = 0;
    let mut window = WINDOW_BYTES;
    // The smallest window at `start` whose scan had too many findings to finish.
    let mut crowded: Option<usize> = None;
    while start < text.len() {
        let context = if start == 0 {
            lead
        } else {
            match crate::domain::redact::context_start(&text[..start]) {
                Some(from) => &text[from..start],
                None => return Ok(skipped(text, Reason::Oversize)),
            }
        };
        let room = MAX_UNIT_BYTES - context.len();
        window = window.min(room);
        let last = text.len() - start <= window;
        let mut end = if last { text.len() } else { start + window };
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mode = if last {
            Mode::ProtectText
        } else {
            Mode::ProtectStream
        };
        let piece = process.request(repo, context, &text[start..end], mode, remaining())?;
        validate(&text[start..end], &piece, mode).map_err(Failure::Protocol)?;
        if !passed(&piece) {
            return Ok(skipped(
                text,
                piece.reason.unwrap_or(Reason::SourceIncomplete),
            ));
        }
        // A crowded window released nothing, or was replaced whole; either way it is divided.
        let capped = piece.reason == Some(Reason::FindingsCap);
        let consumed = if capped {
            0
        } else if last {
            end - start
        } else {
            piece.consumed.unwrap_or(0)
        };
        // A cut inside a private key that is still open is no safe cut: the header would leave
        // with this part and the rest of the body would reach a request that cannot recognize it.
        let consumed = if consumed > 0 && !last && keys.end_after(start + consumed).is_some() {
            0
        } else {
            consumed
        };
        if consumed == 0 {
            let size = end - start;
            if capped {
                crowded = Some(size);
            }
            let stuck = if capped {
                size <= SMALLEST_WINDOW
            } else {
                crowded.is_some_and(|crowded| size.saturating_mul(2) >= crowded)
            };
            if stuck {
                outcome.content.push_str(CROWDED_TEXT);
                outcome.replacements += 1;
                // The omission covers a private key it opens to the key's footer, so no part of
                // its body is left for a window that no longer sees the header.
                start = keys.end_after(end).unwrap_or(end);
                window = WINDOW_BYTES;
                crowded = None;
            } else if capped {
                window = (size / 2).max(SMALLEST_WINDOW);
            } else if window >= room {
                return Ok(skipped(text, Reason::Oversize));
            } else {
                window = window.saturating_mul(2).min(room);
            }
            continue;
        }
        outcome.content.push_str(&piece.content);
        outcome.replacements += piece.replacements;
        start += consumed;
        window = WINDOW_BYTES;
        crowded = None;
    }
    Ok(outcome)
}

/// Admission checks prevent a bad worker response from changing carrier boundaries.
fn validate(input: &str, output: &Outcome, mode: Mode) -> crate::Result<()> {
    if matches!(mode, Mode::ProtectStream) {
        let consumed = output
            .consumed
            .context("privacy stream cursor is missing")?;
        ensure!(
            consumed <= input.len() && input.is_char_boundary(consumed),
            "invalid privacy stream cursor"
        );
    }
    if matches!(
        mode,
        Mode::ProtectJsonl | Mode::ProtectEnvelopes | Mode::HydrateJsonl | Mode::HydrateEnvelopes
    ) {
        let mut lines = output.content.split_inclusive('\n');
        for before in input.split_inclusive('\n') {
            let after = lines
                .next()
                .context("privacy response lost a native line")?;
            ensure!(
                before.ends_with('\n') == after.ends_with('\n'),
                "privacy changed a record boundary"
            );
            match (
                serde_json::from_str::<serde_json::Value>(before),
                serde_json::from_str::<serde_json::Value>(after),
            ) {
                (Ok(before), Ok(after)) => {
                    for field in [
                        "_session_id",
                        "_source",
                        "session_id",
                        "sessionId",
                        "event_id",
                        "uuid",
                        "type",
                    ] {
                        // Native identities masked by legacy writers need their original value on restore.
                        let restored_native_identity = matches!(mode, Mode::HydrateJsonl)
                            && matches!(field, "session_id" | "sessionId" | "event_id" | "uuid")
                            && before
                                .get(field)
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|value| {
                                    super::dictionary::token_identity(value).is_some()
                                });
                        ensure!(
                            before.get(field) == after.get(field) || restored_native_identity,
                            "privacy changed a carrier identity"
                        );
                    }
                }
                (Err(_), Err(_)) => {}
                _ => anyhow::bail!("privacy changed native record framing"),
            }
        }
        ensure!(
            lines.next().is_none(),
            "privacy response added native lines"
        );
    }
    Ok(())
}

/// The outbox is durable, so scheduling cannot become a precondition of a successful push.
pub fn schedule_sync(hub: &str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let started = (|| -> crate::Result<Child> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([worker::SYNC, hub])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        Ok(command.spawn()?)
    })();
    let Ok(mut child) = started else {
        RUNNING.store(false, Ordering::Release);
        return;
    };
    if std::thread::Builder::new()
        .name("privacy-sync".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(25);
            while !matches!(child.try_wait(), Ok(Some(_))) {
                if Instant::now() >= deadline {
                    #[cfg(unix)]
                    unsafe {
                        libc::killpg(child.id() as i32, libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            RUNNING.store(false, Ordering::Release);
        })
        .is_err()
    {
        RUNNING.store(false, Ordering::Release);
    }
}

/// Missing recovery data cannot veto a native append. Unknown opaque spans retain history.
pub fn native_continuity(
    repo: &Path,
    committed: &str,
    live: &str,
) -> crate::Result<crate::domain::transcript::Continuity> {
    use crate::domain::{
        privacy::continuity,
        transcript::{self, Continuity},
    };
    let direct = transcript::continuity(committed, live);
    if direct != Continuity::Diverged {
        return Ok(direct);
    }
    let comparable = transform(Some(repo), committed, Mode::HydrateEnvelopes);
    let Some((end, _)) = continuity::retain_prefix(committed, &comparable.content, live)? else {
        return Ok(Continuity::Diverged);
    };
    Ok(
        if live[end..]
            .lines()
            .any(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
        {
            Continuity::Append
        } else {
            Continuity::Noop
        },
    )
}

/// Explicit management can report its own failure without making it a business prerequisite.
pub fn manage(
    repo: Option<&Path>,
    command: super::management::Command,
) -> crate::Result<serde_json::Value> {
    let input = Zeroizing::new(serde_json::to_string(&command)?);
    let outcome = transform(repo, &input, Mode::Manage);
    ensure!(
        outcome.status == Status::Complete,
        "privacy management is temporarily unavailable; recording and upload remain available"
    );
    Ok(serde_json::from_str(&outcome.content)?)
}

/// One line for each journal of rules this device registered under another account or Hub that
/// it cannot read, naming the account and how to make the rules readable again; empty when every
/// rule reads. An error when the worker cannot say, which is not the same as nothing to report.
pub fn unreadable_rule_hints() -> crate::Result<Vec<String>> {
    let command = super::management::Command {
        action: "status".into(),
        global: true,
        id: None,
        name: None,
        secret: None,
    };
    let status = manage(None, command)?;
    let rows = status["unreadable_rules"]
        .as_array()
        .context("privacy status did not list unreadable rules")?;
    Ok(rows
        .iter()
        .map(|row| match (row["hub"].as_str(), row["account"].as_str()) {
            (Some(hub), Some(account)) => format!(
                "rules this computer registered for account {account} on {hub} cannot be read; \
                 sign in to that account once with `agit login --hub {hub}` so agit can read them, \
                 then sign back in"
            ),
            _ => format!(
                "the privacy rules in {} cannot be read; the file may be damaged",
                row["path"].as_str().unwrap_or("an unreadable journal")
            ),
        })
        .collect())
}

/// Ends this process's worker before the process exits, unless another thread is using it. No
/// caller is waiting on a request then, so one still in flight was abandoned.
///
/// The worker holds the privacy journal open. One that outlives its command is still closing the
/// journal when the next command opens it: that command's worker answers `store_busy` and a
/// settlement leaves its turn unsettled, and a running worker reading the device's rules finds
/// that journal unreadable and withholds what it checks.
pub fn shutdown() {
    let process = shared()
        .try_lock()
        .ok()
        .and_then(|mut state| state.process.take());
    if let Some(mut process) = process {
        process.close();
    }
}

/// Reload requests invalidate only the optional worker; its next caller gets a fresh snapshot.
pub fn invalidate() {
    if let Ok(mut state) = shared().try_lock() {
        if let Some(process) = &mut state.process {
            process.terminate();
        }
        state.failed_until = Some(Instant::now());
    }
}

/// Observations are optional privacy input; canonical IDs retain their exact values. The
/// metadata changes only once every observation passed protection; until then this returns
/// [`Pending`] and the caller must not commit the metadata.
pub fn protect_metadata(
    repo: &Path,
    metadata: &mut crate::domain::meta::Meta,
) -> Result<(), Pending> {
    let fields = observation_fields(metadata);
    let malformed = || Pending {
        reason: Some(Reason::ProtocolError),
    };
    let input = serde_json::to_string(&fields).map_err(|_| malformed())?;
    let protected = protect_for_commit(Some(repo), &input, Mode::ProtectJsonl)?;
    let values = serde_json::from_str::<Vec<String>>(&protected)
        .ok()
        .filter(|values| values.len() == fields.len())
        .ok_or_else(malformed)?;
    for (field, value) in fields.into_iter().zip(values) {
        *field = value;
    }
    Ok(())
}

fn observation_fields(metadata: &mut crate::domain::meta::Meta) -> Vec<&mut String> {
    let mut fields = vec![&mut metadata.cwd];
    fields.extend(metadata.code.iter_mut());
    fields.extend(metadata.milestone.iter_mut());
    fields.extend(metadata.title.iter_mut());
    if let Some(state) = metadata.cwd_state.as_mut() {
        if !crate::domain::secrets::identity::empty_status_digest(state) {
            fields.extend(state.status_digest.iter_mut());
        }
        fields.extend(state.origin.iter_mut());
        fields.extend(state.branch.iter_mut());
    }
    fields
}

/// Missing mappings remain visible as opaque placeholders for callers to diagnose.
pub fn hydrate_metadata(repo: &Path, metadata: &mut crate::domain::meta::Meta) -> usize {
    let fields = observation_fields(metadata);
    let Ok(input) = serde_json::to_string(&fields) else {
        return usize::MAX;
    };
    let outcome = transform(Some(repo), &input, Mode::HydrateJsonl);
    let Ok(values) = serde_json::from_str::<Vec<String>>(&outcome.content) else {
        return usize::MAX;
    };
    if fields.len() != values.len() {
        return usize::MAX;
    }
    let mut unresolved = 0;
    for (field, value) in fields.into_iter().zip(values) {
        unresolved += super::projector::tokens(&value).count();
        *field = value;
    }
    unresolved
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Exit does not wait out a request whose caller gave up. A hydration request may run for its
    /// whole watchdog allowance, far longer than the floor; a close that waited for the reply, or
    /// for the worker to leave on its own, would hold the command's exit that long for a result
    /// nobody reads.
    #[test]
    fn closing_kills_a_worker_still_on_an_abandoned_hydration_request() {
        // A worker that never answers and ignores the end of its input.
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 60"]);
        let mut process = Process::spawn(command).unwrap();
        assert!(matches!(
            process.request(
                None,
                "",
                "",
                Mode::HydrationValues,
                Duration::from_millis(10)
            ),
            Err(Failure::Deadline)
        ));
        let started = Instant::now();
        process.close();
        assert!(started.elapsed() < worker::WATCHDOG_FLOOR + Duration::from_secs(2));
        assert!(matches!(process.child.try_wait(), Ok(Some(_))));
    }
}
