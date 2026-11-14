//! Observation retains collection authority without acquiring native input control.

use super::*;
use anyhow::{Context, ensure};

#[derive(Default)]
pub(super) struct Collector {
    cursor: usize,
    recorded: HashMap<String, Recorded>,
    reported: HashMap<String, String>,
    /// Conversations whose capture recorded no carrier, and when each may be read again. Each
    /// attempt reads and hashes the whole transcript.
    delayed: HashMap<String, Delay>,
}

/// The first delay after a capture records no carrier, and the longest; each further one doubles.
const DELAYS: (std::time::Duration, std::time::Duration) = (
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(10 * 60),
);

struct Delay {
    failures: u32,
    /// The quick tries used since the last capture that recorded a carrier.
    quick: u32,
    until: tokio::time::Instant,
}

/// The completed boundary last recorded for a conversation, if it has one yet, and the carrier
/// it was read from.
#[derive(Clone, Debug)]
struct Recorded {
    digest: Option<String>,
    carrier: Carrier,
    /// Where the next read of the carrier may start, when it is still the transcript read here.
    progress: Option<Progress>,
}

/// How long a conversation's transcript may be read only past its last completed turn before it
/// is read whole again, whether or not it changed since. A rewrite of what came before that turn
/// that the shorter read can't see, one with an append in the same write, is refused by the
/// archive job, and the whole read that follows takes it in; a transcript that went quiet right
/// after it is read whole all the same.
const WHOLE_READ_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Where a transcript's completed part ends, and the SHA-256 state over that part, so the next
/// read starts there: what follows the last completed turn is the turn running and what was
/// appended, and only it can move the boundary.
#[derive(Clone)]
struct Progress {
    end: u64,
    hasher: sha2::Sha256,
    /// When the transcript was last read whole.
    whole_at: tokio::time::Instant,
    /// Whether it was read only past a completed turn since.
    read_past: bool,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress")
            .field("end", &self.end)
            .field("whole_at", &self.whole_at)
            .field("read_past", &self.read_past)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Carrier {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    file: (u64, u64),
    /// Claude Code's completion receipts, which end a turn without changing the transcript.
    receipts: Vec<std::ffi::OsString>,
}

impl Carrier {
    fn of(entry: &roster::Entry, path: &std::path::Path, metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        #[cfg(unix)]
        let receipts = if entry.runtime == "claude-code" {
            crate::rc::native_claude::completion::receipt_names(&entry.thread_id, path)
        } else {
            vec![]
        };
        #[cfg(not(unix))]
        let receipts = {
            let _ = entry;
            vec![]
        };
        Self {
            path: normalized(path),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            file: (metadata.dev(), metadata.ino()),
            receipts,
        }
    }

    /// Whether `other` was read from the same file as this carrier, which it can have grown since.
    fn same_file(&self, other: &Self) -> bool {
        #[cfg(unix)]
        let same = self.file == other.file;
        #[cfg(not(unix))]
        let same = true;
        same && self.path == other.path && other.len >= self.len
    }

    /// A carrier that is still the conversation's current one and is unchanged holds no new
    /// turn, so the whole transcript need not be read and hashed again. Codex restores a
    /// compressed rollout before it appends, so a carrier that went cold since it was read is
    /// unchanged too, and checking it never restores it. Whether a whole read is due all the same
    /// is not asked here; [`Recorded::unchanged`] asks both.
    fn current_and_unchanged(&self, entry: &roster::Entry) -> bool {
        let current = current_carrier(entry);
        // Gemini CLI deletes a session past its retention; with no other file in its place the
        // conversation is cold, not changed, and is never read again.
        if current.is_none() && entry.runtime == "gemini-cli" && !self.path.exists() {
            return true;
        }
        if current.is_none_or(|current| normalized(&current) != self.path) {
            return false;
        }
        match std::fs::metadata(&self.path) {
            Ok(metadata) => Self::of(entry, &self.path, &metadata) == *self,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entry.runtime == "codex" && crate::adapter::codex_rollout::is_cold(&self.path)
            }
            Err(_) => false,
        }
    }
}

impl Recorded {
    /// Whether the collector passes this conversation over: its carrier is still current and
    /// unchanged, and it was not read only past a completed turn [`WHOLE_READ_EVERY`] or longer
    /// after it was last read whole. Without the second part, a transcript that went quiet after
    /// such a read is never read whole again until it changes, however long that takes.
    fn unchanged(&self, entry: &roster::Entry, now: tokio::time::Instant) -> bool {
        self.carrier.current_and_unchanged(entry)
            && !self.progress.as_ref().is_some_and(|progress| {
                progress.read_past && now >= progress.whole_at + WHOLE_READ_EVERY
            })
    }
}

fn normalized(path: &std::path::Path) -> PathBuf {
    match (
        path.parent().map(|parent| parent.canonicalize()),
        path.file_name(),
    ) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_owned(),
    }
}

/// The transcript the runtime currently writes, located without restoring cold storage.
fn current_carrier(entry: &roster::Entry) -> Option<PathBuf> {
    if entry.runtime != "codex" {
        return crate::adapter::get(&entry.runtime)
            .ok()?
            .resolve_present(&entry.thread_id, Some(std::path::Path::new(&entry.cwd)));
    }
    let thread = match &entry.native_source {
        Some(source) => {
            let registry = crate::rc::runtime_sources::Registry::open().ok()?;
            let context =
                crate::rc::runtime_context::RuntimeContext::resolve(&registry, &source.source_id)
                    .ok()?;
            crate::adapter::codex_index::thread_by_id_in(&context.source.home, &entry.thread_id)?
        }
        None => crate::adapter::codex_index::thread_by_id(&entry.thread_id)?,
    };
    Some(crate::adapter::codex_rollout::plain_path(
        &thread.rollout_path,
    ))
}

