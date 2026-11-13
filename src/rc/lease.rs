//! One writer per Claude Code conversation: which process holds it, and whether a terminal that
//! took it over loaded everything the conversation holds.
//!
//! A terminal (TUI) that resumes a conversation the daemon runs headless takes it over at the
//! headless turn boundary. Claude Code keeps a conversation as a tree of records, and a terminal
//! continues one branch of it. With the AgentGit plugin on both sides, a terminal is judged by
//! that branch: it is current only if the newest record it appended descends from the headless
//! writer's last record. A terminal that loaded another branch (one a terminal opened mid-turn
//! wrote to, which Claude Code may resume afterwards) is `stale` however late it started.
//!
//! Without both records the terminal is judged against its process start time: Claude Code
//! loads the resumed history before it announces itself, so any headless output from shortly
//! before that start onwards may be missing from what it loaded.

use serde::{Deserialize, Serialize};

pub use crate::protocol::{LeaseReason as Reason, LeaseState as State, LeaseView as View};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Headless output this close before a terminal's start counts as after it. Process start times
/// are tick-granular, and an error must fall on the stale side.
pub const START_MARGIN_MS: i64 = 1_000;
/// A terminal announces itself when it loads a conversation, at startup or on an in-process
/// switch. Output older than this before an announcement preceded that load.
pub const OUTPUT_MEMORY_MS: i64 = 10 * 60 * 1_000;
/// How long a headless turn may hold a reopened conversation before the daemon stops it anyway,
/// so an unanswered approval cannot block the terminal forever.
pub const RELEASE_DEADLINE_MS: i64 = 10 * 60 * 1_000;
const MAX_THREADS: usize = 256;
/// How many conversations the daemon remembers having run headless. A forgotten one goes back to
/// the start-time rule, so this is kept well above how many conversations one device runs.
const MAX_HEADLESS_THREADS: usize = 4096;
/// The largest transcript the branch rule reads. Past it, which branch a terminal continues is
/// not known, and a terminal with the plugin is held back as stale.
pub const MAX_TRANSCRIPT_BYTES: u64 = 128 * 1024 * 1024;
/// How many transcripts' record parents are kept between checks; a waiting terminal asks again
/// every few seconds, and an unchanged transcript is not read again.
const PARENT_CACHE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Terminal {
    pub pid: u32,
    pub started_ms: i64,
    /// The AgentGit plugin runs in this terminal: it can carry web input and hold local input.
    pub plugin: bool,
}

impl Terminal {
    fn same_process(&self, other: &Terminal) -> bool {
        self.pid == other.pid && self.started_ms == other.started_ms
    }
}

/// The branch a terminal continues, as its plugin saw it: the id of the newest record it
/// appended, and the transcript holding it. Ids only, kept on this device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Loaded {
    pub leaf: String,
    pub transcript: PathBuf,
}

/// What a transcript says about the branch a terminal continues, against the headless writer's
/// last record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    /// The terminal's newest record is on disk, and its branch holds the writer's last record.
    Holds,
    /// The terminal's newest record is on disk, on a branch without the writer's last record.
    Lacks,
    /// The terminal's newest record is not on disk yet: Claude Code writes a terminal's start
    /// records only with its next one, the person's first prompt. `resumed_holds` is whether
    /// the record `claude --resume` continues (the newest `last-prompt` record's `leafUuid`)
    /// holds the writer's last record.
    Unwritten { resumed_holds: bool },
    /// Not known: the transcript can't be read or is over MAX_TRANSCRIPT_BYTES, or the
    /// terminal's record isn't on disk and the record Claude Code resumes can't be found.
    Unknown,
}

/// What `loaded` says against the writer's last record `ancestor`.
pub type Descends<'a> = &'a dyn Fn(&Loaded, &str) -> Branch;

/// Walk a Claude Code transcript from `loaded`'s leaf towards the root, across compaction
/// boundaries, looking for `ancestor`. Only record ids are read, from a transcript of at most
/// MAX_TRANSCRIPT_BYTES.
pub fn descends(loaded: &Loaded, ancestor: &str) -> Branch {
    descends_within(loaded, ancestor, MAX_TRANSCRIPT_BYTES)
}

fn descends_within(loaded: &Loaded, ancestor: &str, limit: u64) -> Branch {
    let Some(parents) = parents(&loaded.transcript, limit) else {
        return Branch::Unknown;
    };
    if parents.contains_key(&loaded.leaf) {
        return if reaches(&parents, &loaded.leaf, ancestor) {
            Branch::Holds
        } else {
            Branch::Lacks
        };
    }
    match resumed_leaf(&loaded.transcript, limit) {
        Some(resumed) if parents.contains_key(&resumed) => Branch::Unwritten {
            resumed_holds: reaches(&parents, &resumed, ancestor),
        },
        _ => Branch::Unknown,
    }
}

fn reaches(parents: &HashMap<String, Option<String>>, from: &str, ancestor: &str) -> bool {
    let mut at = Some(from);
    // A cycle cannot be longer than the transcript.
    for _ in 0..=parents.len() {
        match at {
            Some(id) if id == ancestor => return true,
            Some(id) => at = parents.get(id).and_then(|parent| parent.as_deref()),
            None => return false,
        }
    }
    false
}

/// The record `claude --resume` continues: the `leafUuid` of the transcript's newest
/// `last-prompt` record (Claude Code 2.1.287 and later write them). The file is read backwards
/// from its end, and only that one field is parsed: the record's prompt text is never decoded or
/// kept, and the id is not stored. `None` when there is no such record, the newest names no
/// leaf, or it can't be parsed. Claude Code appends these records, so the last one is the newest.
fn resumed_leaf(path: &Path, limit: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    #[derive(Deserialize)]
    struct LastPrompt {
        #[serde(rename = "leafUuid")]
        leaf: Option<String>,
    }
    // Claude Code writes the record's type first; a record written otherwise is not found, and
    // the terminal is held back.
    const RECORD: &[u8] = br#"{"type":"last-prompt""#;
    const CHUNK: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let mut end = file.metadata().ok()?.len();
    if end > limit {
        return None;
    }
    // The bytes after `end` not yet scanned: the start of a line cut by the previous chunk.
    let mut rest = Vec::new();
    loop {
        let start = end.saturating_sub(CHUNK);
        let mut bytes = vec![0; (end - start) as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut bytes).ok()?;
        bytes.extend_from_slice(&rest);
        // Whole lines begin after the first newline, unless this chunk starts the file.
        let whole = if start == 0 {
            Some(0)
        } else {
            bytes
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|at| at + 1)
        };
        if let Some(whole) = whole {
            if let Some(line) = bytes[whole..]
                .rsplit(|byte| *byte == b'\n')
                .find(|line| line.starts_with(RECORD))
            {
                return serde_json::from_slice::<LastPrompt>(line).ok()?.leaf;
            }
            bytes.truncate(whole);
        }
        if start == 0 {
            return None;
        }
        rest = bytes;
        end = start;
    }
}

type Parents = std::sync::Arc<HashMap<String, Option<String>>>;
type ParentKey = (PathBuf, u64, Option<std::time::SystemTime>);

static PARENTS: std::sync::Mutex<Vec<(ParentKey, Parents)>> = std::sync::Mutex::new(Vec::new());

