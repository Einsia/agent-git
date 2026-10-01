//! Business callers receive content even when the isolated privacy worker is unavailable.

use super::projector::{MAX_UNIT_BYTES, Mode, Outcome, Status};
use super::worker::{self, Request};
use anyhow::{Context, ensure};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const FOREGROUND_BUDGET: Duration = Duration::from_millis(750);
const RETRY_DELAY: Duration = Duration::from_secs(10);
const BATCH_BYTES: usize = 128 * 1024;

struct Process {
    child: Child,
    #[cfg(windows)]
    job: Option<crate::rc::windows_job::Job>,
    send: mpsc::SyncSender<Zeroizing<Vec<u8>>>,
    receive: mpsc::Receiver<crate::Result<Outcome>>,
    io: Option<std::thread::JoinHandle<()>>,
}

impl Process {
    fn start() -> crate::Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg(worker::WORKER)
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
                    let result = (|| -> crate::Result<Outcome> {
                        worker::write_frame(&mut input, &bytes)?;
                        let response = worker::read_frame(&mut output)?;
                        let outcome: Outcome = serde_json::from_slice(&response)?;
                        ensure!(
                            outcome.content.len() <= MAX_UNIT_BYTES + 1024 * 1024,
                            "privacy output exceeds its byte budget"
                        );
                        Ok(outcome)
                    })();
                    let failed = result.is_err();
                    if responses.send(result).is_err() || failed {
                        break;
                    }
                }
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
        })
    }

    fn request(
        &mut self,
        repo: Option<&Path>,
        text: &str,
        mode: Mode,
        remaining: Duration,
    ) -> crate::Result<Outcome> {
        let request = Request {
            repo: repo.map(Path::to_owned),
            mode,
            text: text.to_owned(),
        };
        let bytes = Zeroizing::new(serde_json::to_vec(&request)?);
        self.send
            .try_send(bytes)
            .map_err(|_| anyhow::anyhow!("privacy worker is busy"))?;
        self.receive
            .recv_timeout(remaining)
            .context("privacy worker deadline expired")?
    }

    fn terminate(&mut self) {
        #[cfg(windows)]
        if let Some(job) = self.job.take() {
            let _ = job.terminate_and_close();
        }
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.child.id() as i32, libc::SIGKILL);
        }
        let _ = self.child.kill();
        let (closed, receiver) = mpsc::sync_channel(0);
        drop(receiver);
        self.send = closed;
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

fn shared() -> &'static Mutex<State> {
    static STATE: Mutex<State> = Mutex::new(State {
        process: None,
        failed_until: None,
    });
    &STATE
}

/// One bounded attempt per operation; lock contention immediately uses the caller's input.
pub fn transform(repo: Option<&Path>, text: &str, mode: Mode) -> Outcome {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        worker::inline(repo, text, mode).unwrap_or_else(|| transform_inner(repo, text, mode))
    }))
    .unwrap_or_else(|_| Outcome::skipped(text))
}

fn transform_inner(repo: Option<&Path>, text: &str, mode: Mode) -> Outcome {
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
        };
    }
    let started = Instant::now();
    let Ok(mut state) = shared().try_lock() else {
        return Outcome::skipped(text);
    };
    if let Some(until) = state.failed_until {
        if Instant::now() < until {
            return Outcome::skipped(text);
        }
        if state
            .process
            .as_mut()
            .is_some_and(|process| !process.reaped())
        {
            return Outcome::skipped(text);
        }
        state.process = None;
        state.failed_until = None;
    }
    if state.process.is_none() {
        match Process::start() {
            Ok(process) => state.process = Some(process),
            Err(_) => {
                state.failed_until = Some(Instant::now() + RETRY_DELAY);
                return Outcome::skipped(text);
            }
        }
    }
    let mut outcome = Outcome {
        content: String::with_capacity(text.len()),
        status: Status::Complete,
        replacements: 0,
        unresolved: 0,
        consumed: None,
    };
    let mut cursor = 0;
    while cursor < text.len() {
        let remaining = FOREGROUND_BUDGET.saturating_sub(started.elapsed());
        if remaining.is_zero() {
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
        if end - cursor > MAX_UNIT_BYTES {
            outcome.content.push_str(&text[cursor..end]);
            outcome.status = Status::Partial;
            cursor = end;
            continue;
        }
        let result = state.process.as_mut().expect("worker started").request(
            repo,
            &text[cursor..end],
            mode,
            remaining,
        );
        let result = result.and_then(|batch| {
            validate(&text[cursor..end], &batch, mode)?;
            Ok(batch)
        });
        match result {
            Ok(batch) => {
                if matches!(mode, Mode::ProtectStream) {
                    return batch;
                }
                outcome.content.push_str(&batch.content);
                outcome.replacements += batch.replacements;
                outcome.unresolved += batch.unresolved;
                if batch.status != Status::Complete {
                    outcome.status = Status::Partial;
                }
                cursor = end;
            }
            Err(_) => {
                state.process.as_mut().expect("worker started").terminate();
                state.failed_until = Some(Instant::now() + RETRY_DELAY);
                break;
            }
        }
    }
    if cursor < text.len() {
        outcome.content.push_str(&text[cursor..]);
        outcome.status = if cursor == 0 {
            Status::Skipped
        } else {
            Status::Partial
        };
    }
    outcome
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

/// Reload requests invalidate only the optional worker; its next caller gets a fresh snapshot.
pub fn invalidate() {
    if let Ok(mut state) = shared().try_lock() {
        if let Some(process) = &mut state.process {
            process.terminate();
        }
        state.failed_until = Some(Instant::now());
    }
}

/// Observations are optional privacy input; canonical IDs retain their exact values.
pub fn protect_metadata(repo: &Path, metadata: &mut crate::domain::meta::Meta) {
    let fields = observation_fields(metadata);
    let Ok(input) = serde_json::to_string(&fields) else {
        return;
    };
    let protected = transform(Some(repo), &input, Mode::ProtectJsonl);
    let Ok(values) = serde_json::from_str::<Vec<String>>(&protected.content) else {
        return;
    };
    if fields.len() == values.len() {
        for (field, value) in fields.into_iter().zip(values) {
            *field = value;
        }
    }
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