impl Collector {
    pub(super) async fn reconcile(
        &mut self,
        daemon: &Arc<Mutex<Daemon>>,
        admission: &crate::rc::admission::Admission,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) {
        let mut sessions: Vec<_> = daemon
            .lock()
            .await
            .roster
            .observed
            .iter()
            .cloned()
            .collect();
        if sessions.is_empty() {
            return;
        }
        self.recorded
            .retain(|logical, _| sessions.contains(logical));
        self.reported
            .retain(|logical, _| sessions.contains(logical));
        self.delayed.retain(|logical, _| sessions.contains(logical));
        let offset = self.cursor % sessions.len();
        sessions.rotate_left(offset);
        self.cursor = self.cursor.wrapping_add(16);
        for logical in sessions.into_iter().take(16) {
            let Some(_work) = admission.enter() else {
                return;
            };
            if self.is_delayed(&logical, tokio::time::Instant::now()) {
                continue;
            }
            if let Some(recorded) = self.recorded.get(&logical).cloned() {
                let entry = daemon.lock().await.roster.get(&logical).cloned();
                let now = tokio::time::Instant::now();
                if let Some(entry) = entry
                    && tokio::task::spawn_blocking(move || recorded.unchanged(&entry, now))
                        .await
                        .unwrap_or(false)
                {
                    continue;
                }
            }
            // A quick try that finds another one waiting on the worker keeps its place.
            let _quick = if self.in_quick(&logical) {
                match super::archive_recovery::QUICK_GATE.try_acquire() {
                    Ok(permit) => Some(permit),
                    Err(_) => continue,
                }
            } else {
                None
            };
            let request = {
                let mut state = daemon.lock().await;
                state.reserve_observed_capture(&logical)
            };
            let Some(request) = request else { continue };
            let claim = request.prior_entry.as_ref().map(|entry| {
                entry
                    .native_source
                    .as_ref()
                    .map(|source| source.session_ref(&entry.thread_id))
                    .unwrap_or_else(|| entry.thread_id.clone())
            });
            // A stop lets the capture in flight finish within the grace archive recovery gives its
            // stages. A capture dropped at once ends the settlement subprocess it started wherever
            // that subprocess is, and a repository write it is in the middle of stays half done.
            let previous = self.recorded.get(&logical).cloned();
            let capture = {
                let daemon = daemon.clone();
                async move {
                    Ok::<_, anyhow::Error>(
                        tokio::time::timeout(
                            std::time::Duration::from_secs(30),
                            collect(&daemon, request, previous),
                        )
                        .await,
                    )
                }
            };
            let result = super::archive_recovery::run(stop, capture).await;
            daemon.lock().await.opening_sessions.remove(&logical);
            let Some(result) = result else { return };
            // A capture that panicked fails like one that returned an error.
            let result = result.unwrap_or_else(|error| Ok(Err(error)));
            // Slow: the capture ran out of its time, or its landing recorded a wait the worker
            // may get past when asked again.
            let slow = match &result {
                Err(_) => true,
                Ok(Err(_)) => match claim {
                    Some(claim) => tokio::task::spawn_blocking(move || {
                        crate::domain::privacy::pending::load()
                            .get(&claim)
                            .is_some_and(|wait| {
                                wait.refused_bytes.is_none()
                                    && crate::domain::privacy::service::transient(&wait.reason)
                            })
                    })
                    .await
                    .unwrap_or(false),
                    None => false,
                },
                Ok(Ok(_)) => false,
            };
            self.settled(
                &logical,
                matches!(result, Ok(Ok(Some(_)))),
                slow,
                tokio::time::Instant::now(),
            );
            match result {
                Ok(Ok(recorded)) => {
                    self.reported.remove(&logical);
                    if let Some(recorded) = recorded {
                        self.recorded.insert(logical, recorded);
                    }
                }
                result => {
                    // The next capture reads the whole transcript again: a failure may mean the
                    // part before the last completed turn is not what was read.
                    if let Some(recorded) = self.recorded.get_mut(&logical) {
                        recorded.progress = None;
                    }
                    // The same failure recurs every cycle until its cause changes; one line each.
                    let failure = format!("{result:?}");
                    if self.reported.get(&logical) != Some(&failure) {
                        eprintln!(
                            "agitd: observed conversation archive remains pending ({logical}): {failure}"
                        );
                        self.reported.insert(logical, failure);
                    }
                }
            }
        }
    }
}

impl Collector {
    fn is_delayed(&self, logical: &str, now: tokio::time::Instant) -> bool {
        self.delayed
            .get(logical)
            .is_some_and(|delay| now < delay.until)
    }

    /// Whether the next capture of `logical` is one of its quick tries.
    fn in_quick(&self, logical: &str) -> bool {
        self.delayed
            .get(logical)
            .is_some_and(|delay| delay.failures == 0 && delay.quick > 0)
    }

    /// Notes how a capture of `logical` ended: with a carrier recorded, or not. One that ended
    /// only because the privacy worker did not answer in time (`slow`) is tried again shortly a
    /// few times before the doubling delay applies.
    fn settled(&mut self, logical: &str, recorded: bool, slow: bool, now: tokio::time::Instant) {
        use super::archive_recovery::{QUICK_DELAY, QUICK_TRIES};
        if recorded {
            self.delayed.remove(logical);
            return;
        }
        let delay = self.delayed.entry(logical.into()).or_insert(Delay {
            failures: 0,
            quick: 0,
            until: now,
        });
        if slow && delay.failures == 0 && delay.quick < QUICK_TRIES {
            delay.quick += 1;
            delay.until = now + super::archive_recovery::retry_delay(QUICK_DELAY);
            return;
        }
        delay.failures = delay.failures.saturating_add(1);
        delay.until = now
            + super::archive_recovery::retry_delay(
                DELAYS
                    .0
                    .saturating_mul(1 << (delay.failures - 1).min(16))
                    .min(DELAYS.1),
            );
    }
}

async fn collect(
    daemon: &Arc<Mutex<Daemon>>,
    request: super::capture_binding::Request,
    previous: Option<Recorded>,
) -> crate::Result<Option<Recorded>> {
    let logical = request.logical.clone();
    let mut entry = request
        .prior_entry
        .clone()
        .context("Observed session binding is missing")?;
    let project = request
        .project
        .clone()
        .context("Observed project is missing")?;
    let lineage = request
        .resolve()
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?
        .context("Observed capture is unavailable")?;
    {
        let mut state = daemon.lock().await;
        ensure!(
            state.opts.local_owner
                && state.settlement.borrow().local_owner
                && state
                    .mirror
                    .project_path(&entry.workspace_id, &project.0)
                    .as_ref()
                    == Some(&project.1),
            "Observed project binding changed during capture"
        );
        ensure!(
            state.roster.get(&logical).is_some_and(|current| {
                current.thread_id == entry.thread_id
                    && crate::protocol::NativeSourceRef::same_store(
                        current.native_source.as_ref(),
                        entry.native_source.as_ref(),
                    )
                    && current.runtime == entry.runtime
                    && current.cwd == entry.cwd
            }),
            "Observed native binding changed during capture"
        );
        entry.agit_session = Some(lineage.to_string());
        entry.expected_agent_id = Some(lineage.agent_id().into());
        let changed = state.roster.get(&logical).is_none_or(|current| {
            current.agit_session != entry.agit_session
                || current.expected_agent_id != entry.expected_agent_id
        }) || state.roster.captures.get(&logical) != lineage.capture.as_ref();
        if changed {
            let mut roster = state.roster.clone();
            if let Some(kind) = &lineage.capture {
                roster.captures.insert(logical.clone(), kind.clone());
            }
            roster.record(&logical, entry.clone())?;
            roster.save()?;
            state.roster = roster;
            // The row's repository and branch come from this lineage.
            state
                .list_changes
                .session(super::dormant_row(&logical, &entry));
        }
    }
    let now = tokio::time::Instant::now();
    tokio::task::spawn_blocking(move || {
        capture_boundary(&logical, &entry, &lineage, previous.as_ref(), now)
    })
    .await?
}