/// Each record's parent in the transcript at `path`, read again only when its size or
/// modification time changed (Claude Code only appends); `None` past `limit` bytes.
fn parents(path: &Path, limit: u64) -> Option<Parents> {
    use std::io::Read;
    #[derive(Deserialize)]
    struct Row {
        uuid: Option<String>,
        #[serde(rename = "parentUuid")]
        parent: Option<String>,
        #[serde(rename = "logicalParentUuid")]
        logical_parent: Option<String>,
    }
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if metadata.len() > limit {
        return None;
    }
    let key = (path.to_owned(), metadata.len(), metadata.modified().ok());
    let cache = || {
        PARENTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };
    if let Some((_, parents)) = cache().iter().find(|(cached, _)| *cached == key) {
        return Some(parents.clone());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    // It grew past the limit while it was read.
    if bytes.len() as u64 > limit {
        return None;
    }
    let mut parents = HashMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Ok(Row {
            uuid: Some(uuid),
            parent,
            logical_parent,
        }) = serde_json::from_slice::<Row>(line)
        {
            parents.insert(uuid, parent.or(logical_parent));
        }
    }
    let parents = std::sync::Arc::new(parents);
    let mut cache = cache();
    cache.retain(|(cached, _)| cached.0 != key.0);
    if cache.len() >= PARENT_CACHE {
        cache.remove(0);
    }
    cache.push((key, parents.clone()));
    Some(parents)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Holder {
    Tui(Terminal),
    /// Web input running inside this terminal's own process through the plugin.
    Web(Terminal),
    Headless {
        session_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub epoch: u64,
    pub holder: Holder,
    pub state: State,
    pub reason: Reason,
    pub since_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<Terminal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<i64>,
    /// The headless writer process, by pid and start time, when known: whose release failed (the
    /// hold lasts while that exact process runs, across daemon restarts), or that a handover
    /// waits on (it resolves without that writer's word once the process is gone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<Terminal>,
    /// The branch the terminal holding or awaiting the conversation continues, when its plugin
    /// has said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<Loaded>,
}

/// What the daemon must do after a terminal announced itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// Stop this headless session at its turn boundary, at once when it is idle.
    Release {
        session_id: String,
    },
    Granted(State),
    Unchanged(State),
}

/// The headless writer of a conversation when a terminal announces itself.
pub struct Headless<'a> {
    pub session_id: &'a str,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Leases {
    #[serde(default)]
    threads: HashMap<String, Lease>,
    /// Wall-clock time of the last output each conversation's headless writer produced.
    #[serde(default)]
    output: HashMap<String, i64>,
    /// The last record each conversation's headless writer appended, as its plugin reported it
    /// at the end of its turn. Absent while a turn runs, and when a writer ended unobserved.
    #[serde(default)]
    writer_leaf: HashMap<String, String>,
    /// When this daemon last saw a headless writer of each conversation at work. Such a
    /// conversation is judged by branch: while its writer's last record is unknown, a terminal
    /// with the plugin is held back.
    #[serde(default)]
    headless_ran: HashMap<String, i64>,
    #[serde(default)]
    epoch: u64,
}

/// The daemon's lease file, the same for the daemon and for a terminal's hooks, which run as plain
/// commands.
pub fn path() -> Option<PathBuf> {
    crate::rc::local_rc_dir()
        .ok()
        .map(|dir| dir.join("leases.json"))
}

impl Leases {
    /// The headless writer that a handover of `thread` waits on, or whose release failed, as the
    /// lease file at `path` records it now. Read only: nothing is expired or saved.
    #[cfg(unix)]
    pub fn handover_writer_in(path: &Path, thread: &str) -> Option<Terminal> {
        let leases: Self = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        let lease = leases.threads.get(thread)?;
        (matches!(lease.holder, Holder::Headless { .. })
            && matches!(lease.state, State::Releasing | State::ReleaseFailed))
        .then_some(lease.writer)
        .flatten()
    }

    pub fn get(&self, thread: &str) -> Option<&Lease> {
        self.threads.get(thread)
    }

    pub fn record_output(&mut self, thread: &str, now_ms: i64) {
        if self.output.len() >= MAX_THREADS && !self.output.contains_key(thread) {
            // A conversation with a lease keeps its output time: a terminal holding or awaiting
            // it is judged by that time. Only when every remembered conversation has a lease
            // does the memory grow instead.
            let oldest = self
                .output
                .iter()
                .filter(|(thread, _)| !self.threads.contains_key(*thread))
                .min_by_key(|(_, at)| **at)
                .map(|(thread, _)| thread.clone());
            if let Some(oldest) = oldest {
                self.output.remove(&oldest);
            }
        }
        self.output.insert(thread.to_owned(), now_ms);
        self.mark_headless(thread, now_ms);
    }