fn capture_boundary(
    logical: &str,
    entry: &roster::Entry,
    lineage: &crate::rc::lineage::AgitSession,
    previous: Option<&Recorded>,
    now: tokio::time::Instant,
) -> crate::Result<Option<Recorded>> {
    use crate::domain::{link, store::Store};
    let key = entry
        .native_source
        .as_ref()
        .map(|source| source.session_ref(&entry.thread_id))
        .unwrap_or_else(|| entry.thread_id.clone());
    let store = Store::open()?.context("Observed capture store is missing")?;
    let claim = link::get_checked(&store, &entry.runtime, &key)?
        .context("Observed capture claim is missing")?;
    #[cfg(unix)]
    if entry.runtime == "claude-code"
        && crate::rc::native_claude::Client::unwritten_transcript(
            &entry.thread_id,
            std::path::Path::new(&entry.cwd),
        )
        .is_some()
    {
        return Ok(None);
    }
    let path = claim
        .resolve()
        .context("Observed native transcript is missing")?;
    let mut file = std::fs::File::open(&path)?;
    let metadata = file.metadata()?;
    let carrier = Carrier::of(entry, &path, &metadata);
    let size = metadata.len();
    ensure!(
        size <= crate::adapter::native_snapshot::MAX_CAPTURE_BYTES as u64,
        "Observed native transcript exceeds its capture budget"
    );
    let read = observe(
        previous,
        &carrier,
        &mut file,
        (&entry.runtime, &entry.thread_id, &path),
        size,
        now,
    )?;
    let progress = read.progress;
    let Some(boundary) = read.boundary else {
        return Ok(Some(Recorded {
            digest: previous
                .filter(|_| progress.is_some())
                .and_then(|previous| previous.digest.clone()),
            carrier,
            progress,
        }));
    };
    let digest = read.digest.context("a completed boundary has its digest")?;
    let recorded = previous.and_then(|previous| previous.digest.as_deref());
    if recorded == Some(digest.as_str()) {
        return Ok(Some(Recorded {
            digest: Some(digest),
            carrier,
            progress,
        }));
    }
    let job = crate::rc::archive_jobs::Job::capture(
        logical,
        &entry.thread_id,
        &entry.runtime,
        entry.native_source.clone(),
        std::path::Path::new(&entry.cwd),
        lineage,
        &format!("observed-{digest}"),
        &path,
        boundary,
        None,
    )?;
    job.record()?;
    Ok(Some(Recorded {
        digest: Some(digest),
        carrier,
        progress,
    }))
}

/// Reads the transcript `carrier` describes, `size` bytes long, from where [`carried_progress`]
/// says this read may start, as [`read_from`] does.
fn observe(
    previous: Option<&Recorded>,
    carrier: &Carrier,
    file: &mut std::fs::File,
    (runtime, thread, path): (&str, &str, &std::path::Path),
    size: u64,
    now: tokio::time::Instant,
) -> crate::Result<Reading> {
    let carried = carried_progress(previous, carrier, file, now);
    read_from(runtime, thread, path, file, size, carried, now)
}

/// Where this read of `carrier` may start: past the completed turn `previous` ended at, while
/// `carrier` is the file that was read then, longer, or as long and not written since, the byte
/// before that end still ends a record, and the transcript was read whole less than
/// [`WHOLE_READ_EVERY`] ago. `None` reads it whole. A transcript only ever grows by appending, so
/// one written without growing was rewritten in place.
fn carried_progress(
    previous: Option<&Recorded>,
    carrier: &Carrier,
    file: &mut std::fs::File,
    now: tokio::time::Instant,
) -> Option<Progress> {
    let previous = previous?;
    let progress = previous.progress.clone()?;
    (previous.carrier.same_file(carrier)
        && (carrier.len > previous.carrier.len || carrier.modified == previous.carrier.modified)
        && now < progress.whole_at + WHOLE_READ_EVERY
        && ends_record(file, progress.end))
    .then_some(progress)
}

/// Whether the byte before `end` in `file` ends a record, as a completed boundary does.
fn ends_record(file: &mut std::fs::File, end: u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let mut last = [0u8];
    end > 0
        && file.seek(SeekFrom::Start(end - 1)).is_ok()
        && file.read_exact(&mut last).is_ok()
        && last == *b"\n"
}

/// What one read of a transcript found: the last completed boundary past where it started and
/// that boundary's digest, if there is one, and where the next read may start.
struct Reading {
    /// How many bytes of the transcript were read.
    #[cfg(test)]
    bytes_read: u64,
    boundary: Option<u64>,
    digest: Option<String>,
    progress: Option<Progress>,
}

/// Reads `file`, `size` bytes long, from the end of its last completed turn when `carried` says
/// where that is, or whole, and finds the last completed boundary in what it read. A boundary is
/// only ever found past the start: what came before ended a completed turn, so the turn running
/// and what was appended alone can close another. Its digest covers the whole transcript up to
/// it, the carried state extended over what was read.
fn read_from(
    runtime: &str,
    thread: &str,
    path: &std::path::Path,
    file: &mut std::fs::File,
    size: u64,
    carried: Option<Progress>,
    now: tokio::time::Instant,
) -> crate::Result<Reading> {
    use sha2::{Digest, Sha256};
    use std::io::{Read as _, Seek, SeekFrom};
    let (start, hasher, whole_at) = match carried {
        Some(progress) => (progress.end, progress.hasher, progress.whole_at),
        None => (0, Sha256::new(), now),
    };
    let read_past = start > 0;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![];
    file.take(size - start).read_to_end(&mut bytes)?;
    let unchanged = |hasher: Sha256| Progress {
        end: start,
        hasher,
        whole_at,
        read_past,
    };
    let Some(last) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(Reading {
            #[cfg(test)]
            bytes_read: bytes.len() as u64,
            boundary: None,
            digest: None,
            progress: (start > 0).then(|| unchanged(hasher)),
        });
    };
    let text = std::str::from_utf8(&bytes[..=last])?;
    let found = if start == 0 {
        crate::rc::archive_jobs::completed_end(runtime, thread, path, text)?
    } else {
        completed_after(runtime, thread, path, start, &hasher, text)?.map(|end| start + end)
    };
    let Some(boundary) = found.filter(|boundary| *boundary > start) else {
        return Ok(Reading {
            #[cfg(test)]
            bytes_read: bytes.len() as u64,
            boundary: None,
            digest: None,
            progress: (start > 0).then(|| unchanged(hasher)),
        });
    };
    let mut hasher = hasher;
    hasher.update(&bytes[..(boundary - start) as usize]);
    let digest = format!("{:x}", hasher.clone().finalize());
    Ok(Reading {
        #[cfg(test)]
        bytes_read: bytes.len() as u64,
        boundary: Some(boundary),
        digest: Some(digest),
        progress: Some(Progress {
            end: boundary,
            hasher,
            whole_at,
            read_past,
        }),
    })
}

/// [`crate::rc::archive_jobs::completed_end`] for `text`, the records that follow `start`, the
/// end of a completed turn, given the SHA-256 state over the bytes before it: relative to `start`.
fn completed_after(
    runtime: &str,
    thread: &str,
    path: &std::path::Path,
    start: u64,
    hasher: &sha2::Sha256,
    text: &str,
) -> crate::Result<Option<u64>> {
    let boundary = crate::commands::commit::completed_native_boundary(runtime, text)?;
    #[cfg(unix)]
    if runtime == "claude-code" {
        let start = usize::try_from(start).context("a transcript offset is out of range")?;
        let completed = crate::rc::native_claude::completion::boundaries_after(
            thread,
            path,
            start,
            hasher,
            text.as_bytes(),
        )?;
        return Ok(boundary.max(completed.last().map(|end| (end - start) as u64)));
    }
    #[cfg(not(unix))]
    let _ = (thread, path, start, hasher);
    Ok(boundary)
}

/// Whether the conversation `native` of `runtime`, running in `root` inside the project folder
/// `project` an RC workspace binds, may be saved and uploaded without the person asking: as the
/// agit folder governing `root` decides, else by that binding (see
/// [`crate::rc::capture::consented`]); never one the project folder held when its uploads were
/// turned on ([`crate::rc::folder_cutoff::leaves_out`], the rule its publication is held back by
/// too); and for Codex only once AgentGit saw a hook of that very conversation.
fn capture_allowed(
    runtime: &str,
    project: &std::path::Path,
    root: &std::path::Path,
    native: &str,
) -> bool {
    crate::commands::project::folder_decision(root, runtime, native).unwrap_or(true)
        && !crate::rc::folder_cutoff::leaves_out(project, runtime, native)
        && (runtime != "codex" || crate::commands::project::codex_capture_allowed(root, native))
}

impl Daemon {
    fn reserve_observed_capture(
        &mut self,
        logical: &str,
    ) -> Option<super::capture_binding::Request> {
        if !self.opts.local_owner || !self.settlement.borrow().local_owner {
            return None;
        }
        let entry = self.roster.get(logical)?.clone();
        let project_id = entry.project_id.as_ref()?;
        let project = self.mirror.project_path(&entry.workspace_id, project_id)?;
        let cwd = PathBuf::from(&entry.cwd);
        if !cwd.starts_with(&project)
            || !capture_allowed(&entry.runtime, &project, &cwd, &entry.thread_id)
        {
            return None;
        }
        let conflicts = |runtime: &str,
                         source: Option<&crate::protocol::NativeSourceRef>,
                         native: Option<&str>| {
            runtime == entry.runtime
                && crate::protocol::NativeSourceRef::same_store(
                    source,
                    entry.native_source.as_ref(),
                )
                && native == Some(entry.thread_id.as_str())
        };
        if self.sessions.contains_key(logical)
            || self.opening_sessions.contains_key(logical)
            || self.archive_recovering.contains_key(logical)
            || self.sessions.values().any(|live| {
                conflicts(
                    &live.info.runtime,
                    live.info.native_source.as_ref(),
                    live.runtime_thread_id.as_deref(),
                )
            })
            || self.opening_sessions.values().any(|opening| {
                conflicts(
                    &opening.runtime,
                    opening.native_source.as_ref(),
                    opening.native_id.as_deref(),
                )
            })
            || self
                .archive_recovering
                .values()
                .any(|job| conflicts(&job.runtime, job.native_source.as_ref(), Some(&job.native)))
        {
            return None;
        }
        self.opening_sessions.insert(
            logical.into(),
            LaunchReservation {
                generation: 0,
                runtime: entry.runtime.clone(),
                native_source: entry.native_source.clone(),
                native_id: Some(entry.thread_id.clone()),
            },
        );
        Some(super::capture_binding::Request {
            logical: logical.into(),
            runtime: entry.runtime.clone(),
            native: entry.thread_id.clone(),
            native_source: entry.native_source.clone(),
            cwd,
            project: Some((project_id.clone(), project)),
            prior_entry: Some(entry),
            prior_capture: self.roster.captures.get(logical).cloned(),
            wire: None,
            authority: crate::rc::authority::Guard::default(),
            settlement: self.settlement.subscribe(),
        })
    }