    fn mark_headless(&mut self, thread: &str, now_ms: i64) {
        if self.headless_ran.len() >= MAX_HEADLESS_THREADS
            && !self.headless_ran.contains_key(thread)
        {
            let oldest = self
                .headless_ran
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(thread, _)| thread.clone());
            if let Some(oldest) = oldest {
                self.headless_ran.remove(&oldest);
            }
        }
        self.headless_ran.insert(thread.to_owned(), now_ms);
    }

    /// The headless writer of `thread` may have written until now without saying what: its last
    /// record is no longer known either.
    pub fn record_unobserved_output(&mut self, thread: &str, now_ms: i64) {
        self.record_output(thread, now_ms);
        self.writer_leaf.remove(thread);
    }

    /// A headless turn started: until its writer reports its last record, that record is not
    /// known.
    pub fn writer_turn_started(&mut self, thread: &str) {
        self.writer_leaf.remove(thread);
    }

    /// The headless writer of `thread` finished a turn on record `leaf`.
    pub fn writer_reported(&mut self, thread: &str, leaf: &str, now_ms: i64) {
        self.mark_headless(thread, now_ms);
        if self.writer_leaf.len() >= MAX_THREADS && !self.writer_leaf.contains_key(thread) {
            // Forgetting one only holds its next terminal back.
            let unleased = self
                .writer_leaf
                .keys()
                .find(|thread| !self.threads.contains_key(*thread))
                .or_else(|| self.writer_leaf.keys().next())
                .cloned();
            if let Some(unleased) = unleased {
                self.writer_leaf.remove(&unleased);
            }
        }
        self.writer_leaf.insert(thread.to_owned(), leaf.to_owned());
    }

    /// Whether the headless writer of `thread` produced output from shortly before `terminal`
    /// started onwards, however long ago. Asked only of a conversation this daemon ran
    /// headless: a missing output time there was forgotten, not absent, and counts as output.
    fn output_since_start(&self, thread: &str, terminal: &Terminal) -> bool {
        self.output
            .get(thread)
            .is_none_or(|at| *at >= terminal.started_ms - START_MARGIN_MS)
    }

    fn stale(&self, thread: &str, terminal: &Terminal, now_ms: i64) -> bool {
        self.output.get(thread).is_some_and(|at| {
            *at >= terminal.started_ms - START_MARGIN_MS && now_ms - *at <= OUTPUT_MEMORY_MS
        })
    }

    /// The branch rule, for a terminal with the plugin in a conversation this daemon ran
    /// headless: it is current only when the branch it continues holds the headless writer's
    /// last record. Fail-closed: while that record is unknown (a turn runs, its report was lost,
    /// the writer ended unobserved), until the terminal's plugin has named its branch, and when
    /// that branch cannot be read, the terminal is held back as stale. `None` when the rule does
    /// not apply: a terminal without the plugin, or a conversation never run headless here.
    fn by_branch(
        &self,
        thread: &str,
        terminal: &Terminal,
        loaded: Option<&Loaded>,
        descends: Descends<'_>,
    ) -> Option<State> {
        if !terminal.plugin
            || !(self.headless_ran.contains_key(thread) || self.writer_leaf.contains_key(thread))
        {
            return None;
        }
        let writer_leaf = self.writer_leaf.get(thread);
        Some(
            match writer_leaf
                .zip(loaded)
                .map(|(writer_leaf, loaded)| descends(loaded, writer_leaf))
            {
                Some(Branch::Holds) => State::Held,
                // Until the terminal's own record is on disk: what Claude Code resumed must hold
                // the writer's last record, and no headless output may postdate the terminal's
                // start (the writer may have written between Claude Code's load and its read).
                // That process keeps the history it loaded however long it waits, so here the
                // output does not age out of memory.
                Some(Branch::Unwritten {
                    resumed_holds: true,
                }) if !self.output_since_start(thread, terminal) => State::Held,
                _ => State::Stale,
            },
        )
    }

    fn judge(
        &self,
        thread: &str,
        terminal: &Terminal,
        loaded: Option<&Loaded>,
        descends: Descends<'_>,
        now_ms: i64,
    ) -> State {
        self.by_branch(thread, terminal, loaded, descends)
            .unwrap_or(if self.stale(thread, terminal, now_ms) {
                State::Stale
            } else {
                State::Held
            })
    }

    fn set(&mut self, thread: &str, holder: Holder, state: State, reason: Reason, now_ms: i64) {
        self.epoch += 1;
        self.threads.insert(
            thread.to_owned(),
            Lease {
                epoch: self.epoch,
                holder,
                state,
                reason,
                since_ms: now_ms,
                next: None,
                deadline_ms: None,
                writer: None,
                loaded: None,
            },
        );
    }

    /// The conversation whose lease `session_id` holds as a headless writer.
    pub fn thread_held_by(&self, session_id: &str) -> Option<String> {
        self.threads
            .iter()
            .find(|(_, lease)| {
                matches!(&lease.holder, Holder::Headless { session_id: holder } if holder == session_id)
            })
            .map(|(thread, _)| thread.clone())
    }

    fn grant(
        &mut self,
        thread: &str,
        terminal: Terminal,
        loaded: Option<Loaded>,
        reason: Reason,
        descends: Descends<'_>,
        now_ms: i64,
    ) -> State {
        let state = self.judge(thread, &terminal, loaded.as_ref(), descends, now_ms);
        self.set(thread, Holder::Tui(terminal), state, reason, now_ms);
        self.threads.get_mut(thread).expect("set above").loaded = loaded;
        state
    }

    /// A terminal loaded `thread`. A headless writer hands over at its turn boundary; otherwise
    /// the terminal holds the conversation, stale when it may lack the writer's last turn (see
    /// the module). A terminal already holding or awaiting the lease keeps its state:
    /// announcements repeat when the plugin re-registers, long after the load a start time
    /// judges. Only the branch it names is judged again, as its plugin sees new records.
    pub fn opened(
        &mut self,
        thread: &str,
        terminal: Terminal,
        loaded: Option<Loaded>,
        headless: Option<Headless<'_>>,
        descends: Descends<'_>,
        now_ms: i64,
    ) -> Opened {
        if let Some(lease) = self.threads.get_mut(thread) {
            let known = match (&mut lease.holder, &mut lease.next) {
                (Holder::Tui(current) | Holder::Web(current), _)
                    if current.same_process(&terminal) =>
                {
                    current.plugin |= terminal.plugin;
                    true
                }
                (Holder::Headless { .. }, Some(next)) if next.same_process(&terminal) => {
                    next.plugin |= terminal.plugin;
                    true
                }
                _ => false,
            };
            if known {
                if loaded.is_some() {
                    lease.loaded = loaded;
                }
                let state = lease.state;
                let holding = match lease.holder {
                    Holder::Tui(current) | Holder::Web(current)
                        if matches!(state, State::Held | State::Stale) =>
                    {
                        Some((current, lease.holder.clone(), lease.reason))
                    }
                    _ => None,
                };
                let loaded = lease.loaded.clone();
                if let Some((current, holder, reason)) = holding
                    && let Some(judged) =
                        self.by_branch(thread, &current, loaded.as_ref(), descends)
                    && judged != state
                {
                    self.set(thread, holder, judged, reason, now_ms);
                    self.threads.get_mut(thread).expect("set above").loaded = loaded;
                    return Opened::Granted(judged);
                }
                return Opened::Unchanged(state);
            }
            // The writer whose release failed may still be running, whoever opens next. A
            // terminal opening meanwhile becomes the one that takes over when the hold lifts,
            // if nobody is waiting yet.
            if lease.state == State::ReleaseFailed {
                if lease.next.is_none() {
                    lease.next = Some(terminal);
                    lease.loaded = loaded;
                }
                return Opened::Unchanged(State::ReleaseFailed);
            }
            // The writer already hands over to the terminal that opened first, by the deadline
            // set then. Another terminal asks again at every retry of its refused registration;
            // taking the handover in turns, the two would move the deadline on each time, and a
            // turn stuck on an approval would never reach it.
            if lease.state == State::Releasing
                && lease.next.is_some()
                && headless.as_ref().is_some_and(|headless| {
                    matches!(&lease.holder, Holder::Headless { session_id } if session_id == headless.session_id)
                })
            {
                return Opened::Unchanged(State::Releasing);
            }
        }
        match headless {
            Some(headless) => {
                self.set(
                    thread,
                    Holder::Headless {
                        session_id: headless.session_id.to_owned(),
                    },
                    State::Releasing,
                    Reason::TerminalOpened,
                    now_ms,
                );
                let lease = self.threads.get_mut(thread).expect("set above");
                lease.next = Some(terminal);
                lease.loaded = loaded;
                lease.deadline_ms = Some(now_ms + RELEASE_DEADLINE_MS);
                Opened::Release {
                    session_id: headless.session_id.to_owned(),
                }
            }
            None => Opened::Granted(self.grant(
                thread,
                terminal,
                loaded,
                Reason::TerminalOpened,
                descends,
                now_ms,
            )),
        }
    }

    /// The headless writer of `thread` ended: a waiting terminal takes the conversation, judged
    /// against everything that writer produced.
    pub fn headless_ended(
        &mut self,
        thread: &str,
        session_id: &str,
        descends: Descends<'_>,
        now_ms: i64,
    ) -> bool {
        let Some(lease) = self.threads.get(thread) else {
            return false;
        };
        if !matches!(&lease.holder, Holder::Headless { session_id: holder } if holder == session_id)
        {
            return false;
        }
        match lease.next {
            Some(next) => {
                let loaded = lease.loaded.clone();
                self.grant(
                    thread,
                    next,
                    loaded,
                    Reason::HeadlessEnded,
                    descends,
                    now_ms,
                );
            }
            None => {
                self.threads.remove(thread);
            }
        }
        true
    }

    /// The headless writer of `thread` ended without its process being confirmed gone. The
    /// conversation stays with it, and the waiting terminal is told why.
    pub fn release_failed(
        &mut self,
        thread: &str,
        session_id: &str,
        writer: Option<Terminal>,
        now_ms: i64,
    ) -> bool {
        let lease = self.threads.get(thread);
        let mine = |lease: &Lease| matches!(&lease.holder, Holder::Headless { session_id: holder } if holder == session_id);
        if lease.is_some_and(|lease| mine(lease) && lease.state == State::ReleaseFailed) {
            return false;
        }
        // Whether or not anyone waits now, everyone who opens the conversation later must find
        // the failure; a terminal that was waiting or holding it takes over once it is lifted.
        let next = lease.and_then(|lease| match &lease.holder {
            Holder::Headless { .. } => lease.next,
            Holder::Tui(terminal) | Holder::Web(terminal) => Some(*terminal),
        });
        let loaded = lease.and_then(|lease| lease.loaded.clone());
        // It may have written past its last report.
        self.writer_leaf.remove(thread);
        self.set(
            thread,
            Holder::Headless {
                session_id: session_id.to_owned(),
            },
            State::ReleaseFailed,
            Reason::HeadlessEnded,
            now_ms,
        );
        let lease = self.threads.get_mut(thread).expect("set above");
        lease.next = next;
        lease.writer = writer;
        lease.loaded = loaded;
        true
    }

    /// Lift every failed release whose recorded writer process is gone: a waiting terminal takes
    /// the conversation as stale, since that writer ended unobserved; with nobody waiting, the
    /// lease ends. A failed release whose writer is unknown waits for a daemon restart.
    pub fn lift_gone_writers(
        &mut self,
        alive: impl Fn(&Terminal) -> bool,
        now_ms: i64,
    ) -> Vec<String> {
        let gone: Vec<_> = self
            .threads
            .iter()
            .filter(|(_, lease)| lease.state == State::ReleaseFailed)
            .filter(|(_, lease)| lease.writer.is_some_and(|writer| !alive(&writer)))
            .map(|(thread, lease)| (thread.clone(), lease.next, lease.loaded.clone()))
            .collect();
        for (thread, next, loaded) in &gone {
            // What the writer wrote after its release failed was never observed: it may have
            // written until now, so every terminal that loaded the conversation before is behind.
            self.record_unobserved_output(thread, now_ms);
            match next.filter(|next| alive(next)) {
                Some(next) => {
                    self.set(
                        thread,
                        Holder::Tui(next),
                        State::Stale,
                        Reason::HeadlessEnded,
                        now_ms,
                    );
                    self.threads.get_mut(thread).expect("set above").loaded = loaded.clone();
                }
                None => {
                    self.threads.remove(thread);
                }
            }
        }
        gone.into_iter().map(|(thread, ..)| thread).collect()
    }

    /// The handover of `thread` waits on the writer process `writer`.
    pub fn release_writer(&mut self, thread: &str, writer: Terminal) {
        if let Some(lease) = self
            .threads
            .get_mut(thread)
            .filter(|lease| lease.state == State::Releasing)
        {
            lease.writer = Some(writer);
        }
    }

    /// Resolve every overdue handover whose writer's row is gone (`row` is false for it) and
    /// whose recorded writer process has exited, as a writer that ended unobserved: a waiting
    /// terminal takes the conversation as stale, or the lease ends. A writer whose process runs,
    /// or was never recorded, keeps the conversation waiting.
    pub fn lift_unobserved_releases(
        &mut self,
        row: impl Fn(&str) -> bool,
        alive: impl Fn(&Terminal) -> bool,
        now_ms: i64,
    ) -> Vec<String> {
        let gone: Vec<_> = self
            .threads
            .iter()
            .filter(|(_, lease)| lease.state == State::Releasing)
            .filter(|(_, lease)| lease.deadline_ms.is_some_and(|deadline| now_ms >= deadline))
            .filter(|(_, lease)| {
                matches!(&lease.holder, Holder::Headless { session_id } if !row(session_id))
            })
            .filter(|(_, lease)| lease.writer.is_some_and(|writer| !alive(&writer)))
            .map(|(thread, lease)| (thread.clone(), lease.next, lease.loaded.clone()))
            .collect();
        for (thread, next, loaded) in &gone {
            self.record_unobserved_output(thread, now_ms);
            match next.filter(|next| alive(next)) {
                Some(next) => {
                    self.set(
                        thread,
                        Holder::Tui(next),
                        State::Stale,
                        Reason::HeadlessEndedUnobserved,
                        now_ms,
                    );
                    self.threads.get_mut(thread).expect("set above").loaded = loaded.clone();
                }
                None => {
                    self.threads.remove(thread);
                }
            }
        }
        gone.into_iter().map(|(thread, ..)| thread).collect()
    }

    /// The lease of `thread`, for tests that move its clock.
    #[cfg(test)]
    pub(crate) fn lease_mut(&mut self, thread: &str) -> Option<&mut Lease> {
        self.threads.get_mut(thread)
    }

    /// Headless sessions whose handover deadline has passed.
    pub fn overdue(&self, now_ms: i64) -> Vec<String> {
        self.threads
            .values()
            .filter(|lease| lease.state == State::Releasing)
            .filter(|lease| lease.deadline_ms.is_some_and(|deadline| now_ms >= deadline))
            .filter_map(|lease| match &lease.holder {
                Holder::Headless { session_id } => Some(session_id.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn releasing(&self, thread: &str) -> bool {
        self.threads
            .get(thread)
            .is_some_and(|lease| lease.state == State::Releasing)
    }

    /// A turn started in `thread`'s terminal; `remote` when web input started it there.
    pub fn turn_started(&mut self, thread: &str, remote: bool, now_ms: i64) -> bool {
        let Some(lease) = self.threads.get(thread) else {
            return false;
        };
        if lease.state != State::Held {
            return false;
        }
        let (holder, reason) = match (&lease.holder, remote) {
            (Holder::Tui(terminal), true) if terminal.plugin => {
                (Holder::Web(*terminal), Reason::WebInput)
            }
            (Holder::Web(terminal), false) => (Holder::Tui(*terminal), Reason::LocalInput),
            _ => return false,
        };
        let loaded = lease.loaded.clone();
        self.set(thread, holder, State::Held, reason, now_ms);
        self.threads.get_mut(thread).expect("set above").loaded = loaded;
        true
    }

    /// Drop every lease whose holder is gone. A headless holder never outlives the daemon that
    /// supervised it, so on load `headless_alive` is false and a terminal waiting on it takes
    /// over; a releasing lease with nobody left to take over resolves to no holder. A terminal
    /// that takes over from a writer that ended unobserved is stale: that writer's last output
    /// may have reached the transcript without being recorded here.
    pub fn expire(
        &mut self,
        terminal_alive: impl Fn(&Terminal) -> bool,
        headless_alive: impl Fn(&str) -> bool,
        now_ms: i64,
    ) -> Vec<String> {
        let mut changed = Vec::new();
        let threads: Vec<_> = self.threads.keys().cloned().collect();
        for thread in threads {
            let lease = self.threads[&thread].clone();
            match &lease.holder {
                // A failed release outlives the daemon while its writer process runs.
                Holder::Headless { .. }
                    if lease.state == State::ReleaseFailed
                        && lease.writer.is_some_and(|writer| terminal_alive(&writer)) => {}
                Holder::Tui(terminal) | Holder::Web(terminal) if !terminal_alive(terminal) => {
                    self.threads.remove(&thread);
                    changed.push(thread);
                }
                Holder::Headless { session_id } if !headless_alive(session_id) => {
                    // The writer ended unobserved, possibly writing until now.
                    self.record_unobserved_output(&thread, now_ms);
                    match lease.next.filter(|next| terminal_alive(next)) {
                        Some(next) => {
                            self.set(
                                &thread,
                                Holder::Tui(next),
                                State::Stale,
                                Reason::HeadlessEnded,
                                now_ms,
                            );
                            self.threads.get_mut(&thread).expect("set above").loaded =
                                lease.loaded.clone();
                        }
                        None => {
                            self.threads.remove(&thread);
                        }
                    }
                    changed.push(thread);
                }
                Holder::Headless { .. }
                    if lease.next.is_some_and(|next| !terminal_alive(&next)) =>
                {
                    let lease = self.threads.get_mut(&thread).expect("present");
                    lease.next = None;
                    lease.loaded = None;
                    lease.state = State::Held;
                    lease.deadline_ms = None;
                    changed.push(thread);
                }
                _ => {}
            }
        }
        changed
    }

    pub fn load(path: &Path, terminal_alive: impl Fn(&Terminal) -> bool, now_ms: i64) -> Self {
        let mut leases: Self = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        leases.expire(terminal_alive, |_| false, now_ms);
        // A web holder belongs to the controller links of the daemon that granted it.
        for lease in leases.threads.values_mut() {
            if let Holder::Web(terminal) = lease.holder {
                lease.holder = Holder::Tui(terminal);
            }
        }
        leases
    }

    pub fn save(&self, path: &Path) -> crate::Result<()> {
        let directory = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("lease file has no directory"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temporary, self)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path)?;
        Ok(())
    }
}

impl Lease {
    pub fn view(&self, device: Option<&str>) -> View {
        let (holder, takeable) = match &self.holder {
            Holder::Tui(terminal) => ("tui", terminal.plugin && self.state == State::Held),
            Holder::Web(_) => ("web", self.state == State::Held),
            Holder::Headless { .. } => ("headless", self.state == State::Held),
        };
        View {
            epoch: self.epoch,
            holder: holder.into(),
            state: self.state,
            reason: self.reason,
            takeable,
            device: device.map(str::to_owned),
            since: chrono::DateTime::from_timestamp_millis(self.since_ms)
                .unwrap_or_default()
                .to_rfc3339(),
        }
    }

    /// The line `opener` shows while its own input must wait, if it must. A failed release holds
    /// every terminal that opens the conversation; the other holds concern one terminal. Only
    /// the plugin can hold what is typed; a terminal without it is asked not to type.
    pub fn notice_for(&self, thread: &str, opener: &Terminal) -> Option<String> {
        let (terminal, situation) = match (&self.holder, self.state, self.next) {
            (Holder::Headless { .. }, State::Releasing, Some(next))
                if next.same_process(opener) =>
            {
                (
                    next,
                    "A turn started from the web is still running for this conversation.",
                )
            }
            // Another terminal waits to take over first; this one writes only once that one is
            // closed, as between any two terminals.
            (Holder::Headless { .. }, State::Releasing, Some(_)) => {
                return Some(self.behind_first_terminal(thread, opener));
            }
            (Holder::Headless { .. }, State::ReleaseFailed, _) => (
                *opener,
                "agit could not confirm that the web's process for this conversation has stopped, \
                 so this terminal cannot take over yet.",
            ),
            (Holder::Tui(terminal), State::Stale, _) if terminal.same_process(opener) => (
                *terminal,
                "This terminal continues an older branch of this conversation without that turn, \
                 and a message typed here would keep it there.",
            ),
            _ => return None,
        };
        // A message typed here, even one the plugin holds back, makes this terminal's branch the
        // one Claude Code resumes next, and then resuming no longer brings back the web's turn:
        // every remedy that needs a resume asks for it before typing.
        let remedy = match (self.state, terminal.plugin) {
            (State::ReleaseFailed, _) => match self.writer {
                Some(writer) => format!(
                    "It is process {pid} (check with `ps -p {pid}`); once it has stopped, exit \
                     and run `claude --resume {thread}` before typing here.",
                    pid = writer.pid
                ),
                None => format!(
                    "Make sure no `claude -p` for it is still running, then restart agit's \
                     background service (`agit rc stop`, then `agit rc start`), exit and run \
                     `claude --resume {thread}` before typing here."
                ),
            },
            // The deadline is when the daemon stops that turn itself, so the wait has an end the
            // person can read; without it the line reads as waiting for as long as the web runs.
            (State::Releasing, true) => format!(
                "This terminal takes over when it ends{latest}; your message stays here until then.",
                latest = self.latest_takeover()
            ),
            (State::Releasing, false) => format!(
                "When it ends{latest}, exit and run `claude --resume {thread}` before typing here.",
                latest = self.latest_takeover()
            ),
            _ => format!(
                "Exit and run `claude --resume {thread}` now, before typing here, to continue with \
                 the turn sent from the web."
            ),
        };
        // An older branch is what a terminal opened during a web turn has once that turn ends:
        // the step that takes over comes first, so a status line cut to the terminal's width
        // still shows the command to copy.
        if self.state == State::Stale {
            return Some(format!("{remedy} {situation}"));
        }
        Some(format!("{situation} {remedy}"))
    }

    /// The line of a terminal that opened a releasing conversation after another terminal did:
    /// that one takes over from agit's own writer, and this one writes only once it is closed, as
    /// between any two terminals. True whether that writer is in a turn or already stopping.
    fn behind_first_terminal(&self, thread: &str, opener: &Terminal) -> String {
        let by = self
            .latest_takeover_minute()
            .map(|minute| format!(", by {minute} at the latest"))
            .unwrap_or_default();
        let situation = format!(
            "Another terminal opened this conversation before this one and takes it over from \
             agit{by}."
        );
        if opener.plugin {
            format!(
                "{situation} This terminal can write once that one is closed and agit's own \
                 process for it has stopped; your input waits here."
            )
        } else {
            format!(
                "{situation} Once that one is closed and agit's own process for it has \
                 stopped, exit and run `claude --resume {thread}` before typing here."
            )
        }
    }

    /// When a releasing writer's turn is stopped if it has not ended by then, in the terminal's
    /// local time, as a clause of the waiting line; empty when no deadline is set.
    fn latest_takeover(&self) -> String {
        self.latest_takeover_minute()
            .map(|minute| format!(", at {minute} at the latest, when agit stops that turn"))
            .unwrap_or_default()
    }

    /// The minute, in the terminal's local time, by which a releasing writer has been stopped:
    /// the first one wholly after the deadline. The stop comes at the daemon's next tick after
    /// the deadline, so a minute rounded down would name a time the terminal may still be
    /// waiting at.
    fn latest_takeover_minute(&self) -> Option<String> {
        const MINUTE_MS: i64 = 60_000;
        self.deadline_ms
            .map(|deadline| deadline - deadline.rem_euclid(MINUTE_MS) + MINUTE_MS)
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|minute| {
                minute
                    .with_timezone(&chrono::Local)
                    .format("%H:%M")
                    .to_string()
            })
    }
}

/// A device's short code: the last four characters of its Hub device id.
pub fn device_code(device_id: &str) -> Option<String> {
    let code: String = device_id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<Vec<_>>()
        .iter()
        .rev()
        .take(4)
        .rev()
        .collect::<String>()
        .to_ascii_lowercase();
    (code.len() == 4).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: i64 = 1_000_000;
    fn tui(plugin: bool) -> Terminal {
        Terminal {
            pid: 7,
            started_ms: START,
            plugin,
        }
    }
    fn unread(_: &Loaded, _: &str) -> Branch {
        Branch::Unknown
    }

    /// Without a branch named, as a terminal without the plugin opens.
    impl Leases {
        fn open(
            &mut self,
            thread: &str,
            terminal: Terminal,
            headless: Option<Headless<'_>>,
            now_ms: i64,
        ) -> Opened {
            self.opened(thread, terminal, None, headless, &unread, now_ms)
        }

        fn ended(&mut self, thread: &str, session_id: &str, now_ms: i64) -> bool {
            self.headless_ended(thread, session_id, &unread, now_ms)
        }
    }

    fn headless() -> Option<Headless<'static>> {
        Some(Headless {
            session_id: "agit-h",
        })
    }

    /// The three handovers of a headless conversation to a reopened terminal without the plugin
    /// (with it, the branch rule judges). The terminal is judged by everything the headless
    /// writer produced up to the handover, against when the terminal process started, because Claude Code loads the history before it announces
    /// itself: a turn ending between that load and the announcement must not pass.
    #[test]
    fn a_reopened_terminal_is_stale_when_headless_output_may_postdate_its_start() {
        // Idle: the last headless output long before the terminal started.
        let mut leases = Leases::default();
        leases.record_output("t", START - 60_000);
        assert_eq!(
            leases.open("t", tui(false), headless(), START + 2_000),
            Opened::Release {
                session_id: "agit-h".into()
            }
        );
        assert!(leases.releasing("t"));
        assert!(leases.ended("t", "agit-h", START + 2_100));
        assert_eq!(leases.get("t").unwrap().state, State::Held);

        // Mid-turn at the announcement: its end is output after the start.
        let mut leases = Leases::default();
        leases.open("t", tui(false), headless(), START + 2_000);
        leases.record_output("t", START + 9_000);
        leases.ended("t", "agit-h", START + 9_100);
        assert_eq!(leases.get("t").unwrap().state, State::Stale);

        // The race: the headless turn ends after the terminal started, before it announced.
        let mut leases = Leases::default();
        leases.record_output("t", START + 500);
        leases.open("t", tui(false), headless(), START + 2_000);
        leases.ended("t", "agit-h", START + 2_100);
        assert_eq!(leases.get("t").unwrap().state, State::Stale);

        // Start times are coarse: output just before the start counts as after it.
        let mut leases = Leases::default();
        leases.record_output("t", START - 500);
        assert_eq!(
            leases.open("t", tui(false), None, START + 2_000),
            Opened::Granted(State::Stale)
        );
    }

    /// A terminal that announces itself again (the plugin re-registers after a daemon restart)
    /// keeps the state it was judged with; only a new process is judged again.
    #[test]
    fn announcing_again_keeps_the_terminal_state() {
        let mut leases = Leases::default();
        leases.record_output("t", START + 500);
        leases.open("t", tui(false), None, START + 2_000);
        let much_later = START + 2 * OUTPUT_MEMORY_MS;
        assert_eq!(
            leases.open("t", tui(true), None, much_later),
            Opened::Unchanged(State::Stale)
        );
        let resumed = Terminal {
            pid: 8,
            started_ms: much_later,
            plugin: false,
        };
        assert_eq!(
            leases.open("t", resumed, None, much_later + 1_000),
            Opened::Granted(State::Held)
        );
    }

    /// Web input runs inside an idle terminal with the plugin, and typing there takes it back;
    /// a terminal without the plugin cannot carry web input.
    #[test]
    fn web_and_terminal_input_trade_the_lease_of_an_idle_plugin_terminal() {
        let mut leases = Leases::default();
        leases.open("t", tui(true), None, START + 1);
        assert!(leases.turn_started("t", true, START + 2));
        assert!(matches!(leases.get("t").unwrap().holder, Holder::Web(_)));
        assert!(leases.turn_started("t", false, START + 3));
        let lease = leases.get("t").unwrap();
        assert!(matches!(lease.holder, Holder::Tui(_)));
        assert_eq!(lease.reason, Reason::LocalInput);

        let mut plain = Leases::default();
        plain.open("t", tui(false), None, START + 1);
        assert!(!plain.turn_started("t", true, START + 2));
        assert!(!plain.get("t").unwrap().view(None).takeable);
    }

    /// After a daemon restart a lease whose holder is gone expires, and a handover in progress
    /// resolves to its terminal or to nobody, never to a permanent block.
    #[test]
    fn leases_with_a_gone_holder_expire_on_load() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("leases.json");
        let mut leases = Leases::default();
        leases.open("waiting", tui(true), headless(), START + 1);
        leases.open(
            "abandoned",
            Terminal {
                pid: 9,
                ..tui(true)
            },
            Some(Headless {
                session_id: "agit-g",
            }),
            START + 1,
        );
        leases.open(
            "closed",
            Terminal {
                pid: 10,
                ..tui(false)
            },
            None,
            START + 1,
        );
        leases.save(&path).unwrap();

        let loaded = Leases::load(&path, |terminal| terminal.pid == 7, START + 5);
        let waiting = loaded.get("waiting").unwrap();
        assert!(matches!(
            waiting.holder,
            Holder::Tui(Terminal { pid: 7, .. })
        ));
        assert!(loaded.get("abandoned").is_none());
        assert!(loaded.get("closed").is_none());
    }

    /// A daemon restart in the middle of a handover cannot prove that the headless writer's last
    /// turn was recorded before the daemon went down, so the waiting terminal resumes again
    /// rather than continuing from a history that may lack that turn.
    #[test]
    fn a_restart_mid_handover_leaves_the_waiting_terminal_stale() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("leases.json");
        let mut leases = Leases::default();
        leases.record_output("t", START - 60_000);
        leases.open("t", tui(true), headless(), START + 2_000);
        leases.save(&path).unwrap();
        // The headless turn ends after the save, and the daemon stops before saving again.
        leases.record_output("t", START + 9_000);

        let loaded = Leases::load(&path, |terminal| terminal.pid == 7, START + 20_000);
        let lease = loaded.get("t").unwrap();
        assert!(matches!(lease.holder, Holder::Tui(Terminal { pid: 7, .. })));
        assert_eq!(lease.state, State::Stale);
    }

    /// A failed release binds every terminal that opens the conversation afterwards, not only
    /// the one that was waiting, until a daemon restart finds the writer gone.
    #[test]
    fn a_failed_release_holds_a_different_terminal_process_too() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("leases.json");
        let mut leases = Leases::default();
        leases.open("t", tui(true), headless(), START + 1);
        assert!(leases.release_failed("t", "agit-h", None, START + 2));
        let other = Terminal {
            pid: 8,
            started_ms: START + 10,
            plugin: false,
        };
        assert_eq!(
            leases.open("t", other, None, START + 20),
            Opened::Unchanged(State::ReleaseFailed)
        );
        let lease = leases.get("t").unwrap();
        assert!(matches!(lease.holder, Holder::Headless { .. }));
        let notice = lease.notice_for("t", &other).unwrap();
        assert!(notice.contains("agit rc stop") && notice.contains("before typing here"));
        assert!(lease.notice_for("t", &tui(true)).is_some());
        leases.save(&path).unwrap();

        // The manual way out: after a restart only the terminal still running is left.
        let restarted = Leases::load(&path, |terminal| terminal.pid == 8, START + 30);
        assert!(restarted.get("t").is_none());
    }

    /// A daemon restart is not what makes a failed release safe: while the recorded writer
    /// process runs, the hold survives the restart; once it is gone, the hold lifts by itself.
    #[test]
    fn a_failed_release_lasts_while_its_writer_process_runs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("leases.json");
        let writer = Terminal {
            pid: 9,
            started_ms: START - 5_000,
            plugin: false,
        };
        let mut leases = Leases::default();
        leases.open("t", tui(true), headless(), START + 1);
        assert!(leases.release_failed("t", "agit-h", Some(writer), START + 2));
        let notice = leases
            .get("t")
            .unwrap()
            .notice_for("t", &tui(true))
            .unwrap();
        assert!(
            notice.contains("process 9") && notice.contains("`ps -p 9`"),
            "{notice}"
        );
        leases.save(&path).unwrap();

        let mut restarted =
            Leases::load(&path, |terminal| [7, 9].contains(&terminal.pid), START + 3);
        let lease = restarted.get("t").unwrap();
        assert_eq!(lease.state, State::ReleaseFailed, "the writer still runs");
        assert_eq!(lease.writer, Some(writer));

        assert!(
            restarted
                .lift_gone_writers(|terminal| [7, 9].contains(&terminal.pid), START + 4)
                .is_empty()
        );
        assert_eq!(
            restarted.lift_gone_writers(|terminal| terminal.pid == 7, START + 5),
            ["t"]
        );
        let lease = restarted.get("t").unwrap();
        assert!(matches!(lease.holder, Holder::Tui(Terminal { pid: 7, .. })));
        assert_eq!(lease.state, State::Stale, "the writer ended unobserved");
    }

    /// A failed cleanup is recorded even when no terminal was waiting for the conversation, so
    /// a terminal that opens it later finds the failure instead of a free conversation.
    #[test]
    fn a_failed_release_with_nobody_waiting_still_binds_later_openers() {
        let mut leases = Leases::default();
        assert!(leases.release_failed("t", "agit-h", None, START + 1));
        assert!(!leases.release_failed("t", "agit-h", None, START + 2));
        let later = Terminal {
            pid: 8,
            started_ms: START + 10,
            plugin: true,
        };
        assert_eq!(
            leases.open("t", later, None, START + 20),
            Opened::Unchanged(State::ReleaseFailed)
        );
        assert!(leases.get("t").unwrap().notice_for("t", &later).is_some());
    }

    /// A terminal that opens while a failed release stands, with nobody waiting, takes over when
    /// the writer is gone, but only after resuming again: the writer may have written past what
    /// the terminal loaded. Another terminal that loaded the conversation earlier is behind too.
    #[test]
    fn a_terminal_opening_during_a_failed_release_takes_over_stale() {
        let writer = Terminal {
            pid: 9,
            started_ms: START - 5_000,
            plugin: false,
        };
        let mut leases = Leases::default();
        leases.record_output("t", START - 60_000);
        assert!(leases.release_failed("t", "agit-h", Some(writer), START + 1));
        assert_eq!(
            leases.open("t", tui(true), None, START + 2_000),
            Opened::Unchanged(State::ReleaseFailed)
        );
        assert_eq!(leases.get("t").unwrap().next, Some(tui(true)));

        assert_eq!(
            leases.lift_gone_writers(|terminal| terminal.pid == 7, START + 30_000),
            ["t"]
        );
        let lease = leases.get("t").unwrap();
        assert!(matches!(lease.holder, Holder::Tui(Terminal { pid: 7, .. })));
        assert_eq!(lease.state, State::Stale, "never held: reload first");

        let earlier = Terminal {
            pid: 8,
            started_ms: START + 10_000,
            plugin: true,
        };
        assert_eq!(
            leases.open("t", earlier, None, START + 31_000),
            Opened::Granted(State::Stale)
        );
    }

    /// The shape of the conversation Sim run 5c forked: a web turn (`u1`, `a1`); a terminal
    /// resumed while the second web turn ran wrote its startup records (`h1`, `h2`) under the
    /// first reply, beside that turn (`u2`, `a2`); its held-back prompt left a notice (`d1`) that
    /// Claude Code then resumed, continuing with `r1`. `g1` is a resume of the writer's branch,
    /// and `c0` a compaction boundary on it.
    fn forked_transcript(directory: &Path) -> PathBuf {
        let path = directory.join("conversation.jsonl");
        let rows = [
            r#"{"type":"queue-operation"}"#,
            r#"{"type":"user","uuid":"u1","parentUuid":null}"#,
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1"}"#,
            r#"{"type":"attachment","uuid":"h1","parentUuid":"a1"}"#,
            r#"{"type":"user","uuid":"u2","parentUuid":"a1"}"#,
            r#"{"type":"assistant","uuid":"a2","parentUuid":"u2"}"#,
            r#"{"type":"last-prompt","leafUuid":"a2"}"#,
            r#"{"type":"attachment","uuid":"h2","parentUuid":"h1"}"#,
            r#"{"type":"system","uuid":"d1","parentUuid":"h2"}"#,
            r#"{"type":"last-prompt","leafUuid":"d1"}"#,
            r#"{"type":"assistant","uuid":"r1","parentUuid":"h2"}"#,
            r#"{"type":"attachment","uuid":"g1","parentUuid":"a2"}"#,
            r#"{"type":"system","uuid":"c0","parentUuid":null,"logicalParentUuid":"g1"}"#,
            r#"{"type":"user","uuid":"c1","parentUuid":"c0"}"#,
        ];
        std::fs::write(&path, rows.join("\n")).unwrap();
        path
    }

    fn at(transcript: &Path, leaf: &str) -> Loaded {
        Loaded {
            leaf: leaf.into(),
            transcript: transcript.to_owned(),
        }
    }

    #[test]
    fn a_branch_descends_through_parents_and_compactions() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = forked_transcript(directory.path());
        assert_eq!(descends(&at(&transcript, "a2"), "a2"), Branch::Holds);
        assert_eq!(descends(&at(&transcript, "g1"), "a2"), Branch::Holds);
        assert_eq!(descends(&at(&transcript, "c1"), "a2"), Branch::Holds);
        assert_eq!(descends(&at(&transcript, "r1"), "a2"), Branch::Lacks);
        assert_eq!(descends(&at(&transcript, "d1"), "a2"), Branch::Lacks);
        assert_eq!(descends(&at(&transcript, "a1"), "a2"), Branch::Lacks);
        assert_eq!(
            descends(&at(&transcript, "unwritten"), "a2"),
            Branch::Unwritten {
                resumed_holds: false
            },
            "resumed: the newest last-prompt names d1"
        );
        assert_eq!(
            descends(&at(&directory.path().join("missing.jsonl"), "a2"), "a2"),
            Branch::Unknown
        );
    }

    /// The Sim run 5c fork. The terminal opened mid-turn ends stale; a later resume that landed
    /// on its branch (made newest by its startup records and its held-back prompt) stays stale,
    /// although it started after the writer's last output; a resume of the writer's branch holds
    /// the conversation.
    #[test]
    fn a_resume_that_lands_on_the_forked_branch_stays_stale() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = forked_transcript(directory.path());
        let mut leases = Leases::default();
        leases.writer_reported("t", "a1", START);
        leases.writer_turn_started("t");
        leases.record_output("t", START - 1_000);
        assert_eq!(
            leases.opened(
                "t",
                tui(true),
                Some(at(&transcript, "h2")),
                headless(),
                &descends,
                START + 1_000
            ),
            Opened::Release {
                session_id: "agit-h".into()
            }
        );
        leases.record_output("t", START + 9_000);
        leases.writer_reported("t", "a2", START);
        assert!(leases.headless_ended("t", "agit-h", &descends, START + 9_100));
        assert_eq!(leases.get("t").unwrap().state, State::Stale);

        let reload = Terminal {
            pid: 8,
            started_ms: START + 60_000,
            plugin: true,
        };
        assert_eq!(
            leases.opened(
                "t",
                reload,
                Some(at(&transcript, "r1")),
                None,
                &descends,
                START + 61_000
            ),
            Opened::Granted(State::Stale),
            "the start-time rule alone would hold it"
        );
        assert_eq!(
            leases.opened(
                "t",
                reload,
                Some(at(&transcript, "r1")),
                None,
                &descends,
                START + 62_000
            ),
            Opened::Unchanged(State::Stale),
            "the records it adds keep it on that branch"
        );

        let resumed = Terminal {
            pid: 9,
            started_ms: START + 70_000,
            plugin: true,
        };
        assert_eq!(
            leases.opened(
                "t",
                resumed,
                Some(at(&transcript, "g1")),
                None,
                &descends,
                START + 71_000
            ),
            Opened::Granted(State::Held)
        );
    }

    /// With the writer's last record known, a terminal with the plugin is held back until its
    /// plugin names the branch it continues, and is judged again when it does. The terminal's
    /// settings hook announces it first, with no branch, by its start time.
    #[test]
    fn a_plugin_terminal_is_held_until_it_names_its_branch() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = forked_transcript(directory.path());
        let mut leases = Leases::default();
        leases.writer_reported("t", "a2", START);
        leases.record_output("t", START - 60_000);
        assert_eq!(
            leases.opened("t", tui(false), None, None, &descends, START + 1_000),
            Opened::Granted(State::Held)
        );
        assert_eq!(
            leases.opened("t", tui(true), None, None, &descends, START + 1_100),
            Opened::Granted(State::Stale),
            "no branch named yet"
        );
        assert_eq!(
            leases.opened(
                "t",
                tui(true),
                Some(at(&transcript, "unwritten")),
                None,
                &descends,
                START + 1_200
            ),
            Opened::Unchanged(State::Stale),
            "a branch that cannot be read"
        );
        assert_eq!(
            leases.opened(
                "t",
                tui(true),
                Some(at(&transcript, "g1")),
                None,
                &descends,
                START + 1_300
            ),
            Opened::Granted(State::Held)
        );
        assert_eq!(leases.get("t").unwrap().loaded, Some(at(&transcript, "g1")));
    }

    /// Fail-closed: in a conversation this daemon ran headless, a terminal with the plugin is
    /// held back whenever the writer's last record is unknown: while a turn runs, when the turn
    /// ended with no report, after a writer ended unobserved. A terminal without the plugin, and
    /// a conversation never run headless here, keep the start-time rule.
    #[test]
    fn with_the_writers_last_record_unknown_a_plugin_terminal_is_held_back() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = forked_transcript(directory.path());
        let on_writer_branch = Some(at(&transcript, "g1"));
        let unknown: [&dyn Fn(&mut Leases); 3] = [
            &|leases: &mut Leases| leases.writer_turn_started("t"),
            &|_: &mut Leases| {},
            &|leases: &mut Leases| leases.record_unobserved_output("t", START - 60_000),
        ];
        for (case, lose) in unknown.iter().enumerate() {
            let mut leases = Leases::default();
            leases.writer_reported("t", "a1", START - 120_000);
            leases.writer_turn_started("t");
            leases.record_output("t", START - 60_000);
            if case == 1 {
                // The turn ends, and its report never arrives.
                leases.record_output("t", START - 59_000);
            }
            lose(&mut leases);
            assert_eq!(
                leases.opened(
                    "t",
                    tui(true),
                    on_writer_branch.clone(),
                    None,
                    &descends,
                    START + 1_000
                ),
                Opened::Granted(State::Stale),
                "case {case}: the start-time rule alone would hold it"
            );
            let plain = Terminal {
                pid: 8,
                ..tui(false)
            };
            leases.threads.clear();
            assert_eq!(
                leases.opened("t", plain, None, None, &descends, START + 2_000),
                Opened::Granted(State::Held),
                "case {case}: without the plugin, the start time judges"
            );
        }

        let mut never = Leases::default();
        assert_eq!(
            never.opened("t", tui(true), None, None, &descends, START + 1_000),
            Opened::Granted(State::Held),
            "a conversation never run headless here"
        );
    }

    /// The branch rule reads at most MAX_TRANSCRIPT_BYTES of a transcript; past the cap the
    /// answer is unknown and a terminal with the plugin is held back. An unchanged transcript is
    /// not read again, and one that grew is.
    #[test]
    fn the_transcript_walk_is_capped_and_reread_only_when_it_changes() {
        let directory = tempfile::tempdir().unwrap();
        let transcript = forked_transcript(directory.path());
        let size = std::fs::metadata(&transcript).unwrap().len();
        let g1 = at(&transcript, "g1");
        assert_eq!(descends_within(&g1, "a2", size), Branch::Holds);
        assert_eq!(
            descends_within(&g1, "a2", size - 1),
            Branch::Unknown,
            "past the cap"
        );

        let mut leases = Leases::default();
        leases.writer_reported("t", "a2", START);
        let capped = |loaded: &Loaded, ancestor: &str| descends_within(loaded, ancestor, size - 1);
        assert_eq!(
            leases.opened(
                "t",
                tui(true),
                Some(g1.clone()),
                None,
                &capped,
                START + 1_000
            ),
            Opened::Granted(State::Stale)
        );

        let first = parents(&transcript, size).unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &first,
            &parents(&transcript, size).unwrap()
        ));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, b"\n{\"uuid\":\"g2\",\"parentUuid\":\"g1\"}").unwrap();
        drop(file);
        assert_eq!(
            descends(&at(&transcript, "g2"), "a2"),
            Branch::Holds,
            "read again"
        );
    }

    /// A conversation as Claude Code really writes it when a terminal resumes it: the
    /// terminal's start records are reported through `session.append` (`t-start` here) but
    /// written to the file only with its first prompt, so the terminal's record is not on disk.
    /// `rows` follow the web's turns (`u1`, `a1`, `u2`, `a2`); `resumed` is the newest
    /// `last-prompt` record's leaf.
    fn real_ordering(directory: &Path, rows: &[&str], resumed: Option<&str>) -> PathBuf {
        let path = directory.join("conversation.jsonl");
        let mut lines = vec![
            r#"{"type":"user","uuid":"u1","parentUuid":null}"#.to_owned(),
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1"}"#.to_owned(),
            r#"{"type":"last-prompt","lastPrompt":"first","leafUuid":"a1","sessionId":"t"}"#
                .to_owned(),
            r#"{"type":"user","uuid":"u2","parentUuid":"a1"}"#.to_owned(),
            r#"{"type":"assistant","uuid":"a2","parentUuid":"u2"}"#.to_owned(),
        ];
        lines.extend(rows.iter().map(|row| row.to_string()));
        if let Some(resumed) = resumed {
            lines.push(format!(
                r#"{{"type":"last-prompt","lastPrompt":"second","leafUuid":"{resumed}","sessionId":"t"}}"#
            ));
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    /// A terminal with the plugin opens, naming its start record `t-start`; the state it gets.
    fn opened_by_plugin(leases: &mut Leases, transcript: &Path, started_ms: i64) -> State {
        let terminal = Terminal {
            pid: 8,
            started_ms,
            plugin: true,
        };
        match leases.opened(
            "t",
            terminal,
            Some(at(transcript, "t-start")),
            None,
            &descends,
            started_ms + 1_000,
        ) {
            Opened::Granted(state) | Opened::Unchanged(state) => state,
            other => panic!("{other:?}"),
        }
    }

    /// A conversation the web ran headless, its writer's last record `a2`, its last output at
    /// `output_ms`.
    fn ran_headless(output_ms: i64) -> Leases {
        let mut leases = Leases::default();
        leases.record_output("t", output_ms);
        leases.writer_reported("t", "a2", output_ms);
        leases
    }

    /// While a terminal's own record is not on disk, it is judged by the record Claude Code
    /// resumed and by its start time. Ignoring the resumed record (holding every such terminal
    /// back) fails the idle case.
    #[test]
    fn an_unwritten_terminal_record_is_judged_by_what_claude_code_resumed() {
        let directory = tempfile::tempdir().unwrap();

        // Idle resume: the web's turns ended long before the terminal started.
        let idle = real_ordering(directory.path(), &[], Some("a2"));
        let mut leases = ran_headless(START - 60_000);
        assert_eq!(opened_by_plugin(&mut leases, &idle, START), State::Held);

        // Mid-turn: the web's turn ended after the terminal started, although Claude Code
        // resumes its record by now.
        let mut leases = ran_headless(START + 5_000);
        assert_eq!(opened_by_plugin(&mut leases, &idle, START), State::Stale);

        // The same terminal asks again for as long as it waits (the plugin re-checks every few
        // seconds); it keeps the history it loaded, so the web's turn never ages out of its hold.
        let terminal = Terminal {
            pid: 8,
            started_ms: START,
            plugin: true,
        };
        for now_ms in [START + 5_000 + OUTPUT_MEMORY_MS + 1, START + 24 * 3_600_000] {
            assert_eq!(
                leases.opened(
                    "t",
                    terminal,
                    Some(at(&idle, "t-start")),
                    None,
                    &descends,
                    now_ms
                ),
                Opened::Unchanged(State::Stale),
                "re-queried at {now_ms}"
            );
        }

        // Reload after a held-back prompt (Sim run 5c): the stale terminal's start records and
        // its drop notice were written under the first reply, and the drop moved the resumed
        // record to that notice.
        let dropped = real_ordering(
            directory.path(),
            &[
                r#"{"type":"attachment","uuid":"h1","parentUuid":"a1"}"#,
                r#"{"type":"attachment","uuid":"h2","parentUuid":"h1"}"#,
                r#"{"type":"system","uuid":"d1","parentUuid":"h2"}"#,
            ],
            Some("d1"),
        );
        let mut leases = ran_headless(START - 60_000);
        assert_eq!(opened_by_plugin(&mut leases, &dropped, START), State::Stale);

        // Fail-closed: the newest last-prompt names no leaf, or there is none at all.
        let withdrawn = real_ordering(
            directory.path(),
            &[r#"{"type":"last-prompt","leafUuid":null,"explicit":true}"#],
            None,
        );
        let mut leases = ran_headless(START - 60_000);
        assert_eq!(
            opened_by_plugin(&mut leases, &withdrawn, START),
            State::Stale
        );
        let text = std::fs::read_to_string(&idle).unwrap();
        let none: Vec<_> = text
            .lines()
            .filter(|line| !line.contains("last-prompt"))
            .collect();
        std::fs::write(&idle, none.join("\n")).unwrap();
        let mut leases = ran_headless(START - 60_000);
        assert_eq!(opened_by_plugin(&mut leases, &idle, START), State::Stale);

        // Once the terminal's record is on disk, that record alone decides.
        let written = real_ordering(
            directory.path(),
            &[r#"{"type":"attachment","uuid":"t-start","parentUuid":"a1"}"#],
            Some("a2"),
        );
        let mut leases = ran_headless(START - 60_000);
        assert_eq!(opened_by_plugin(&mut leases, &written, START), State::Stale);
    }

    /// A terminal waiting on a conversation keeps that conversation's output time while 257
    /// other conversations produce output, and stays stale; a conversation ran headless whose
    /// output time is gone anyway is judged as if its writer had written since the start.
    #[test]
    fn a_waiting_terminal_stays_stale_when_other_conversations_fill_the_output_memory() {
        let directory = tempfile::tempdir().unwrap();
        let idle = real_ordering(directory.path(), &[], Some("a2"));
        let mut leases = ran_headless(START + 5_000);
        assert_eq!(opened_by_plugin(&mut leases, &idle, START), State::Stale);
        for other in 0..=MAX_THREADS {
            let thread = format!("other-{other}");
            leases.record_output(&thread, START + 10_000 + other as i64);
            leases.writer_reported(&thread, "x", START + 10_000 + other as i64);
        }
        assert!(
            leases.output.contains_key("t"),
            "the waiting terminal's evidence is kept"
        );
        assert_eq!(opened_by_plugin(&mut leases, &idle, START), State::Stale);

        let mut forgotten = ran_headless(START - 60_000);
        forgotten.output.remove("t");
        assert_eq!(
            opened_by_plugin(&mut forgotten, &idle, START),
            State::Stale,
            "a missing output time is not 'no output'"
        );
    }

    /// The newest last-prompt record is found from the end of the file, across read chunks;
    /// a record that only mentions one in its text is not it.
    #[test]
    fn the_resumed_record_is_the_newest_last_prompt_leaf() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("conversation.jsonl");
        let filler = format!(
            r#"{{"type":"user","uuid":"f","message":"{}"}}"#,
            "x".repeat(70_000)
        );
        let lines = [
            r#"{"type":"last-prompt","lastPrompt":"old","leafUuid":"old"}"#.to_owned(),
            filler.clone(),
            r#"{"type":"last-prompt","lastPrompt":"{\"leafUuid\":\"x\"}","leafUuid":"new"}"#
                .to_owned(),
            filler.clone(),
            filler,
            r#"{"type":"user","message":"{\"type\":\"last-prompt\",\"leafUuid\":\"fake\"}"}"#
                .to_owned(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        assert_eq!(
            resumed_leaf(&path, MAX_TRANSCRIPT_BYTES).as_deref(),
            Some("new")
        );
        let size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(resumed_leaf(&path, size - 1), None, "past the cap");
    }

    /// A handover whose writer never reports its end resolves at the deadline sweep only when
    /// the writer's row is gone and its recorded process has exited: the waiting terminal then
    /// takes over as stale. Before the deadline, while the row remains, while the process
    /// runs, or when it was never recorded, the conversation keeps waiting.
    #[test]
    fn an_unobserved_writer_end_resolves_only_when_its_process_is_gone() {
        let writer = Terminal {
            pid: 9,
            started_ms: START - 5_000,
            plugin: false,
        };
        let waiting = |record: bool| {
            let mut leases = Leases::default();
            leases.open("t", tui(true), headless(), START);
            if record {
                leases.release_writer("t", writer);
            }
            leases
        };
        let after = START + RELEASE_DEADLINE_MS;
        let only_terminal = |terminal: &Terminal| terminal.pid == 7;
        let row_gone = |_: &str| false;

        let mut leases = waiting(true);
        assert!(
            leases
                .lift_unobserved_releases(row_gone, only_terminal, after - 1)
                .is_empty(),
            "before the deadline"
        );
        assert!(
            leases
                .lift_unobserved_releases(|_| true, only_terminal, after)
                .is_empty(),
            "the row remains"
        );
        assert!(
            leases
                .lift_unobserved_releases(row_gone, |_| true, after)
                .is_empty(),
            "the writer process still runs"
        );
        assert_eq!(leases.get("t").unwrap().state, State::Releasing);
        assert!(
            waiting(false)
                .lift_unobserved_releases(row_gone, only_terminal, after)
                .is_empty(),
            "a writer never recorded can't be judged"
        );

        assert_eq!(
            leases.lift_unobserved_releases(row_gone, only_terminal, after),
            ["t"]
        );
        let lease = leases.get("t").unwrap();
        assert!(matches!(lease.holder, Holder::Tui(Terminal { pid: 7, .. })));
        assert_eq!(lease.state, State::Stale);
        assert_eq!(lease.reason, Reason::HeadlessEndedUnobserved);
        assert!(lease.notice_for("t", &tui(true)).is_some());
    }

    #[test]
    fn device_code_is_the_last_four_id_characters() {
        assert_eq!(
            device_code("01a1889c-7288-7160-9be5-97b3bc7dAB12").as_deref(),
            Some("ab12")
        );
        assert_eq!(device_code("a-b"), None);
    }
}