    pub(super) fn retain_observed_capture(
        &mut self,
        info: &SessionInfo,
        cwd: &std::path::Path,
    ) -> crate::Result<Option<SessionInfo>> {
        if !self.opts.local_owner || info.workspace_id != crate::rc::endpoint::WORKSPACE {
            return Ok(None);
        }
        let Some(project) = &info.project_id else {
            return Ok(None);
        };
        let root = self
            .mirror
            .project_path(&info.workspace_id, project)
            .context("Observed project is no longer bound")?;
        ensure!(
            cwd.starts_with(&root),
            "Observed session is outside its project"
        );
        let native = info
            .runtime_session_id
            .as_deref()
            .context("Observed native identity is missing")?;
        // A conversation the folder leaves out, or a Codex one whose own hooks never ran, is not
        // saved in the background: watching it doesn't change that, and it stays on this
        // computer until the person saves it.
        if !capture_allowed(&info.runtime, &root, cwd, native) {
            return Ok(None);
        }
        let logical = self
            .roster
            .logical_for_thread_in(
                &info.runtime,
                info.native_source
                    .as_ref()
                    .map(|source| source.source_id.as_str()),
                native,
                &info.workspace_id,
            )
            .unwrap_or_else(crate::domain::meta::mint_session_id);
        let mut roster = self.roster.clone();
        let mut entry = match roster.get(&logical) {
            Some(entry) => {
                ensure!(
                    entry.thread_id == native
                        && crate::protocol::NativeSourceRef::same_store(
                            entry.native_source.as_ref(),
                            info.native_source.as_ref(),
                        )
                        && entry.cwd == cwd.to_string_lossy(),
                    "Observed session binding changed"
                );
                entry.clone()
            }
            None => roster::Entry {
                native_source: info.native_source.clone(),
                runtime: info.runtime.clone(),
                thread_id: native.into(),
                cwd: cwd.to_string_lossy().into(),
                workspace_id: info.workspace_id.clone(),
                project_id: Some(project.clone()),
                agit_session: None,
                expected_agent_id: None,
                permission_mode: info.permission_mode,
                guard_attempts: Default::default(),
                prior_threads: vec![],
                ever_dangerous: info.dangerous,
                project_share: false,
            },
        };
        let changed_danger = info.dangerous && !entry.ever_dangerous;
        entry.ever_dangerous |= info.dangerous;
        let mut archive = info.clone();
        archive.session_id = logical.clone();
        if let Some((session, identity)) = entry
            .agit_session
            .as_deref()
            .zip(entry.expected_agent_id.as_deref())
        {
            let lineage = crate::rc::lineage::AgitSession::parse(session, identity)?;
            archive.agent = Some(lineage.slug());
            archive.branch = Some(lineage.branch().into());
        }
        let row = super::dormant_row(&logical, &entry);
        roster.record(&logical, entry)?;
        if roster.observed.insert(logical) || changed_danger {
            roster.save()?;
            self.roster = roster;
            self.list_changes.session(row);
        }
        Ok(Some(archive))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One turn of a transcript in `runtime`'s records: a request, a tool step, and the reply that
    /// ends it.
    fn turn(runtime: &str, n: usize) -> Vec<String> {
        let lines = match runtime {
            "codex" => vec![
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user",
                    "content":[{"type":"input_text","text":format!("request {n}")}]}}),
                serde_json::json!({"type":"response_item","payload":{"type":"function_call",
                    "name":"shell","call_id":format!("call-{n}"),"arguments":"{}"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"function_call_output",
                    "call_id":format!("call-{n}"),"output":"done"}}),
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":format!("answer {n}")}]}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            ],
            _ => vec![
                serde_json::json!({"type":"user","uuid":format!("u{n}"),
                    "message":{"role":"user","content":format!("request {n}")}}),
                serde_json::json!({"type":"assistant","uuid":format!("a{n}"),
                    "message":{"role":"assistant","stop_reason":"tool_use",
                        "content":[{"type":"tool_use","id":format!("t{n}"),"name":"Bash","input":{}}]}}),
                serde_json::json!({"type":"user","uuid":format!("r{n}"),
                    "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":format!("t{n}"),"content":"done"}]}}),
                serde_json::json!({"type":"assistant","uuid":format!("e{n}"),
                    "message":{"role":"assistant","stop_reason":"end_turn",
                        "content":[{"type":"text","text":format!("answer {n}")}]}}),
            ],
        };
        lines.into_iter().map(|line| format!("{line}\n")).collect()
    }

    /// The carrier a read of the transcript at `path` records, without a runtime's receipts.
    fn carrier_of(path: &std::path::Path) -> Carrier {
        let metadata = std::fs::metadata(path).unwrap();
        Carrier {
            path: normalized(path),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            file: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
            receipts: vec![],
        }
    }

    /// The boundary and the digest over everything before it that a read leaves for the next.
    fn state(reading: &Reading) -> Option<(u64, String)> {
        use sha2::Digest as _;
        reading.progress.as_ref().map(|progress| {
            (
                progress.end,
                format!("{:x}", progress.hasher.clone().finalize()),
            )
        })
    }

    /// Read from the end of the last completed turn, a transcript that grows a line or a part of
    /// one at a time, through several turns, leaves at every step the boundary and digest a whole
    /// read of it leaves, and asks the file for no more than what follows that turn. A read that
    /// started elsewhere or carried the wrong state leaves another boundary or digest; one that
    /// read the whole file again asks for more. What it leaves says whether it read only past a
    /// turn, which decides when the collector reads an unchanged transcript whole again.
    #[test]
    fn observed_capture_reads_only_what_follows_the_last_completed_turn() {
        for runtime in ["codex", "claude-code"] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("transcript.jsonl");
            std::fs::write(&path, "").unwrap();
            let thread = "71717171-7171-4171-8171-717171717171";
            let now = tokio::time::Instant::now();
            let mut written = String::new();
            let mut previous: Option<Recorded> = None;
            let mut shorter = 0;
            for n in 0..6 {
                for line in turn(runtime, n) {
                    // Each record arrives in two writes, so some reads end inside a record.
                    let (head, tail) = line.split_at(line.len() / 2);
                    for part in [head, tail] {
                        written.push_str(part);
                        std::fs::write(&path, &written).unwrap();
                        let size = written.len() as u64;
                        let start = previous
                            .as_ref()
                            .and_then(|previous| previous.progress.as_ref())
                            .map_or(0, |progress| progress.end);
                        let carrier = carrier_of(&path);
                        let mut file = std::fs::File::open(&path).unwrap();
                        let read = observe(
                            previous.as_ref(),
                            &carrier,
                            &mut file,
                            (runtime, thread, &path),
                            size,
                            now,
                        )
                        .unwrap();
                        let mut file = std::fs::File::open(&path).unwrap();
                        let whole =
                            read_from(runtime, thread, &path, &mut file, size, None, now).unwrap();
                        assert_eq!(state(&read), state(&whole), "{runtime} at {size}");
                        assert_eq!(read.bytes_read, size - start, "{runtime} at {size}");
                        shorter += usize::from(read.bytes_read < whole.bytes_read);
                        if let Some(progress) = &read.progress {
                            assert_eq!(progress.read_past, start > 0, "{runtime} at {size}");
                        }
                        previous = Some(Recorded {
                            digest: None,
                            carrier,
                            progress: read.progress,
                        });
                    }
                }
            }
            assert!(
                shorter > 0,
                "{runtime}: no read started past a completed turn"
            );
            assert_eq!(
                previous
                    .and_then(|previous| previous.progress)
                    .map(|progress| progress.end),
                Some(written.len() as u64),
                "{runtime}: the last turn completed"
            );
        }
    }

    /// The next read starts past the last completed turn only while the transcript is the file
    /// that was read, grown or not written since, still ending a record there, and read whole
    /// within [`WHOLE_READ_EVERY`]; otherwise it reads the whole file. A check that let a
    /// replaced, truncated or rewritten file, or a stale state, through would read past a boundary
    /// that isn't there, or hash bytes that changed.
    #[tokio::test(start_paused = true)]
    async fn the_next_read_starts_past_the_last_turn_only_in_the_file_that_was_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transcript.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let carrier = carrier_of;
        let previous = Recorded {
            digest: Some("digest".into()),
            carrier: carrier(&path),
            progress: Some(Progress {
                end: 8,
                hasher: sha2::Sha256::default(),
                whole_at: tokio::time::Instant::now(),
                read_past: false,
            }),
        };
        let carried = |previous: &Recorded| {
            let mut file = std::fs::File::open(&path).unwrap();
            carried_progress(
                Some(previous),
                &carrier(&path),
                &mut file,
                tokio::time::Instant::now(),
            )
            .map(|progress| progress.end)
        };

        // Rewritten in place, its length kept.
        std::fs::write(&path, "{\"a\":9}\n{\"b\":2}\n").unwrap();
        let written = previous.carrier.modified.unwrap() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(written)
            .unwrap();
        assert_eq!(carried(&previous), None);
        // The same file, grown.
        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        assert_eq!(carried(&previous), Some(8));
        // No longer a record end at the boundary.
        std::fs::write(&path, "{\"a\":10}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        assert_eq!(carried(&previous), None);
        // Shorter than when it was read.
        std::fs::write(&path, "{\"a\":1}\n").unwrap();
        assert_eq!(carried(&previous), None);
        // Another file in its place.
        std::fs::write(&path, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        #[cfg(unix)]
        {
            let replacement = directory.path().join("replacement.jsonl");
            std::fs::write(&replacement, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
            std::fs::rename(&replacement, &path).unwrap();
            assert_eq!(carried(&previous), None);
        }
        // The same file again, but read whole too long ago.
        let previous = Recorded {
            carrier: carrier(&path),
            ..previous
        };
        assert_eq!(carried(&previous), Some(8));
        tokio::time::advance(WHOLE_READ_EVERY).await;
        assert_eq!(carried(&previous), None);
    }

    /// The collector skips a conversation only while the carrier it read is still the thread's
    /// current carrier and unchanged, and not while a read only past its last turn has gone
    /// [`WHOLE_READ_EVERY`] without a whole read. Comparing only the old file would miss a Codex
    /// segment rollover, treating a missing file as changed would restore every cold rollout, and
    /// skipping on the carrier alone leaves a transcript that went quiet after such a read
    /// unchecked until it changes.
    #[test]
    fn unchanged_carrier_skips_reads_until_it_grows_rolls_over_or_is_restored() {
        let _environment = crate::infra::config::env_lock();
        let previous = std::env::var_os("CODEX_HOME");
        let home = tempfile::tempdir().unwrap();
        // SAFETY: this test holds the process-wide configuration lock.
        unsafe { std::env::set_var("CODEX_HOME", home.path()) };
        let restore_home = scopeguard(move || match previous {
            // SAFETY: the configuration lock is still held.
            Some(previous) => unsafe { std::env::set_var("CODEX_HOME", previous) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        });
        let thread = "70707070-7070-4070-8070-707070707070";
        let directory = home.path().join("sessions/2026/10/01");
        std::fs::create_dir_all(&directory).unwrap();
        let root = directory.join(format!("rollout-2026-10-01T00-00-00-{thread}.jsonl"));
        std::fs::write(&root, "{}\n").unwrap();
        let database = rusqlite::Connection::open(home.path().join("state_5.sqlite")).unwrap();
        database
            .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER)")
            .unwrap();
        database
            .execute(
                "INSERT INTO threads (id, rollout_path, archived) VALUES (?1, ?2, 0)",
                [thread, root.to_str().unwrap()],
            )
            .unwrap();
        let entry: roster::Entry = serde_json::from_value(serde_json::json!({
            "runtime": "codex", "thread_id": thread, "cwd": home.path(), "workspace_id": "local",
        }))
        .unwrap();
        let read =
            |path: &std::path::Path| Carrier::of(&entry, path, &std::fs::metadata(path).unwrap());

        let carrier = read(&root);
        assert!(carrier.current_and_unchanged(&entry));
        // Read only past its last turn, an unchanged transcript is still read whole once
        // `WHOLE_READ_EVERY` has passed; read whole since, it is passed over again.
        let at = tokio::time::Instant::now();
        let recorded = |read_past| Recorded {
            digest: None,
            carrier: carrier.clone(),
            progress: Some(Progress {
                end: 3,
                hasher: sha2::Sha256::default(),
                whole_at: at,
                read_past,
            }),
        };
        assert!(recorded(true).unchanged(&entry, at));
        assert!(!recorded(true).unchanged(&entry, at + WHOLE_READ_EVERY));
        assert!(recorded(false).unchanged(&entry, at + WHOLE_READ_EVERY));
        std::fs::write(&root, "{}\n{}\n").unwrap();
        assert!(
            !carrier.current_and_unchanged(&entry),
            "an append is new content"
        );

        let carrier = read(&root);
        std::fs::write(
            root.with_extension("jsonl.zst"),
            ruzstd::encoding::compress_to_vec(
                &b"{}\n{}\n"[..],
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .unwrap();
        std::fs::remove_file(&root).unwrap();
        assert!(
            carrier.current_and_unchanged(&entry),
            "a cold carrier is unchanged"
        );
        assert!(
            !root.exists(),
            "checking a cold carrier must not restore it"
        );

        let segment = directory.join(format!(
            "rollout-2026-10-01T00-00-00-{thread}_80808080-8080-4080-8080-808080808080.jsonl"
        ));
        std::fs::write(&segment, "{}\n").unwrap();
        database
            .execute(
                "UPDATE threads SET rollout_path = ?1 WHERE id = ?2",
                [segment.to_str().unwrap(), thread],
            )
            .unwrap();
        assert!(
            !carrier.current_and_unchanged(&entry),
            "a rollover moves the current carrier"
        );
        drop(restore_home);
    }

    /// A conversation whose capture records no carrier is read again only after a delay that
    /// doubles up to its cap, and a capture that records one ends the delay. Without it, every
    /// cycle reads and hashes the whole transcript of each such conversation again.
    #[test]
    fn captures_without_a_carrier_back_off_until_one_is_recorded() {
        let (first, longest) = DELAYS;
        let mut collector = Collector::default();
        let start = tokio::time::Instant::now();
        assert!(!collector.is_delayed("logical", start));
        collector.settled("logical", false, false, start);
        assert!(collector.is_delayed("logical", start + first / 2));
        assert!(!collector.is_delayed("logical", start + first));
        assert!(!collector.is_delayed("other", start));
        collector.settled("logical", false, false, start);
        assert!(collector.is_delayed("logical", start + first));
        assert!(!collector.is_delayed("logical", start + first * 2));
        for _ in 0..32 {
            collector.settled("logical", false, false, start);
        }
        assert!(collector.is_delayed("logical", start + longest / 2));
        assert!(!collector.is_delayed("logical", start + longest));
        collector.settled("logical", true, false, start);
        assert!(!collector.is_delayed("logical", start));
    }

    /// A capture that ended only because the privacy worker did not answer in time is tried again
    /// a few times a short delay apart, and only then does the doubling delay begin; a capture
    /// that failed otherwise starts the doubling at once, and one that records a carrier gives
    /// the quick tries back. Without them a new conversation whose landing met a worker still
    /// starting would wait half a minute, then a minute, for its finished turn.
    #[test]
    fn a_capture_that_met_a_slow_privacy_worker_is_tried_again_shortly() {
        use super::super::archive_recovery::{QUICK_DELAY, QUICK_TRIES};
        let (first, _) = DELAYS;
        let mut collector = Collector::default();
        let start = tokio::time::Instant::now();
        let mut now = start;
        for _ in 0..QUICK_TRIES {
            collector.settled("logical", false, true, now);
            assert!(collector.in_quick("logical"));
            assert!(collector.is_delayed("logical", now + QUICK_DELAY / 2));
            now += QUICK_DELAY;
            assert!(!collector.is_delayed("logical", now));
        }
        collector.settled("logical", false, true, now);
        assert!(!collector.in_quick("logical"));
        assert!(collector.is_delayed("logical", now + QUICK_DELAY));
        assert!(!collector.is_delayed("logical", now + first));
        collector.settled("logical", true, false, now);
        collector.settled("logical", false, true, now);
        assert!(!collector.is_delayed("logical", now + QUICK_DELAY));

        collector.settled("other", false, false, start);
        assert!(!collector.in_quick("other"));
        assert!(collector.is_delayed("other", start + QUICK_DELAY));
        collector.settled("other", false, true, start);
        assert!(collector.is_delayed("other", start + first));
    }

    /// A Claude Code turn can end by a completion receipt alone, with the transcript unchanged,
    /// so its carrier changes when a receipt arrives. A carrier made of file metadata only
    /// would skip that conversation until the transcript grew again.
    #[cfg(unix)]
    #[test]
    fn a_completion_receipt_changes_an_unchanged_claude_carrier() {
        let _environment = crate::infra::config::env_lock();
        let previous = std::env::var_os("AGIT_HOME");
        let home = tempfile::tempdir().unwrap();
        // SAFETY: this test holds the process-wide configuration lock.
        unsafe { std::env::set_var("AGIT_HOME", home.path()) };
        let restore_home = scopeguard(move || match previous {
            // SAFETY: the configuration lock is still held.
            Some(previous) => unsafe { std::env::set_var("AGIT_HOME", previous) },
            None => unsafe { std::env::remove_var("AGIT_HOME") },
        });
        let thread = "71717171-7171-4171-8171-717171717171";
        let transcript = home.path().join("native.jsonl");
        std::fs::write(&transcript, "{\"type\":\"user\"}\n").unwrap();
        let entry: roster::Entry = serde_json::from_value(serde_json::json!({
            "runtime": "claude-code", "thread_id": thread, "cwd": home.path(), "workspace_id": "local",
        }))
        .unwrap();
        let read = || {
            Carrier::of(
                &entry,
                &transcript,
                &std::fs::metadata(&transcript).unwrap(),
            )
        };

        let carrier = read();
        assert_eq!(read(), carrier);
        let receipts = crate::rc::native_claude::completion::receipt_directory(thread, &transcript);
        std::fs::create_dir_all(&receipts).unwrap();
        std::fs::write(receipts.join("generation-turn.json"), "{}").unwrap();
        assert_ne!(read(), carrier, "a receipt can end a turn");
        drop(restore_home);
    }

    fn scopeguard(run: impl FnOnce()) -> impl Drop {
        struct Guard<F: FnOnce()>(Option<F>);
        impl<F: FnOnce()> Drop for Guard<F> {
            fn drop(&mut self) {
                if let Some(run) = self.0.take() {
                    run();
                }
            }
        }
        Guard(Some(run))
    }

    /// A Codex conversation watched from the Hub is taken in and collected only when AgentGit saw a
    /// hook of that very conversation. Hooks that ran for one conversation in the folder, then
    /// stopped being trusted, don't let a later one in: it stays on this computer.
    #[test]
    fn a_codex_conversation_is_taken_in_only_once_its_own_hooks_ran() {
        if crate::rc::in_isolated_test(
            "rc::daemon::observed_capture::tests::a_codex_conversation_is_taken_in_only_once_its_own_hooks_ran",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let project = root.path().canonicalize().unwrap();
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.opts.local_owner = true;
            state
                .settlement
                .send_modify(|authority| authority.local_owner = true);
            state
                .mirror
                .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
                .unwrap();
            let watched = |name: &str, native: &str| {
                let (tx, _rx) = mpsc::channel(1);
                let mut live = super::super::tests::rpc_test_live(
                    name,
                    1,
                    tx,
                    crate::protocol::PermissionMode::Default,
                );
                assert_eq!(live.info.runtime, "codex");
                live.info.workspace_id = crate::rc::endpoint::WORKSPACE.into();
                live.info.project_id = Some("project".into());
                live.info.runtime_session_id = Some(native.into());
                live.runtime_thread_id = Some(native.into());
                live
            };
            let hooked = watched("hooked", "hooked-native");
            let unhooked = watched("unhooked", "unhooked-native");
            for live in [&hooked, &unhooked] {
                assert!(
                    state
                        .retain_observed_capture(&live.info, &project)
                        .unwrap()
                        .is_none()
                );
            }
            assert!(state.roster.observed.is_empty(), "nothing is taken in");

            // The first conversation's hooks run; then the person untrusts them, and the second
            // runs without.
            crate::commands::project::codex_hooks_seen("hooked-native");
            assert!(
                state
                    .retain_observed_capture(&unhooked.info, &project)
                    .unwrap()
                    .is_none(),
                "a conversation whose own hooks never ran stays out"
            );
            let taken = state
                .retain_observed_capture(&hooked.info, &project)
                .unwrap()
                .expect("taken in once its own hooks ran");
            assert_eq!(state.roster.observed.len(), 1);
            assert!(state.roster.observed.contains(&taken.session_id));
            assert!(state.reserve_observed_capture(&taken.session_id).is_some());
            state.opening_sessions.remove(&taken.session_id);

            // Without the record of its hooks, a conversation already taken in is not collected.
            std::fs::remove_file(
                root.path()
                    .join("agit/hooks-seen/codex-sessions/hooked-native"),
            )
            .unwrap();
            assert!(
                state.reserve_observed_capture(&taken.session_id).is_none(),
                "not collected without its own hooks"
            );
        });
    }

    /// A watched conversation the folder's policy leaves out is neither taken in nor collected:
    /// one the person dropped, and any while the folder doesn't upload by itself or is off.
    /// Watching it from the Hub never lifts that.
    #[test]
    fn a_watched_conversation_the_folder_leaves_out_is_not_taken_in() {
        if crate::rc::in_isolated_test(
            "rc::daemon::observed_capture::tests::a_watched_conversation_the_folder_leaves_out_is_not_taken_in",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let project = root.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.opts.local_owner = true;
            state
                .settlement
                .send_modify(|authority| authority.local_owner = true);
            state
                .mirror
                .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
                .unwrap();
            let watched = |name: &str, native: &str| {
                let (tx, _rx) = mpsc::channel(1);
                let mut live = super::super::tests::rpc_test_live(
                    name,
                    1,
                    tx,
                    crate::protocol::PermissionMode::Default,
                );
                live.info.workspace_id = crate::rc::endpoint::WORKSPACE.into();
                live.info.project_id = Some("project".into());
                live.info.runtime_session_id = Some(native.into());
                live.runtime_thread_id = Some(native.into());
                crate::commands::project::codex_hooks_seen(native);
                live
            };
            let runtime = watched("probe", "probe-native").info.runtime;
            let kept = watched("kept", "kept-native");
            let dropped = watched("dropped", "dropped-native");
            crate::commands::project::policy_for_test(
                &project,
                true,
                true,
                &[(runtime.as_str(), "dropped-native")],
            );
            assert!(
                state
                    .retain_observed_capture(&dropped.info, &project)
                    .unwrap()
                    .is_none(),
                "a dropped conversation is not taken in"
            );
            let taken = state
                .retain_observed_capture(&kept.info, &project)
                .unwrap()
                .expect("a conversation the folder keeps is taken in");
            assert_eq!(state.roster.observed.len(), 1);
            assert!(state.reserve_observed_capture(&taken.session_id).is_some());
            state.opening_sessions.remove(&taken.session_id);

            // Dropped after it was taken in, it is not collected.
            crate::commands::project::policy_for_test(
                &project,
                true,
                true,
                &[
                    (runtime.as_str(), "dropped-native"),
                    (runtime.as_str(), "kept-native"),
                ],
            );
            assert!(
                state.reserve_observed_capture(&taken.session_id).is_none(),
                "a conversation dropped after it was taken in is not collected"
            );

            // A subfolder with its own setting decides for the conversations that run in it.
            let sub = project.join("sub");
            std::fs::create_dir(&sub).unwrap();
            crate::commands::project::policy_for_test(
                &sub,
                true,
                true,
                &[(runtime.as_str(), "sub-native")],
            );
            let in_sub = watched("in-sub", "sub-native");
            assert!(
                state
                    .retain_observed_capture(&in_sub.info, &sub)
                    .unwrap()
                    .is_none(),
                "the subfolder's own setting keeps it out"
            );

            // A folder that doesn't upload by itself, or is off, keeps every conversation out.
            for (enabled, auto_upload) in [(true, false), (false, true)] {
                crate::commands::project::policy_for_test(&project, enabled, auto_upload, &[]);
                let later = watched("later", "later-native");
                assert!(
                    state
                        .retain_observed_capture(&later.info, &project)
                        .unwrap()
                        .is_none(),
                    "taken in from a folder with enabled {enabled}, auto_upload {auto_upload}"
                );
                assert!(state.reserve_observed_capture(&taken.session_id).is_none());
            }
        });
    }

    /// A conversation the folder held when its uploads were turned on is not taken in when it is
    /// watched from the Hub, later turns included, and still not after the daemon restarts; one
    /// started afterwards is. Once the person saves it themselves, it is taken in too.
    #[test]
    fn a_conversation_from_before_the_folder_was_turned_on_is_not_taken_in() {
        if crate::rc::in_isolated_test(
            "rc::daemon::observed_capture::tests::a_conversation_from_before_the_folder_was_turned_on_is_not_taken_in",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _context = runtime.enter();
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let watched = |name: &str, native: &str| {
            let (tx, _rx) = mpsc::channel(1);
            let mut live = super::super::tests::rpc_test_live(
                name,
                1,
                tx,
                crate::protocol::PermissionMode::Default,
            );
            live.info.workspace_id = crate::rc::endpoint::WORKSPACE.into();
            live.info.project_id = Some("project".into());
            live.info.runtime_session_id = Some(native.into());
            live.runtime_thread_id = Some(native.into());
            crate::commands::project::codex_hooks_seen(native);
            live
        };
        let kind = watched("probe", "probe-native").info.runtime;
        crate::rc::folder_cutoff::record(&project, [format!("{kind}:before-native")]).unwrap();
        // Each run is a daemon started afresh, with only what it keeps on disk.
        let start = || {
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let project = project.clone();
            async move {
                {
                    let mut state = daemon.lock().await;
                    state.opts.local_owner = true;
                    state
                        .settlement
                        .send_modify(|authority| authority.local_owner = true);
                    state
                        .mirror
                        .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
                        .unwrap();
                }
                daemon
            }
        };
        runtime.block_on(async {
            let daemon = start().await;
            let mut state = daemon.lock().await;
            let before = watched("before", "before-native");
            assert!(
                state
                    .retain_observed_capture(&before.info, &project)
                    .unwrap()
                    .is_none(),
                "a conversation from before is not taken in"
            );
            let after = watched("after", "after-native");
            let taken = state
                .retain_observed_capture(&after.info, &project)
                .unwrap()
                .expect("a conversation started afterwards is taken in");
            assert!(state.reserve_observed_capture(&taken.session_id).is_some());
        });
        runtime.block_on(async {
            let daemon = start().await;
            let mut state = daemon.lock().await;
            let before = watched("before", "before-native");
            assert!(
                state
                    .retain_observed_capture(&before.info, &project)
                    .unwrap()
                    .is_none(),
                "nor after the daemon restarts"
            );
            crate::rc::folder_cutoff::admit(&project, &kind, "before-native").unwrap();
            assert!(
                state
                    .retain_observed_capture(&before.info, &project)
                    .unwrap()
                    .is_some(),
                "saved by the person, it is taken in"
            );
        });
    }

    #[test]
    fn observed_identity_survives_viewers_and_respects_native_writer_and_project_fences() {
        if crate::rc::in_isolated_test(
            "rc::daemon::observed_capture::tests::observed_identity_survives_viewers_and_respects_native_writer_and_project_fences",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        crate::rc::select_local_authority();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            crate::commands::project::codex_hooks_seen("native");
            let project = root.path().canonicalize().unwrap();
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.lock().await;
            state.opts.local_owner = true;
            state
                .settlement
                .send_modify(|authority| authority.local_owner = true);
            state
                .mirror
                .bind(crate::rc::endpoint::WORKSPACE, "project", &project)
                .unwrap();
            let (tx, _rx) = mpsc::channel(1);
            let mut native = super::super::tests::rpc_test_live(
                "watch",
                1,
                tx,
                crate::protocol::PermissionMode::Default,
            );
            native.info.workspace_id = crate::rc::endpoint::WORKSPACE.into();
            native.info.project_id = Some("project".into());
            native.info.runtime_session_id = Some("native".into());
            native.runtime_thread_id = Some("native".into());
            let first = state
                .retain_observed_capture(&native.info, &project)
                .unwrap()
                .unwrap();
            assert_ne!(first.session_id, native.info.session_id);
            assert!(state.sessions.is_empty());
            assert!(state.watches.is_empty());
            native.info.dangerous = true;
            let repeated = state
                .retain_observed_capture(&native.info, &project)
                .unwrap()
                .unwrap();
            assert_eq!(first.session_id, repeated.session_id);
            let saved = Roster::try_load().unwrap();
            assert!(saved.observed.contains(&first.session_id));
            assert!(saved.get(&first.session_id).unwrap().ever_dangerous);
            state.roster = saved;
            state.sessions.insert("other-alias".into(), native);
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
            state.sessions.clear();
            assert!(state.reserve_observed_capture(&first.session_id).is_some());
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
            let failure = state
                .require_launch_slot(
                    &first,
                    &LaunchSpec {
                        cwd: project.clone(),
                        resume_from: Some("native".into()),
                        agit_session: None,
                        model: None,
                        dangerous: false,
                        permission_mode: None,
                        attach_only: false,
                        project_share: false,
                    },
                )
                .err()
                .unwrap();
            assert_eq!(failure.error.data.as_ref().unwrap()["retryable"], true);
            assert_eq!(failure.error.data.as_ref().unwrap()["outcome"], "not_sent");
            state.opening_sessions.clear();
            state
                .mirror
                .unbind(crate::rc::endpoint::WORKSPACE, "project");
            assert!(state.reserve_observed_capture(&first.session_id).is_none());
        });
    }
}
