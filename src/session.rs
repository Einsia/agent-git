//! Raw session dump management (new model: don't distill facts, version the agent's full session directly).
//!
//! Claude Code dumps its entire session into ~/.claude/projects/<slug>/ on its own:
//!   <uuid>.jsonl              full transcript
//!   <uuid>/subagents/*.jsonl  subagent transcripts
//!   <uuid>/tool-results/*.txt large tool results
//!   memory/                   memory
//! `agit a snap` mirrors this blob into sessions/<env-slug>/<runtime>/ of the store belonging to the
//! agent that PRODUCED each session (`route`) — the dump is per project, so one pass can feed several
//! stores, and one store is fed by several projects.

use crate::adapter::claude_code;
use crate::commands::Attribution;
use crate::scope::{self, Scope};
use crate::ui;
use crate::{errln, outln};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub const SESSIONS_SUBDIR: &str = "sessions";

/// The watcher's pidfile, in `<env>/.agit/` (gitignored by `agit init`).
const WATCH_PID: &str = "agit-watch.pid";

/// The runtimes agit speaks, from the adapter registry — the single place they are named. Peers:
/// there is no first among them, so they read alphabetically wherever a user sees them, and no code
/// path may fall back to one of them as a default.
pub fn runtimes() -> Vec<&'static str> {
    crate::adapter::names()
}

/// The runtimes named the way the user should always see them: alphabetically, in one breath.
pub fn runtime_list() -> String {
    runtimes().join(", ")
}

/// Whether a runtime holds any session **this project owns**.
///
/// Ownership is the cwd recorded in the transcript, the same rule the collectors use — the dump
/// directory merely existing is not enough, since claude's slug dir can be occupied entirely by a
/// colliding project.
pub fn has_live_sessions(rt: &str, env: &Path) -> bool {
    crate::adapter::get(rt).map(|a| !a.project_sessions(env).is_empty()).unwrap_or(false)
}

/// The runtimes with sessions for this project, alphabetically.
pub fn live_runtimes(env: &Path) -> Vec<&'static str> {
    runtimes().into_iter().filter(|rt| has_live_sessions(rt, env)).collect()
}

/// The runtimes with sessions already in the Agent Store, alphabetically.
///
/// **Both layouts, always.** A store partitioned by environment while this globbed only the flat
/// `sessions/<rt>/` reported ZERO runtimes, so `agit a merge` died with "No claude-code, codex sessions
/// found to merge" against a store visibly full of them. Flat stores exist on disk and are never
/// migrated, so neither layout is the legacy one to be read second.
pub fn store_runtimes(agent: &Path) -> Vec<&'static str> {
    let root = agent.join(SESSIONS_SUBDIR);
    runtimes().into_iter().filter(|rt| runtime_has_sessions(&root, rt)).collect()
}

fn runtime_has_sessions(sessions: &Path, rt: &str) -> bool {
    if dir_holds_transcript(&sessions.join(rt)) {
        return true;
    }
    std::fs::read_dir(sessions)
        .map(|d| {
            d.filter_map(|e| e.ok()).any(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false) && dir_holds_transcript(&e.path().join(rt)))
        })
        .unwrap_or(false)
}

fn dir_holds_transcript(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "jsonl")).unwrap_or(false)))
        .unwrap_or(false)
}

/// The store's partition for an environment: `sessions/<env-slug>/<rt>/` (§6). One store is shared by
/// several code repos, which is the point of the model — so a flat `sessions/<rt>/` mixes every repo's
/// transcripts into one folder, and leaves nothing to tell `agit start` which repo the memory it is
/// carrying was recorded in.
///
/// `slug_for` maps every non-alphanumeric character to `-`, so `/my/app`, `/my-app`, `/my_app` and
/// `/my.app` all collide on one slug. Survivable HERE and nowhere else: a collision groups two repos'
/// sessions under one folder, and each session's recorded cwd — not this slug — stays the authority on
/// where it ran. It is a partition key, never an identity.
fn env_slug(env: &Path) -> String {
    claude_code::slug_for(env)
}

/// Resolve the one runtime a single-runtime command acts on. There is NO default: an explicit flag
/// wins, else the only runtime actually present, else we ask. Picking a runtime because it is spelled
/// "claude" is exactly the framing bug this exists to prevent, so ambiguity is never resolved silently.
///
/// `present` is the caller's notion of presence (live dumps for capture, the store for merge), and
/// `what` completes the sentence "which runtime should agit …?".
pub fn resolve_runtime(explicit: Option<&str>, present: &[&'static str], what: &str) -> Result<String> {
    use std::io::{stdin, stdout, BufRead, IsTerminal, Write};
    if let Some(r) = explicit {
        let rt = normalize(r);
        if !runtimes().contains(&rt.as_str()) {
            bail!("unknown runtime `{r}`. Registered: {}", runtime_list());
        }
        return Ok(rt);
    }
    match present {
        [] => bail!("No {} sessions found to {what}.", runtime_list()),
        [only] => Ok(only.to_string()),
        many => {
            let names = many.join(", ");
            if !stdin().is_terminal() {
                bail!("multiple runtimes have sessions ({names}): say which with --from <runtime>.");
            }
            outln!("Sessions exist for {names}. Which runtime should agit {what}?");
            for (i, rt) in many.iter().enumerate() {
                outln!("  {}) {rt}", i + 1);
            }
            print!("Choice [1-{}]: ", many.len());
            let _ = stdout().flush();
            let mut line = String::new();
            stdin().lock().read_line(&mut line).ok();
            let pick = line.trim();
            // accept either the number or the runtime's name
            many.iter()
                .position(|rt| *rt == pick)
                .or_else(|| pick.parse::<usize>().ok().filter(|n| (1..=many.len()).contains(n)).map(|n| n - 1))
                .map(|i| many[i].to_string())
                .ok_or_else(|| anyhow::anyhow!("no runtime picked; rerun with --from <runtime> ({names})"))
        }
    }
}

/// Locate the runtime session dump directory for the current project.
fn source_dir(runtime: &str, cwd: &Path) -> Result<PathBuf> {
    match runtime {
        "claude-code" | "claude" | "cc" => {
            let dir = claude_code::projects_dir()?.join(claude_code::slug_for(cwd));
            if !dir.exists() {
                bail!(
                    "Could not find the Claude Code session directory for this project: {}\n\
                     (has this project not been run in Claude Code yet?)",
                    dir.display()
                );
            }
            Ok(dir)
        }
        other => bail!("session dump for runtime `{other}` isn't wired up yet (see src/session.rs)"),
    }
}

// ─────────────────── Capture: whose session is this, and which store does it go to? ───────────────────

/// One store's share of a capture pass: the sessions attributed to the agent that owns it.
struct Routed {
    store: PathBuf,
    /// The agent's current label. `None` = the legacy nested store, which carries no identity.
    agent: Option<String>,
    /// Disclosed once per pass rather than once per session.
    note: Option<String>,
    sessions: Vec<Owned>,
}

/// One captured session, and the agent `agit start` recorded as its author.
struct Owned {
    src: PathBuf,
    id: String,
    by: Option<Attribution>,
}

/// A repo from before agent identity: nothing here names an agent, so there is nothing to attribute
/// against and the legacy nested store is the only place its sessions can go. The cutover (§12, `agit a
/// import`) deletes this branch; until then, refusing would strand every repo that predates identity.
fn predates_identity(env: &Path) -> bool {
    !env.join(crate::agent::BINDING_FILE).exists()
        && std::env::var("AGIT_AGENT").map(|v| v.trim().is_empty()).unwrap_or(true)
        && matches!(crate::agent::read_active(env), Ok(None))
}

/// Which store an attributed session goes to, and what to call its agent. `None` = a launch record
/// naming an agent this machine no longer has: warned and skipped, since one dead record must not stop
/// every other agent being captured.
fn store_for(by: Option<&Attribution>, id: &str) -> Result<Option<(PathBuf, Option<String>)>> {
    let Some(by) = by else {
        return Ok(Some((scope::root_for(Scope::Agent)?, None)));
    };
    // The aid is the identity, so the store AND the current label both come from it: the record's name
    // is a snapshot from launch time, and a rename must not orphan it.
    match crate::agent::info(by.aid()) {
        Ok(ag) => Ok(Some((ag.store, Some(ag.name)))),
        Err(e) => {
            errln!(
                "  ⚠ {id} not captured: launched by {} ({}), absent from this machine ({e:#})",
                by.name(),
                by.aid()
            );
            Ok(None)
        }
    }
}

/// Route this runtime's sessions to the store of the agent that produced each one.
///
/// This is what the launch record is for (§6). Both runtimes dump per PROJECT
/// (`~/.claude/projects/<cwd-slug>/`), so two agents working in one repo write into the SAME folder.
/// The active pointer cannot tell their sessions apart, so attributing by it misfiles silently — into
/// the wrong agent, and from there to the wrong team. One pass therefore writes into as many stores as
/// there are agents that worked here; collapsing it back to a single store is the bug.
fn route(rt: &str, env: &Path) -> Result<(Vec<Routed>, String)> {
    let (owned, source_desc) = live_sessions(rt, env)?;
    let mut out: Vec<Routed> = Vec::new();
    for (src, id) in owned {
        // The launch record, else the repo's default agent — never the active pointer.
        let by = match crate::commands::attribute_session(&id) {
            Ok(a) => Some(a),
            Err(_) if predates_identity(env) => None,
            // Any other failure — an unknown $AGIT_AGENT, a binding whose id no longer matches its
            // store — is a real error. Filing the transcript somewhere anyway is the silent misfiling
            // this routing exists to stop, and one pushed to the wrong team cannot be recalled.
            Err(e) => return Err(e),
        };
        let Some((store, agent)) = store_for(by.as_ref(), &id)? else { continue };
        let i = match out.iter().position(|r| r.store == store) {
            Some(i) => i,
            None => {
                out.push(Routed { store, agent, note: None, sessions: Vec::new() });
                out.len() - 1
            }
        };
        // One fallback anywhere in the group is worth disclosing, even if the first session had a record.
        if out[i].note.is_none() {
            out[i].note = by.as_ref().and_then(Attribution::note);
        }
        out[i].sessions.push(Owned { src, id, by });
    }
    Ok((out, source_desc))
}

/// This runtime's sessions that belong to this project: (transcript, session id), plus how to describe
/// where they came from. Both come from the adapter — ownership rules and directory layout are the
/// runtime's own business (claude splits by project slug; codex by date, with the cwd filter).
///
/// An absent runtime directory yields an EMPTY list, not an error, so `snap --from <rt>` and
/// `agit watch` behave the same for every runtime whether or not the project has run there; the source
/// description still names where it looked.
fn live_sessions(rt: &str, env: &Path) -> Result<(Vec<(PathBuf, String)>, String)> {
    let adapter = crate::adapter::get(rt)?;
    Ok((adapter.project_sessions(env), adapter.source_desc(env)))
}

// ─────────────── stranded sessions: started in the wrong dir, warn + relocate ───────────────

/// Sessions across every runtime that were DROPPED from capture because their recorded cwd != `env`, yet
/// are plausibly meant for THIS repo (a parent dir, or the same repo at another path). This does NOT
/// loosen ownership — capture still drops them; this collects the same drops so we can warn and so
/// `agit relocate` can bring them in. A genuinely-unrelated project is not stranded_here and never appears.
pub fn stranded_here(env: &Path) -> Vec<crate::adapter::StrandedSession> {
    let mut out = vec![];
    for rt in runtimes() {
        if let Ok(a) = crate::adapter::get(rt) {
            out.extend(a.stranded_sessions(env));
        }
    }
    out
}

/// Print ONE informational note per wrong directory when capture drops sessions that look meant for THIS
/// repo, pointing at `agit relocate`. Never fatal — capture of the real, matching sessions still
/// succeeds. Silent when nothing is stranded-and-plausible (an unrelated drop is correct, not warned).
pub fn warn_stranded(env: &Path) {
    let stranded = stranded_here(env);
    if stranded.is_empty() {
        return;
    }
    // Group by the directory they actually ran in; BTreeMap keeps the note order stable across runs.
    let mut by_cwd: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for s in &stranded {
        *by_cwd.entry(s.recorded_cwd.clone()).or_default() += 1;
    }
    for (cwd, n) in by_cwd {
        errln!(
            "note: {n} session(s) ran in {cwd}, not this repo ({}). started in the wrong directory? run `agit relocate` to bring them here.",
            env.display()
        );
    }
}

/// Capture into the store the sessions THIS repo now owns for `rt` — used by `agit relocate` after it
/// installs a relocated session under `env`'s slug and records its launch. Quiet: it reuses the same
/// route → mirror → gated-commit path as snap, without snap's per-store banner, and returns how many
/// files it wrote. Errors from a single store are surfaced but never abort the whole relocate.
pub fn capture_relocated(env: &Path, rt: &str) -> Result<usize> {
    let rt = normalize(rt);
    let (routed, _) = route(&rt, env)?;
    let mut written = 0usize;
    for r in &routed {
        let _lock = lock_store(&r.store)?;
        let (stats, _hits, _dst) = mirror_owned(&rt, env, r)?;
        let mut count = 0u64;
        commit_snap(&r.store, &rt, &mut count);
        written += stats.added + stats.updated;
    }
    Ok(written)
}

/// `agit a snap [--from <runtime>]` — mirror session dumps into the Agent Store, once.
///
/// With no `--from` there is nothing to default to: claude-code and codex are peers, so snap captures
/// BOTH (the shape `watch` already uses), skipping quietly whichever has no sessions for this project.
/// An explicit `--from` is a different contract — the user named a runtime, so its absence is an error.
pub fn snap(runtime: Option<&str>, capture_harness: bool) -> Result<i32> {
    // Validate an explicit runtime BEFORE any filesystem walk: `--from bogus` used to reach the
    // per-runtime path and die with a confusing "isn't wired up yet", and under `--watch` it became a
    // silent permanent no-op — the watcher polls a runtime that can never exist. Fail-open on a typo is
    // exactly what no-default-runtime exists to prevent, so every path funnels through one check.
    if let Some(r) = runtime {
        // through the SAME funnel: resolve_runtime returns on the explicit branch before reading
        // `present`, so an unknown name fails here instead of dying later as "isn't wired up yet".
        let r = resolve_runtime(Some(r), &[], "snap")?;
        return sync(&r, capture_harness);
    }
    let env = scope::env_root()?;
    // Before deciding there is nothing to capture, disclose any sessions dropped for running in the wrong
    // directory — otherwise an all-stranded project (everything ran in the parent) looks empty and the
    // user's work is silently stranded. Informational; the real capture below is unaffected.
    warn_stranded(&env);
    let live = live_runtimes(&env);
    if live.is_empty() {
        bail!(
            "No {} sessions found for this project.\n\
             (has it been run in either yet? `agit adapter` lists the runtimes; `--from <runtime>` forces one.)",
            runtime_list()
        );
    }
    // A blocked snap in ANY runtime propagates as a non-zero exit (see snap_one), so capturing several
    // runtimes at once never masks a held-back secret behind a peer's clean capture.
    let mut code = 0;
    for rt in &live {
        code = code.max(snap_one(rt, &env, capture_harness)?);
    }
    Ok(code)
}

/// `agit a snap --from <runtime>` — mirror one named runtime's dump into the Agent Store.
/// `capture_harness` also captures the project's MCP/skills/config (redacting secrets); `--no-harness` skips it.
pub fn sync(runtime: &str, capture_harness: bool) -> Result<i32> {
    let env = scope::env_root()?;
    warn_stranded(&env);
    snap_one(&normalize(runtime), &env, capture_harness)
}

fn snap_one(rt: &str, env: &Path, capture_harness: bool) -> Result<i32> {
    let rt = normalize(rt);
    let (routed, source_desc) = route(&rt, env)?;

    outln!("Mirrored the session dump for {}:", rt);
    outln!("  source : {source_desc}");
    if routed.is_empty() {
        outln!("  (nothing to mirror: no sessions this project owns)");
        return Ok(0);
    }

    // How many snapshots this run actually committed; commit_snap stamps each `● snapped … (#n)`.
    let mut count = 0u64;
    // Whether the gate held any snap out of history. A clean snap exits 0; a blocked one exits non-zero
    // so `snap && push` and `set -e` stop here (and it is consistent with `agit a scan`, which already
    // exits non-zero on the same secret). The mirror on disk still happened either way.
    let mut blocked = false;
    for r in &routed {
        if let Some(n) = &r.note {
            errln!("  note   : {n}");
        }
        // Held across mirror + harness capture + commit: everything this pass writes into the store,
        // and the read-modify-write commit_snap does on the store's index and HEAD.
        let _lock = lock_store(&r.store)?;
        let (stats, _hits, dst) = mirror_owned(&rt, env, r)?;
        let who = match &r.agent {
            Some(a) => format!("   ({a} · {} session(s))", r.sessions.len()),
            None => String::new(),
        };
        outln!("  target : {}{who}", dst.display());
        outln!("  files  : {} files ({} updated / {} added), {} bytes", stats.total, stats.updated, stats.added, stats.bytes);

        // Capture the harness (MCP servers / skills / config) alongside the sessions, redacting
        // secrets. The harness is project-scoped, so every agent that worked here carries its own
        // copy — a store a teammate clones has to stand on its own.
        if capture_harness {
            match crate::harness::capture(&r.store, env, &rt) {
                Ok(h) if h.files > 0 => {
                    outln!("  harness: {} files ({} secret field(s) redacted)", h.files, h.redactions.len());
                    for w in &h.warnings {
                        errln!("  ⚠ {w}");
                    }
                }
                Ok(_) => {}
                // Harness capture must never fail the snap — the session dump is already mirrored.
                Err(e) => errln!("  ⚠ harness capture skipped: {e:#}"),
            }
        }

        // Mirror AND commit in one step, the same mirror → stage → gate → commit the watch tick runs.
        // commit_snap takes no lock of its own — it assumes the caller holds the store lock (the
        // `_lock` above), so calling it here is correct and re-locking would deadlock. Nothing staged
        // is a clean no-op (no empty commit); a suspected secret blocks the commit, leaves the mirror
        // on disk, and discloses the AGIT_ALLOW_SECRETS override — commit_snap warns for all of these.
        blocked |= commit_snap(&r.store, &rt, &mut count);
    }
    // Non-zero (2) when the gate blocked a snap, so a scripted `snap && push` does not push a store whose
    // latest capture was held out of history.
    Ok(if blocked { 2 } else { 0 })
}

// ─────────────────────── The store lock: one store, many concurrent writers ───────────────────────

/// Advisory lockfile in the store's `.git`, so it is never tracked, scanned, or pushed. Named apart
/// from git's own `index.lock`: this guards agit's whole read-modify-write (mirror → add → commit),
/// which git's lock does not span.
const STORE_LOCK: &str = "agit-store.lock";

/// Long enough to outlast a snap of a large dump, short enough that a wedged holder is reported rather
/// than waited on forever. A watcher that loses a tick tries again next tick; a user gets an error.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Held for as long as the writer needs the store; released on drop, including on `?` and on panic.
#[derive(Debug)]
pub struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Take the store's write lock, waiting up to `LOCK_WAIT`.
///
/// A store used to belong to one repo, so its writers were serialized by there only being one of them.
/// It is now shared by several (§6), which makes `snap`, `restore` and the pairing record concurrent
/// writers to ONE index and ONE HEAD **by design** — two repos snapping the same agent at once corrupt
/// each other's index.
///
/// Advisory, and honest about it: nothing stops a writer that never asks. It serializes agit's own
/// paths, which is what the shared store made unsafe.
pub fn lock_store(store: &Path) -> Result<StoreLock> {
    use std::io::{ErrorKind, Write};
    let dir = store.join(".git");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot prepare {} for locking", dir.display()))?;
    let path = dir.join(STORE_LOCK);
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut f) => {
                // Best-effort: the pid names the holder in an error. A lock we hold but cannot stamp is
                // still a lock, so a failed write must not fail the take.
                let _ = writeln!(f, "{}", std::process::id());
                return Ok(StoreLock { path });
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("cannot create {}", path.display())),
        }
        // A holder killed mid-write (SIGKILL, a crash, a reboot) never released it. Break that lock as
        // soon as we can PROVE the pid is gone, rather than making every later write sit out the full
        // timeout. An unreadable pidfile proves nothing — it is equally a holder that has created the
        // file microseconds before stamping it — so it waits out the deadline instead.
        if let Some(pid) = read_pid(&path) {
            if !pid_alive(pid) {
                let _ = std::fs::remove_file(&path);
                continue;
            }
        }
        if std::time::Instant::now() >= deadline {
            let who = match read_pid(&path) {
                Some(p) => format!("pid {p}"),
                None => "an unknown process (the lock names no pid)".to_string(),
            };
            bail!(
                "{} is locked by {who} and did not release it within {}s.\n\
                 \x20      Another agit is writing this agent; one store is shared by every repo that tracks it.\n\
                 \x20      If that process is gone, clear the lock: rm {}",
                store.display(),
                LOCK_WAIT.as_secs(),
                path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Mirror one agent's share of the dump into ITS store, and secret-scan what landed.
///
/// Runtimes differ in storage model, but the ownership rule is the same for both: mirror ONLY this
/// project's sessions, decided by the cwd recorded in the transcript (`live_sessions`).
fn mirror_owned(rt: &str, env: &Path, r: &Routed) -> Result<(Stats, usize, PathBuf)> {
    let part = r.store.join(SESSIONS_SUBDIR).join(env_slug(env)).join(rt);
    let flat = r.store.join(SESSIONS_SUBDIR).join(rt);
    std::fs::create_dir_all(&part)?;
    let mut stats = Stats::default();
    let mut used: Vec<PathBuf> = Vec::new();
    for o in &r.sessions {
        // A session already captured under the pre-partition layout keeps its place. Re-mirroring it
        // into the partition would leave the store holding the SAME session twice, one copy frozen at
        // the moment the layout changed; moving it would rewrite paths in a git repo nobody asked to
        // rewrite. Only sessions new to this store land partitioned.
        let dst = if flat.join(format!("{}.jsonl", o.id)).exists() { &flat } else { &part };
        if !used.contains(dst) {
            used.push(dst.clone());
        }
        let dp = dst.join(format!("{}.jsonl", o.id));
        let copied = copy_if_changed(&o.src, &dp, &mut stats)?;
        // The sidecar is a pure function of the transcript, so rewriting it unchanged would turn every
        // watch tick into a commit. Building it re-reads the transcript, so do it only when one moved.
        if copied || !crate::commands::sidecar_path(&dp).exists() {
            // The comparison set for `/branch` detection is the other sessions this pass routes to the
            // SAME store (one store == one agent), which is the "same agent+env" cohort the model wants.
            let siblings: Vec<(PathBuf, String)> =
                r.sessions.iter().filter(|s| s.id != o.id).map(|s| (s.src.clone(), s.id.clone())).collect();
            let lineage = detect_lineage(rt, &o.src, &o.id, &siblings);
            write_sidecar(&dp, o, env, rt, &r.store, &lineage)?;
        }
        // a session's sidecars (subagents/, tool-results/) live under a dir named for its id
        if let Some(side) = o.src.parent().map(|d| d.join(&o.id)).filter(|d| d.is_dir()) {
            stats.absorb(mirror(&side, &dst.join(&o.id))?);
        }
    }
    // `memory/` hangs off the slug, not off any session, so under a collision it belongs to nobody in
    // particular and cannot be attributed. Carry it only when we have the slug dir to ourselves.
    //
    // ALWAYS partitioned, unlike a session: sessions are UUID-named and disjoint, so a flat store
    // merely piles two repos' transcripts together, but `memory/` is one fixed name PER CHECKOUT (§6).
    // Flat, two repos sharing a store overwrite each other's memory on every snap — the ping-pong §6
    // names — and that is a live loss, not the cosmetic double-count the in-place rule exists to avoid.
    // A legacy flat `memory/` is left where it lies: nothing reads it, so it goes inert rather than stale.
    if rt == "claude-code" && !r.sessions.is_empty() {
        let mem = source_dir("claude-code", env)?.join("memory");
        if mem.is_dir() && claude_code::slug_dir_is_exclusive(env) {
            stats.absorb(mirror(&mem, &part.join("memory"))?);
            if !used.contains(&part) {
                used.push(part.clone());
            }
        }
    }
    // Scan every directory this pass wrote into, not just the one it reports: a store mid-migration
    // takes both, and a secret is no less pushed for having landed in the older half.
    let mut hits = 0;
    for d in &used {
        hits += crate::scan::scan_tree(d)?;
    }
    Ok((stats, hits, used.first().cloned().unwrap_or(part)))
}

/// What the store records about a captured session (§6). It exists so the facts `start` needs survive a
/// clone: git carries content, but not mtimes, and not the launch record, which is machine-local.
///
/// This is also where provenance is written (§ provenance): the sidecar is COMMITTED beside the session,
/// so the signature travels with the transcript to a teammate, unlike the machine-local launch record.
/// The signature binds `(sha256(transcript) ‖ aid ‖ committer email ‖ started)` to this machine's key.
fn write_sidecar(dp: &Path, o: &Owned, env: &Path, rt: &str, store: &Path, lineage: &Lineage) -> Result<()> {
    let mut v = serde_json::json!({
        "env": env.display().to_string(),
        "runtime": rt,
        // NEVER null. The sidecar is what a teammate ORDERS by (`latest_session`), and a clone erases
        // filesystem mtimes — so a null here forced the reader to fall back to the machine's own mtime,
        // and two teammates with a byte-identical store then resolved a DIFFERENT latest. Record the
        // transcript's own last activity, or, when it carries no timestamp at all, a stable committed
        // floor (never `now`, which would churn a fresh commit on every watch tick and still differ per
        // machine). Either value travels with the store, so everyone orders the same way.
        "last_activity": last_activity(&o.src).unwrap_or_else(|| NO_ACTIVITY_FLOOR.to_string()),
        // The divergence DAG, recorded as content-derived metadata (§ storage: lineage, not git branches).
        // A root/absent session serializes with a null parent and an empty subagents list, and an old
        // sidecar that predates this field renders as a root - there is no migration (we have no users).
        "lineage": lineage.to_json(),
    });
    if let Some(a) = &o.by {
        v["aid"] = a.aid().into();
        v["name"] = a.name().into();
        // A guess must stay legible as a guess to whoever clones this store later.
        v["attributed_by"] = match a {
            Attribution::Launched(_) => "launch-record",
            Attribution::RepoDefault { .. } => "repo-default",
        }
        .into();
        // Sign the captured transcript. Best-effort by design: no attributable aid, no readable
        // transcript, or no usable machine key each leave the sidecar unsigned — capture must never fail
        // because signing could not run, and verification degrades to "unverified" rather than blocking.
        if let Some(p) = sign_captured(dp, a, store) {
            v["provenance"] = serde_json::to_value(&p).unwrap_or(serde_json::Value::Null);
        }
    }
    let body = format!("{}\n", serde_json::to_string_pretty(&v)?);
    let p = crate::commands::sidecar_path(dp);
    if std::fs::read_to_string(&p).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(&p, body)?;
    }
    Ok(())
}

/// Sign a just-captured transcript, or `None` when anything needed is missing (unreadable transcript, no
/// machine key). Deterministic: an unchanged transcript signs to the same record, so it never churns the
/// sidecar into a fresh commit on every watch tick.
fn sign_captured(dp: &Path, a: &Attribution, store: &Path) -> Option<crate::commands::Provenance> {
    let content = std::fs::read_to_string(dp).ok()?;
    let key = crate::agent::machine_signing_key().ok()?;
    let email = crate::commands::committer_email(store);
    // A launch record fixes when the session began; a fallback-attributed session has none, so its own
    // last activity stands in. Either way the value is recorded and read back at verify, so it is
    // internally consistent regardless of which was used.
    let started = match a {
        Attribution::Launched(l) => l.started.clone(),
        Attribution::RepoDefault { .. } => last_activity(dp).unwrap_or_default(),
    };
    Some(crate::commands::sign_provenance(&key, &content, a.aid(), &email, &started))
}

/// The committed `last_activity` floor for a transcript that records no timestamp of its own. Stable and
/// content-independent, so it never churns the sidecar into a fresh commit, and committed like any other
/// sidecar value, so every teammate reads the same one — unlike the filesystem mtime it replaces. A
/// timestamp-less session therefore sorts below any real one, and ties among such sessions are broken by
/// their committed id, never by the machine's clock. RFC3339 so it parses like a real recency.
pub(crate) const NO_ACTIVITY_FLOOR: &str = "1970-01-01T00:00:00Z";

/// When a transcript last did anything: the last record carrying a timestamp. Both runtimes write a
/// top-level RFC3339 `timestamp` on every record (verified against real dumps from each).
pub(crate) fn last_activity(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut last = None;
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue; // leading records may be queue-operations / non-JSON
        };
        if let Some(t) = rec.get("timestamp").and_then(|v| v.as_str()).filter(|t| !t.is_empty()) {
            last = Some(t.to_string());
        }
    }
    last
}

// ─────────────────────── lineage: the divergence DAG as sidecar metadata ───────────────────────
//
// Subagents, runtime `/branch` siblings, and forks are one operation - agent memory diverges, then
// rejoins - seen at three scopes (design: "A coherent model for subagents, branches, and forks"). This
// wave RECORDS that structure as metadata on each session's committed sidecar; it never moves a
// transcript and never creates a git branch in the store. Two kinds are detected, both grounded in real
// transcript fields confirmed against dumps under ~/.claude/projects:
//
//   * SUBAGENT - Claude Code marks a spawned sub-thread's records `"isSidechain":true`. Each lives under
//     `<id>/subagents/<agent>.jsonl` with a companion `<agent>.meta.json` carrying `toolUseId`, the id of
//     the main-thread `Task` tool_use that spawned it. We record `{ spawn, leaf }`: the main-thread turn
//     that issued that Task, and the sub-thread's last record. (Older inline sidechains are handled too.)
//     Codex has no structural sub-thread marker (its records are session_meta/response_item/event_msg/…),
//     so nothing is fabricated for it.
//   * /branch SIBLING - a session that shares an opening prefix with another session of the same
//     agent+env and then diverges. Detected `leaf` (a record's `leafUuid` resolving into another captured
//     session) when that exact link is present, else by shared prefix (reusing `sync::shared_prefix_len`).

/// One recorded subagent branch-point: the main-thread turn that spawned it (when known), and the
/// sub-thread's leaf. `spawn` is `None` for an in-process teammate whose meta carries no `toolUseId`:
/// the subagent still exists and is counted, its exact spawn turn is just not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    pub spawn: Option<String>,
    pub leaf: String,
}

/// A session's place in the divergence DAG. A root has `parent: None` and no subagents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lineage {
    /// The session this one branched from (a sibling of the same agent+env), or `None` for a root.
    pub parent: Option<String>,
    /// The uuid both sessions shared last - the point the branch forked at.
    pub branch_point: Option<String>,
    /// How the parent link was found: `"leaf"` (exact leafUuid link) or `"prefix"` (shared prefix).
    pub detected_by: Option<String>,
    /// Subagent branch-points this session spawned (empty when it spawned none).
    pub subagents: Vec<Subagent>,
}

impl Lineage {
    /// The committed sidecar shape. Nulls and an empty list are written explicitly so a reader never has
    /// to distinguish "root" from "field absent" - both render as a root.
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "parent": self.parent,
            "branch_point": self.branch_point,
            "detected_by": self.detected_by,
            "subagents": self
                .subagents
                .iter()
                .map(|s| serde_json::json!({ "spawn": s.spawn, "leaf": s.leaf }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Read a captured session's committed lineage from its sidecar. A missing sidecar, missing `lineage`
/// object, or a pre-lineage sidecar all read as a clean root - no migration, by design.
pub fn read_lineage(transcript: &Path) -> Lineage {
    let Ok(text) = std::fs::read_to_string(crate::commands::sidecar_path(transcript)) else {
        return Lineage::default();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Lineage::default();
    };
    let Some(l) = v.get("lineage") else { return Lineage::default() };
    let str_field = |k: &str| l.get(k).and_then(|x| x.as_str()).map(String::from);
    let subagents = l
        .get("subagents")
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    Some(Subagent {
                        spawn: s.get("spawn").and_then(|x| x.as_str()).map(String::from),
                        leaf: s.get("leaf")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Lineage { parent: str_field("parent"), branch_point: str_field("branch_point"), detected_by: str_field("detected_by"), subagents }
}

/// Detect a captured session's lineage: its subagent branch-points, and whether it branched from a
/// sibling. `siblings` are the other sessions of the same agent+env (transcript path + id).
pub fn detect_lineage(rt: &str, src: &Path, id: &str, siblings: &[(PathBuf, String)]) -> Lineage {
    let text = std::fs::read_to_string(src).unwrap_or_default();
    let subagents = detect_subagents(&text, src, id);
    let (parent, branch_point, detected_by) = match detect_branch(rt, &text, id, siblings) {
        Some((p, b, d)) => (Some(p), Some(b), Some(d)),
        None => (None, None, None),
    };
    Lineage { parent, branch_point, detected_by, subagents }
}

/// The uuid of the last record in a transcript file (the sub-thread's leaf), or `None`.
fn last_record_uuid(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut last = None;
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        if let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(u) = rec.get("uuid").and_then(|v| v.as_str()) {
                last = Some(u.to_string());
            }
        }
    }
    last
}

/// Find this session's subagent branch-points. Two storage layouts, both real:
///   * sidechains stored under `<id>/subagents/<agent>.jsonl` (+ `<agent>.meta.json{toolUseId}`) - the
///     current Claude Code shape; the meta's `toolUseId` names the spawning main-thread `Task`.
///   * sidechains inline in the transcript (`isSidechain:true` runs) - older shape; the spawn is the
///     nearest preceding main-thread `Task` turn.
fn detect_subagents(text: &str, src: &Path, id: &str) -> Vec<Subagent> {
    // Map each main-thread Task tool_use id → the uuid of the assistant turn that issued it, and track
    // the most recent such turn so an inline sidechain run can be attributed to the Task before it.
    let mut task_spawn: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut last_task_turn: Option<String> = None;
    let mut out: Vec<Subagent> = Vec::new();
    // Open inline sidechain run, if any: (spawn turn at the point it opened, running leaf uuid).
    let mut open_spawn: Option<String> = None;
    let mut open_leaf: Option<String> = None;

    let close_inline = |out: &mut Vec<Subagent>, spawn: &mut Option<String>, leaf: &mut Option<String>| {
        if let (Some(sp), Some(lf)) = (spawn.take(), leaf.take()) {
            out.push(Subagent { spawn: Some(sp), leaf: lf });
        } else {
            *spawn = None;
            *leaf = None;
        }
    };

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let uuid = rec.get("uuid").and_then(|v| v.as_str()).map(String::from);
        if rec.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false) {
            if open_leaf.is_none() && open_spawn.is_none() {
                open_spawn = last_task_turn.clone();
            }
            if let Some(u) = uuid {
                open_leaf = Some(u);
            }
            continue;
        }
        // A main-thread record closes any open inline sidechain run.
        close_inline(&mut out, &mut open_spawn, &mut open_leaf);
        if rec.get("type").and_then(|v| v.as_str()) == Some("assistant") {
            if let Some(blocks) = rec.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                        && b.get("name").and_then(|v| v.as_str()) == Some("Task")
                    {
                        if let (Some(tid), Some(u)) = (b.get("id").and_then(|v| v.as_str()), &uuid) {
                            task_spawn.insert(tid.to_string(), u.clone());
                            last_task_turn = Some(u.clone());
                        }
                    }
                }
            }
        }
    }
    close_inline(&mut out, &mut open_spawn, &mut open_leaf);

    // Sub-thread files stored alongside the transcript, each with a `.meta.json` naming its spawning Task.
    if let Some(dir) = src.parent().map(|d| d.join(id).join("subagents")).filter(|d| d.is_dir()) {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                .collect();
            files.sort();
            for f in files {
                let Some(leaf) = last_record_uuid(&f) else { continue };
                let tool_use_id = std::fs::read_to_string(f.with_extension("meta.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                    .and_then(|v| v.get("toolUseId").and_then(|x| x.as_str()).map(String::from));
                // Resolve the Task tool_use id to the main-thread turn that issued it; when the transcript
                // no longer holds that turn, the tool_use id itself still names the spawn point. An
                // in-process teammate carries no `toolUseId`, so `spawn` stays None: the subagent is still
                // recorded and counted (leaf is known), its exact spawn turn is simply unknown.
                let spawn = tool_use_id.as_ref().and_then(|t| task_spawn.get(t).cloned()).or(tool_use_id);
                out.push(Subagent { spawn, leaf });
            }
        }
    }

    out.sort_by(|a, b| (a.spawn.as_deref(), a.leaf.as_str()).cmp(&(b.spawn.as_deref(), b.leaf.as_str())));
    out.dedup();
    out
}

/// Detect whether this session branched from a sibling: the exact `leafUuid` link first, then the
/// shared-prefix fallback. Returns `(parent_id, branch_point_uuid, detected_by)`.
fn detect_branch(rt: &str, text: &str, id: &str, siblings: &[(PathBuf, String)]) -> Option<(String, String, String)> {
    detect_branch_leaf(text, siblings).or_else(|| detect_branch_prefix(rt, text, id, siblings))
}

/// EXACT: a record in this session carries a `leafUuid` that resolves to a uuid living in ANOTHER
/// captured session (and not in this one) - that other session is the parent, the branch-point is that
/// uuid. Confirmed field name in real dumps (`leafUuid`), though real dumps only ever carry it as a
/// same-session bookmark, which this correctly ignores.
fn detect_branch_leaf(text: &str, siblings: &[(PathBuf, String)]) -> Option<(String, String, String)> {
    let mut own: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut leaves: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
        if let Some(u) = rec.get("uuid").and_then(|v| v.as_str()) {
            own.insert(u.to_string());
        }
        if let Some(l) = rec.get("leafUuid").and_then(|v| v.as_str()) {
            leaves.push(l.to_string());
        }
    }
    for leaf in leaves {
        if own.contains(&leaf) {
            continue; // a same-session bookmark, not a branch link
        }
        for (sp, sid) in siblings {
            if session_contains_uuid(sp, &leaf) {
                return Some((sid.clone(), leaf, "leaf".to_string()));
            }
        }
    }
    None
}

/// Whether a transcript file contains a record with this uuid.
fn session_contains_uuid(path: &Path, uuid: &str) -> bool {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else { return false };
    for line in std::io::BufReader::new(f).lines() {
        let Ok(line) = line else { break };
        if let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) {
            if rec.get("uuid").and_then(|v| v.as_str()) == Some(uuid) {
                return true;
            }
        }
    }
    false
}

/// FALLBACK: shared-prefix. Reusing `sync::shared_prefix_len`, find the sibling whose opening record
/// chain matches this session's longest before diverging - that sibling is the parent and the last
/// shared uuid is the branch-point. Guards against false positives: the overlap must contain a real user
/// prompt (unrelated sessions differ at their first prompt, so a boilerplate-only overlap is not a
/// branch), and the two must actually diverge. The parent is the EARLIER session (by start timestamp,
/// then id) so the edge is asymmetric and the DAG cannot cycle.
fn detect_branch_prefix(rt: &str, text: &str, id: &str, siblings: &[(PathBuf, String)]) -> Option<(String, String, String)> {
    let this_ir = crate::convo::read_conversation(rt, text).ok()?;
    let this_end = last_timestamp(&this_ir);
    let mut best: Option<(usize, String, String)> = None;
    for (sp, sid) in siblings {
        let stext = std::fs::read_to_string(sp).unwrap_or_default();
        let Ok(sib_ir) = crate::convo::read_conversation(rt, &stext) else { continue };
        let n = crate::sync::shared_prefix_len(&this_ir, &sib_ir);
        if n == 0 {
            continue;
        }
        // Must actually diverge: a full match on both sides is a copy, not a branch we can point at.
        if n >= this_ir.events.len() && n >= sib_ir.events.len() {
            continue;
        }
        if !prefix_has_prompt(&this_ir, n) {
            continue; // boilerplate-only overlap (mode/permission records) - not a branch
        }
        // The parent is the earlier session; if the sibling is the later one, it branched from us. "Later"
        // is judged by most-recent activity (the recency the store already orders sessions by), since a
        // branch and its parent share their opening record and therefore its start timestamp.
        if !sibling_is_parent(&last_timestamp(&sib_ir), &this_end, sid, id) {
            continue;
        }
        let bp = this_ir
            .events
            .get(n - 1)
            .and_then(|e| e.id.clone())
            .or_else(|| sib_ir.events.get(n - 1).and_then(|e| e.id.clone()))?;
        // Longest shared prefix wins; a tie is broken by the smaller sibling id, so the parent a session
        // resolves to is deterministic and does not depend on sibling walk order.
        let better = match best.as_ref() {
            None => true,
            Some((bn, best_id, _)) => n > *bn || (n == *bn && sid < best_id),
        };
        if better {
            best = Some((n, sid.clone(), bp));
        }
    }
    best.map(|(_, p, b)| (p, b, "prefix".to_string()))
}

/// The last record timestamp in a conversation (its most recent activity), if any.
fn last_timestamp(ir: &crate::convo::ConversationIR) -> Option<String> {
    ir.events.iter().rev().find_map(|e| e.timestamp.clone())
}

/// Whether the sibling is the parent (the earlier session): earlier last-activity wins; a tie or
/// missing timestamps fall back to the smaller id, so exactly one of any pair is the parent.
fn sibling_is_parent(sib_end: &Option<String>, this_end: &Option<String>, sib_id: &str, this_id: &str) -> bool {
    match (parse_rfc3339(sib_end), parse_rfc3339(this_end)) {
        (Some(a), Some(b)) if a != b => a < b,
        _ => sib_id < this_id,
    }
}

fn parse_rfc3339(s: &Option<String>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s.as_deref()?.trim()).ok()
}

/// Whether the first `n` records of a conversation contain any real user prompt.
fn prefix_has_prompt(ir: &crate::convo::ConversationIR, n: usize) -> bool {
    ir.events
        .iter()
        .take(n)
        .any(|e| e.kinds.iter().any(|k| matches!(k, crate::convo::EventKind::UserPrompt(_))))
}

/// The store the convert worker acts on when capture has not routed anything yet: the agent this repo
/// resolves to, else the legacy nested store. Converting is not attribution — it makes THIS agent's
/// sessions resumable in either CLI — so the resolved agent is the right answer here.
pub fn convert_target() -> Result<PathBuf> {
    match crate::agent::resolve(None) {
        Ok(a) => Ok(a.store),
        Err(_) => scope::root_for(Scope::Agent),
    }
}

/// `agit -a snap --watch [--interval N]` — **fully automatic snap**: watch the runtime's session dump and,
/// whenever it changes and then settles, mirror + auto-commit into the Agent Store. Runs until Ctrl-C.
/// Runtime-agnostic; the pre-commit secret hook still applies (a snap carrying a secret is refused, with a warning).
/// `snap --watch --from <rt>`: validate first. An unknown name here is the worst case — the loop runs
/// forever polling a dump that cannot exist, reporting nothing, looking healthy.
pub fn snap_watch_checked(runtime: &str, interval_secs: u64, capture_harness: bool) -> Result<i32> {
    let rt = resolve_runtime(Some(runtime), &[], "watch")?;
    snap_watch(&rt, interval_secs, capture_harness)
}

pub fn snap_watch(runtime: &str, interval_secs: u64, capture_harness: bool) -> Result<i32> {
    let env = scope::env_root()?;
    warn_stranded(&env);
    let rt = normalize(runtime);
    let interval = std::time::Duration::from_secs(interval_secs.max(1));
    let watch = source_path(&rt, &env);

    outln!("Auto-snapping {rt} on every change (settling window {interval_secs}s). Ctrl-C to stop.");
    if watch.as_deref().map(|p| !p.exists()).unwrap_or(true) {
        outln!("  (waiting for {rt} sessions to appear…)");
    }

    let mut last_sig = String::new();
    let mut pending = false;
    let mut count: u64 = 0;
    let mut announced = std::collections::HashSet::new();
    loop {
        let sig = watch.as_deref().map(dir_signature).unwrap_or_default();
        if sig != last_sig {
            // changed since last check → wait one more tick for it to settle (debounce a burst of edits)
            pending = true;
            last_sig = sig;
        } else if pending {
            // --no-harness must mean the same thing here as it does for `--watch` with no --from: one
            // documented flag cannot have two behaviours decided by another flag.
            match capture_pass(&rt, &env, capture_harness, &mut count) {
                Ok(touched) => announce_watching(&touched, &env, &mut announced),
                Err(e) => errln!("  snap failed: {e:#}"),
            }
            pending = false;
        }
        std::thread::sleep(interval);
    }
}

/// One settled capture: mirror + commit into every store that has sessions here (§6). Returns the
/// stores it touched, so the convert worker keeps up with agents that appear while it runs.
fn capture_pass(rt: &str, env: &Path, capture_harness: bool, count: &mut u64) -> Result<Vec<PathBuf>> {
    let (routed, _) = route(rt, env)?;
    for r in &routed {
        // mirror → add → commit is one read-modify-write of the store's index and HEAD. Two repos
        // watching one shared agent run this loop at the same time by design, so it is taken as a
        // unit: git's own index.lock does not span it, and a lost tick is not worth a corrupt store.
        let lock = match lock_store(&r.store) {
            Ok(l) => l,
            // Never fatal: the next tick tries again, and a watcher that exits on contention is a
            // watcher that silently stops capturing.
            Err(e) => {
                errln!("  snap {rt} skipped this tick: {e:#}");
                continue;
            }
        };
        match mirror_owned(rt, env, r) {
            Ok((stats, _hits, _)) if stats.added + stats.updated > 0 => {
                if capture_harness {
                    let _ = crate::harness::capture(&r.store, env, rt);
                }
                commit_snap(&r.store, rt, count);
            }
            Ok(_) => {}
            Err(e) => errln!("  snap {rt} failed: {e:#}"),
        }
        drop(lock);
    }
    Ok(routed.into_iter().map(|r| r.store).collect())
}

/// Stage + commit the mirrored dump. Nothing staged → no-op.
///
/// The gate runs HERE, on the staged index, before the commit — not on git's pre-commit hook, which
/// `--no-verify` skips. Auto-snap owns this commit, so a secret in a transcript the agent just saw is
/// caught even on the fully hands-off `watch` path. Blocked → mirrored to disk but never committed;
/// the visible `AGIT_ALLOW_SECRETS` override lets it through and is disclosed by the gate.
/// Returns `true` when staged content was NOT committed because a problem held it back (the secret gate
/// blocked it, the gate errored, or git refused) — the caller turns that into a non-zero exit so a
/// `snap && push` chain (and `set -e`) never proceeds as if the snap landed. `false` means committed, or
/// nothing to commit (a clean no-op). Either way the mirror on disk is left untouched.
fn commit_snap(agent: &Path, rt: &str, count: &mut u64) -> bool {
    let ts = now_iso();
    match gated_commit(agent, &format!("auto-snap {rt} {ts}"), "snap") {
        CommitOutcome::Committed => {
            *count += 1;
            outln!("  {} snapped {ts}  (#{count})", ui::accent("●"));
            false
        }
        CommitOutcome::NothingStaged => false,
        CommitOutcome::Blocked => true,
    }
}

/// The outcome of the one gated-commit primitive.
enum CommitOutcome {
    /// The staged index was committed.
    Committed,
    /// Nothing was staged — a clean no-op (never an empty commit).
    NothingStaged,
    /// Staged content was held OUT of history: the secret gate blocked it, the gate errored, or git
    /// refused. The mirror on disk is left untouched; the index is unstaged so a loop does not spin on it.
    Blocked,
}

/// Stage everything in the store, gate the staged index for secrets, and commit it with `subject`.
///
/// The SINGLE gated-commit primitive: `agit a snap` (via `commit_snap`) and the `agit a merge`
/// merged-session capture (via `commit_merged_session`) both go through it, so the secret gate is
/// byte-for-byte identical on both paths — the same `secret_gate` over the same staged index. `verb`
/// only names the action in the gate's own disclosure line; the DETECTION is the same regardless. On a
/// block or a refusal it warns, unstages (so a watch loop does not spin on it), and leaves the mirror
/// on disk. The wrapper already gated the index, so it skips git's now-redundant pre-commit hook (the
/// hook stays installed as the defense for a raw `git commit` that never went through agit).
fn gated_commit(store: &Path, subject: &str, verb: &str) -> CommitOutcome {
    let _ = scope::git_in_status(store, &["add", "-A"]);
    // `diff --cached --quiet` exits 1 when something is staged, 0 when nothing is.
    if scope::git_in_status(store, &["diff", "--cached", "--quiet"]).0 == 0 {
        return CommitOutcome::NothingStaged;
    }

    // The committer email is the session's provenance identity (the handle that bridges its signature to
    // a hub account). A session captured under an unset identity could never be attributed to anyone, so
    // refuse git-style BEFORE committing: no commit, no staged leftovers. agit's own bookkeeping commits
    // carry an explicit `-c user.email=agit@local`, so they are unaffected — only real session captures
    // pass through this gate.
    if crate::commands::committer_identity_unset(store) {
        crate::commands::warn_committer_identity_unset();
        let _ = scope::git_in_status(store, &["reset", "-q"]); // unstage: no committed, no staged leftovers
        return CommitOutcome::Blocked;
    }

    match crate::commands::secret_gate(store, true, verb) {
        Ok(g) if g.allowed() => {}
        Ok(_) => {
            errln!(
                "{}",
                ui::warn(&format!(
                    "  ⚠ not committed: suspected secrets (mirrored, kept out of history). agit a scan to see; {}=1 to override",
                    crate::commands::ALLOW_ENV
                ))
            );
            let _ = scope::git_in_status(store, &["reset", "-q"]); // unstage so we don't spin on it
            return CommitOutcome::Blocked;
        }
        Err(e) => {
            errln!("{}", ui::warn(&format!("  ⚠ not committed: secret gate failed to run ({e:#}). agit a scan to see")));
            let _ = scope::git_in_status(store, &["reset", "-q"]);
            return CommitOutcome::Blocked;
        }
    }

    let (rc, _) = scope::git_in_status(store, &["commit", "--no-verify", "-m", subject]);
    if rc == 0 {
        CommitOutcome::Committed
    } else {
        errln!("{}", ui::warn("  ⚠ not committed: git commit refused it. agit a scan to see"));
        let _ = scope::git_in_status(store, &["reset", "-q"]); // unstage so we don't spin on it
        CommitOutcome::Blocked
    }
}

/// Capture exactly ONE reconciled merged session into the store partition
/// `sessions/<env-slug>/<rt>/<id>.jsonl` and commit it through the SAME gate `snap` uses, so an
/// `agit a merge` produces a commit (git-style): the merged session becomes the agent's latest, so
/// `latest_session` resolves to it and `agit a log` shows it newest.
///
/// It writes EXACTLY the one session, never a blanket snap of the runtime. The dialogue merge revives
/// temporary A/B copies into the runtime dir that must never enter the store, so the caller hands us the
/// single merged transcript and nothing else from `sessions/` is staged.
///
/// Returns whether the commit was BLOCKED by the secret gate — a merged transcript carrying a secret is
/// mirrored to disk but kept out of history, exactly as snap does — so the caller can reflect the block
/// in the merge's exit code. A fresh sidecar records `last_activity = now`, so this merge sorts ahead of
/// every prior session even when the transcript itself carries no timestamp of its own.
pub fn commit_merged_session(store: &Path, env: &Path, rt: &str, id: &str, content: &str) -> Result<bool> {
    let rt = normalize(rt);
    // Held across the write + the read-modify-write commit, exactly like snap: one store is shared by
    // every repo that tracks the agent, so a concurrent snap/merge must not race this index and HEAD.
    let _lock = lock_store(store)?;
    let part = store.join(SESSIONS_SUBDIR).join(env_slug(env)).join(&rt);
    std::fs::create_dir_all(&part)?;
    let dst = part.join(format!("{id}.jsonl"));
    std::fs::write(&dst, content)?;
    // The sidecar carries the recency `latest_session` orders by. A merge transcript may have no usable
    // timestamp of its own, so stamp `now` here to make it unambiguously the newest session.
    let sidecar = serde_json::json!({
        "env": env.display().to_string(),
        "runtime": rt,
        "last_activity": now_iso(),
        "source": "merge",
    });
    std::fs::write(
        crate::commands::sidecar_path(&dst),
        format!("{}\n", serde_json::to_string_pretty(&sidecar)?),
    )?;

    let ts = now_iso();
    match gated_commit(store, &format!("merge {rt} {ts}"), "merge") {
        CommitOutcome::Committed => {
            outln!("  {} merged session captured into the store", ui::accent("●"));
            Ok(false)
        }
        CommitOutcome::NothingStaged => Ok(false),
        CommitOutcome::Blocked => Ok(true),
    }
}

/// Where a runtime's session dump for this project lives (no existence check — the watcher waits for it).
fn source_path(rt: &str, env: &Path) -> Option<PathBuf> {
    crate::adapter::get(rt).ok().and_then(|a| a.watch_dir(env))
}

/// A cheap change signature of a directory tree: sorted (path, size, mtime) of every file.
fn dir_signature(dir: &Path) -> String {
    let mut parts: Vec<String> = walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            let mt = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
            Some(format!("{}:{}:{mt}", e.path().display(), m.len()))
        })
        .collect();
    parts.sort();
    parts.join("|")
}

fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Canonical runtime name, delegating to the one shared alias map (`adapter::normalize`). Unknown
/// names pass through unchanged, preserving the prior behavior.
fn normalize(runtime: &str) -> String {
    crate::adapter::normalize(runtime).map(str::to_string).unwrap_or_else(|| runtime.to_string())
}

#[derive(Default)]
struct Stats {
    total: usize,
    added: usize,
    updated: usize,
    bytes: u64,
}

impl Stats {
    fn absorb(&mut self, o: Stats) {
        self.total += o.total;
        self.added += o.added;
        self.updated += o.updated;
        self.bytes += o.bytes;
    }
}

/// Copy one file if the destination is missing, a different size, or older; report whether it copied.
/// Missing mtimes are treated as "re-copy", which is the conservative direction.
fn copy_if_changed(src: &Path, dp: &Path, st: &mut Stats) -> Result<bool> {
    if let Some(parent) = dp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let smeta = std::fs::metadata(src)?;
    let mut copied = true;
    match std::fs::metadata(dp) {
        Err(_) => {
            std::fs::copy(src, dp)?;
            st.added += 1;
        }
        Ok(dmeta) => {
            let newer = match (smeta.modified(), dmeta.modified()) {
                (Ok(s), Ok(d)) => s > d,
                _ => true,
            };
            if dmeta.len() != smeta.len() || newer {
                std::fs::copy(src, dp)?;
                st.updated += 1;
            } else {
                copied = false;
            }
        }
    }
    st.total += 1;
    st.bytes += smeta.len();
    Ok(copied)
}

/// Recursively mirror src → dst (decide whether to overwrite by size + mtime only, which is good enough).
fn mirror(src: &Path, dst: &Path) -> Result<Stats> {
    let mut st = Stats { total: 0, added: 0, updated: 0, bytes: 0 };
    mirror_into(src, dst, &mut st)?;
    Ok(st)
}

fn mirror_into(src: &Path, dst: &Path, st: &mut Stats) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            mirror_into(&sp, &dp, st)?;
        } else {
            let smeta = entry.metadata()?;
            match std::fs::metadata(&dp) {
                Err(_) => {
                    std::fs::copy(&sp, &dp)?;
                    st.added += 1;
                }
                Ok(dmeta) => {
                    // Re-copy if size **or** mtime changed. Checking size alone would miss same-length in-place edits
                    // (and would contradict this function's "size + mtime" comment); when mtime is unavailable, re-copy conservatively.
                    let newer = match (smeta.modified(), dmeta.modified()) {
                        (Ok(s), Ok(d)) => s > d,
                        _ => true,
                    };
                    if dmeta.len() != smeta.len() || newer {
                        std::fs::copy(&sp, &dp)?;
                        st.updated += 1;
                    }
                }
            }
            st.total += 1;
            st.bytes += smeta.len();
        }
    }
    Ok(())
}

// ── unified watcher: watch BOTH runtimes' live dumps, auto-snap, auto-convert both ways ──

/// `agit watch` — the fully hands-off loop. Watches both runtimes' live session dumps directly, and on
/// each settle: auto-snaps (mirror + commit, harness included) and (unless --no-convert) auto-converts
/// every session both ways so it's immediately resumable in either CLI. Foreground; Ctrl-C to stop.
pub fn watch(interval_secs: u64, do_convert: bool, capture_harness: bool) -> Result<i32> {
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;
    let env = scope::env_root()?;
    warn_stranded(&env);
    let interval = Duration::from_secs(interval_secs.max(1));
    let runtimes = runtimes();
    let mut last: HashMap<&str, String> = HashMap::new();
    let mut pending: HashMap<&str, bool> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut count = 0u64;
    // The stores to auto-convert: what this repo resolves to now, plus every store a capture routes
    // into. One agent's sessions must not stop converting because another agent also worked here.
    let mut stores: Vec<PathBuf> = convert_target().into_iter().collect();
    let mut announced = std::collections::HashSet::new();
    announce_watching(&stores, &env, &mut announced);
    outln!(
        "Watching {} every {}s: auto-snap{}. Ctrl-C to stop.",
        runtime_list(),
        interval.as_secs(),
        if do_convert { " + auto-convert both ways" } else { "" }
    );
    loop {
        for &rt in &runtimes {
            let sig = source_path(rt, &env).map(|p| dir_signature(&p)).unwrap_or_default();
            // first sight of a runtime counts as "changed" so pre-existing sessions get captured on start
            let changed = last.get(rt).map(|l| l != &sig).unwrap_or(true);
            if changed {
                last.insert(rt, sig);
                pending.insert(rt, true);
            } else if pending.get(rt).copied().unwrap_or(false) {
                pending.insert(rt, false);
                match capture_pass(rt, &env, capture_harness, &mut count) {
                    Ok(touched) => {
                        announce_watching(&touched, &env, &mut announced);
                        for s in touched {
                            if !stores.contains(&s) {
                                stores.push(s);
                            }
                        }
                    }
                    Err(e) => errln!("  snap {rt} failed: {e:#}"),
                }
            }
        }
        if do_convert {
            for s in &stores {
                crate::commands::convert_pass(s, &env, &mut seen);
            }
        }
        std::thread::sleep(interval);
    }
}

/// The watcher's pid and log live with the ENVIRONMENT, because that is what a watcher watches: one
/// repo's session dump, routed afterwards to whichever agents worked there (§6).
///
/// They used to live in the store's `.git`, which made them one file per STORE. A shared store is now
/// the normal case, so two repos watching one agent fought over one pidfile: the second `agit watch`
/// either refused as "already running" or overwrote the first's pid and orphaned a live daemon.
/// `agit init` gitignores `.agit/`, so nothing here is ever tracked or scanned.
fn watch_rundir(env: &Path) -> Result<PathBuf> {
    let dir = env.join(".agit");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

/// One watcher, as it announces itself: `{aid, env, pid, started}`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Watcher {
    /// Identity, not the label — a rename must not orphan the record.
    pub aid: String,
    pub env: String,
    pub pid: u32,
    pub started: String,
}

/// `$AGIT_HOME/watchers.jsonl` — machine-local, spans repos, append-only and scanned, exactly like
/// `launches.jsonl` and for the same reason.
///
/// The pidfile answers "is this environment being watched?". `agit a list` asks a different question —
/// "is anyone watching THIS AGENT, from anywhere?" — which no single environment can answer now that
/// one store is shared by many. A watcher therefore announces each agent it captures for, here.
fn watchers_file(home: &Path) -> PathBuf {
    home.join("watchers.jsonl")
}

fn record_watcher_at(home: &Path, w: &Watcher) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(home)?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(watchers_file(home))?;
    writeln!(f, "{}", serde_json::to_string(w)?)?;
    Ok(())
}

fn watching_pid_at(home: &Path, aid: &str) -> Option<u32> {
    let text = std::fs::read_to_string(watchers_file(home)).ok()?;
    text.lines()
        .filter_map(|l| serde_json::from_str::<Watcher>(l.trim()).ok())
        .filter(|w| w.aid == aid)
        .map(|w| w.pid)
        .find(|p| pid_alive(*p))
}

/// The live watcher capturing for this agent, from any environment — what `agit a list` prints as
/// `● watching`.
///
/// A record whose process is gone reads as not-watching, never as stale-and-believed: nothing prunes
/// this log, and a killed daemon never gets to retract its own record.
pub fn watching_pid(aid: &str) -> Option<u32> {
    watching_pid_at(&scope::agit_home().ok()?, aid)
}

/// Announce this process as the watcher of every agent it captures for. The set is not known at
/// startup — capture discovers agents as they work here — so it is announced as it grows, and each aid
/// only once per process.
fn announce_watching(stores: &[PathBuf], env: &Path, announced: &mut std::collections::HashSet<String>) {
    for s in stores {
        let Ok(id) = crate::agent::read_identity(s) else { continue };
        if !announced.insert(id.aid.clone()) {
            continue;
        }
        let w = Watcher {
            aid: id.aid,
            env: env.display().to_string(),
            pid: std::process::id(),
            started: now_iso(),
        };
        if let Err(e) = scope::agit_home().and_then(|h| record_watcher_at(&h, &w)) {
            errln!("{}", ui::warn(&format!("  ⚠ watcher not announced ({e:#}): agit a list will not show it as watched")));
        }
    }
}

fn read_pid(p: &Path) -> Option<u32> {
    std::fs::read_to_string(p).ok().and_then(|s| s.trim().parse().ok())
}

/// `kill -0 <pid>`: asks the kernel whether the process exists, without delivering a signal.
///
/// 0 is refused before it reaches the kernel, where it does NOT mean "no process": `kill -0 0` signals
/// the caller's own process group and always succeeds. A truncated or zeroed pidfile would otherwise
/// read as a live holder forever — wedging the store lock, and making `agit watch` refuse to start
/// against a watcher that does not exist.
fn pid_alive(pid: u32) -> bool {
    pid != 0
        && std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// `agit watch --daemon` — spawn the watcher detached (own process group, stdio to a log inside the
/// agent repo's .git) so it keeps running after the launching shell exits.
pub fn watch_daemon(interval_secs: u64, do_convert: bool, capture_harness: bool) -> Result<i32> {
    use std::process::{Command, Stdio};
    let env = scope::env_root()?;
    let rundir = watch_rundir(&env)?;
    let logp = rundir.join("agit-watch.log");
    let pidp = rundir.join(WATCH_PID);
    if let Some(pid) = read_pid(&pidp) {
        if pid_alive(pid) {
            outln!("agit watch already running (pid {pid}). Stop it with: agit watch --stop");
            return Ok(0);
        }
    }
    let exe = std::env::current_exe().context("cannot locate the agit binary to spawn")?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&logp)?;
    let log2 = log.try_clone()?;
    let mut cmd = Command::new(&exe);
    cmd.arg("watch").arg("--interval").arg(interval_secs.to_string());
    if !do_convert {
        cmd.arg("--no-convert");
    }
    if !capture_harness {
        cmd.arg("--no-harness");
    }
    cmd.current_dir(&env) // child resolves the same repos from the project root
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log2);
    // Detach the daemon from the launching shell so it outlives it. On Unix, its own process group
    // means the shell's SIGHUP does not reach it. On Windows, DETACHED_PROCESS drops the console and
    // CREATE_NEW_PROCESS_GROUP stops Ctrl-C/close from propagating — the same intent.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    let child = cmd.spawn().context("failed to spawn the background watcher")?;
    let pid = child.id();
    std::fs::write(&pidp, pid.to_string())?;
    outln!("agit watch started in the background (pid {pid}).");
    outln!("  log:    {}", logp.display());
    outln!("  status: agit watch --status   ·   stop: agit watch --stop");
    Ok(0)
}

/// `agit watch --stop` — kill the background watcher recorded for this project.
pub fn watch_stop() -> Result<i32> {
    let pidp = watch_rundir(&scope::env_root()?)?.join(WATCH_PID);
    match read_pid(&pidp) {
        Some(pid) => {
            let killed = std::process::Command::new("kill")
                .arg(pid.to_string())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let _ = std::fs::remove_file(&pidp);
            if killed {
                outln!("Stopped agit watch (pid {pid}).");
            } else {
                outln!("No live process for pid {pid}; cleared the stale pidfile.");
            }
            Ok(0)
        }
        None => {
            outln!("No background watcher is recorded for this project.");
            Ok(0)
        }
    }
}

/// `agit watch --status` — report whether the background watcher is running.
pub fn watch_status() -> Result<i32> {
    let rundir = watch_rundir(&scope::env_root()?)?;
    match read_pid(&rundir.join(WATCH_PID)) {
        Some(pid) if pid_alive(pid) => {
            outln!("agit watch is running (pid {pid}).");
            outln!("  log: {}", rundir.join("agit-watch.log").display());
        }
        _ => outln!("agit watch is not running for this project."),
    }
    Ok(0)
}

#[cfg(test)]
pub(crate) mod testenv {
    use std::path::Path;

    /// $HOME drives the runtimes' dump dirs and $AGIT_HOME drives agit's stores. Both are
    /// process-global, so every test that needs them takes THIS lock and puts them back: a leaked
    /// $HOME points the next test at the developer's real sessions, and two locks are no lock at all.
    pub fn with(home: &Path, agit_home: &Path, f: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = [var("HOME"), var("AGIT_HOME")];
        std::env::set_var("HOME", home);
        std::env::set_var("AGIT_HOME", agit_home);
        // Give this test's ISOLATED HOME a resolvable git identity, so a store scaffolded under it
        // inherits a committer email the way a real user's machine does (the snap gate now refuses an
        // unset identity). This is the test's own throwaway `$HOME/.gitconfig`, never the developer's
        // real one — it honors the "no global git config in tests" rule while making every testenv-based
        // snap test attributable to a stable `tester@agit.test`.
        write_test_gitconfig(home);
        f();
        restore("HOME", &old[0]);
        restore("AGIT_HOME", &old[1]);
    }

    /// Write an isolated `[user]` identity into `$HOME/.gitconfig`. Global git config in this test's own
    /// temp HOME only; a store created here resolves `user.email` to it, so its snapped sessions are
    /// attributable without any store-local `agit@local`.
    fn write_test_gitconfig(home: &Path) {
        let _ = std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = tester\n\temail = tester@agit.test\n",
        );
    }

    fn var(k: &str) -> Option<String> {
        std::env::var(k).ok()
    }

    fn restore(k: &str, v: &Option<String>) {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    fn transcript(cwd: &Path, ts: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"timestamp\":\"{ts}\",\"message\":{{\"content\":\"hi\"}}}}\n",
            cwd.display()
        )
    }

    /// Acceptance criterion 4: two agents at once in ONE repo, each capturing to its OWN store,
    /// attributed by launch record.
    ///
    /// The correctness core of multi-agent. Both runtimes dump per PROJECT, so both agents' sessions
    /// land in ONE folder and the session id is the only thing that can tell them apart. Attribution
    /// by the active pointer would file both under whichever agent happened to be active — silently,
    /// and then push one team's transcript to the other's remote.
    #[test]
    fn two_agents_in_one_repo_capture_into_their_own_stores() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let api = crate::agent::init_agent("api").unwrap();

            // ONE dump folder: claude keys on the project's cwd slug, never on the agent.
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s-fe.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            std::fs::write(slug.join("s-api.jsonl"), transcript(&env, "2026-07-16T10:00:00.000Z")).unwrap();

            crate::commands::record_launch("s-fe", &fe.aid, "frontend", &env, "claude-code", None).unwrap();
            crate::commands::record_launch("s-api", &api.aid, "api", &env, "claude-code", None).unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            assert_eq!(routed.len(), 2, "one pass must write into BOTH agents' stores, not collapse to one");
            for r in &routed {
                mirror_owned("claude-code", &env, r).unwrap();
            }

            // Partitioned by environment, not flat: the store is shared by every repo the agent works
            // in, so the path itself has to say which one each session came from.
            let fe_sessions = fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code");
            let api_sessions = api.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code");
            assert!(fe_sessions.join("s-fe.jsonl").exists(), "frontend's session must land in frontend's store");
            assert!(api_sessions.join("s-api.jsonl").exists(), "api's session must land in api's store");
            assert!(!fe_sessions.join("s-api.jsonl").exists(), "MISFILED: api's transcript is in frontend's store");
            assert!(!api_sessions.join("s-fe.jsonl").exists(), "MISFILED: frontend's transcript is in api's store");
            // the store is keyed by aid, so the path on disk IS the attribution
            assert!(fe.store.ends_with(&fe.aid) && api.store.ends_with(&api.aid));

            // The sidecar carries identity and recency into the store, where a clone can still read
            // them: the launch record is machine-local and never travels.
            let side: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(fe_sessions.join("s-fe.agit.json")).unwrap()).unwrap();
            assert_eq!(side["aid"], fe.aid);
            assert_eq!(side["name"], "frontend");
            assert_eq!(side["attributed_by"], "launch-record", "a record is authoritative and must say so");
            assert_eq!(side["last_activity"], "2026-07-16T09:00:00.000Z");
            assert_eq!(side["env"], env.display().to_string());
        });
    }

    /// The convert worker installs its output as a NEW session in the project's dump folder. Without a
    /// launch record of its own that copy reads as hand-started, so capture files it under the repo's
    /// DEFAULT agent — converting `frontend`'s memory would deposit a copy of it in `api`'s store, and
    /// `agit a push` would then hand it to api's team.
    #[test]
    fn a_converted_session_is_attributed_back_to_the_agent_it_came_from() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let d = fe.store.join("sessions/claude-code");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();

            crate::commands::convert_pass(&fe.store, &env, &mut std::collections::HashSet::new());

            let log = std::fs::read_to_string(agit_home.path().join("launches.jsonl"))
                .expect("the converted copy must get a launch record of its own");
            let recs: Vec<serde_json::Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
            let codex = recs.iter().find(|r| r["runtime"] == "codex").expect("claude-code → codex must be recorded");
            assert_eq!(codex["aid"], fe.aid, "agit's own output must be attributed to the agent it converted FROM");
        });
    }

    /// Store-bloat regression (QA: 1 source → 18 codex rollups over ~6 restarts). The watcher hands
    /// `convert_pass` a fresh `seen` set on every restart, so an already-converted source is re-processed
    /// each time. It must be IDEMPOTENT: the converted copy's id is deterministic in its source, so a
    /// re-convert overwrites the one installed file instead of minting a new rollup for capture to snap.
    /// A genuinely-new source, by contrast, must still convert.
    #[test]
    fn re_converting_the_same_source_is_idempotent_no_new_rollup() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        // Count the codex rollups agit has installed into the runtime dump — one per DISTINCT conversion.
        // Each new-id conversion is one more file here, and one more session for the next snap to commit.
        let count_codex = || -> usize {
            walkdir::WalkDir::new(home.path().join(".codex/sessions"))
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file() && e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
                .count()
        };

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let d = fe.store.join("sessions/claude-code");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();

            // First pass converts s1 → codex.
            crate::commands::convert_pass(&fe.store, &env, &mut std::collections::HashSet::new());
            assert_eq!(count_codex(), 1, "the source converts once");
            let after_first: Vec<_> = std::fs::read_dir(home.path().join(".codex/sessions/2026/01/01"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();

            // A watch RESTART: a brand-new `seen`. The old bug re-converted under a fresh random UUID,
            // leaving a SECOND rollup. Deterministic ids overwrite the one file instead.
            crate::commands::convert_pass(&fe.store, &env, &mut std::collections::HashSet::new());
            assert_eq!(count_codex(), 1, "re-converting the same source must NOT add a second rollup");
            let after_second: Vec<_> = std::fs::read_dir(home.path().join(".codex/sessions/2026/01/01"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(after_first, after_second, "the deterministic id must reuse the same filename");

            // A genuinely-new source still converts to its own file.
            std::fs::write(d.join("s2.jsonl"), transcript(&env, "2026-07-16T11:00:00.000Z")).unwrap();
            crate::commands::convert_pass(&fe.store, &env, &mut std::collections::HashSet::new());
            assert_eq!(count_codex(), 2, "a new source must convert to a new rollup");
        });
    }

    /// The snap must record a NON-NULL `last_activity` even for a transcript that carries no timestamp of
    /// its own — otherwise `latest_session` falls back to the machine mtime and two teammates disagree on
    /// which session is latest. The recorded value is the stable committed floor, so it also never churns
    /// a fresh sidecar commit on the next watch tick.
    #[test]
    fn snapping_a_timestampless_session_writes_a_non_null_recency() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();

            // A claude dump with NO top-level timestamp on any record — `last_activity` reads None.
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            let body = format!(
                "{{\"type\":\"user\",\"cwd\":\"{}\",\"message\":{{\"content\":\"hi\"}}}}\n",
                env.display()
            );
            std::fs::write(slug.join("s-notime.jsonl"), &body).unwrap();
            crate::commands::record_launch("s-notime", &fe.aid, "frontend", &env, "claude-code", None).unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            for r in &routed {
                mirror_owned("claude-code", &env, r).unwrap();
            }

            let sc = fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code").join("s-notime.agit.json");
            let side: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&sc).unwrap()).unwrap();
            assert!(!side["last_activity"].is_null(), "a timestamp-less session must NOT record a null recency");
            assert_eq!(side["last_activity"], NO_ACTIVITY_FLOOR, "it falls back to the stable committed floor");

            // Idempotent: a second snap of the same content must not rewrite the sidecar (no commit churn).
            let before = std::fs::metadata(&sc).unwrap().modified().unwrap();
            let (routed2, _) = route("claude-code", &env).unwrap();
            for r in &routed2 {
                mirror_owned("claude-code", &env, r).unwrap();
            }
            assert_eq!(std::fs::metadata(&sc).unwrap().modified().unwrap(), before, "the floored sidecar must be stable across snaps");
        });
    }

    /// The record's `name` is a snapshot from launch time, so a rename since then must not orphan it
    /// or make capture report a label that no longer exists: the aid is the identity.
    #[test]
    fn a_stale_label_in_the_record_still_resolves_by_aid() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            // launched as `web`, since renamed to `frontend`
            crate::commands::record_launch("s1", &fe.aid, "web", &env, "claude-code", None).unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            assert_eq!(routed.len(), 1);
            assert_eq!(routed[0].store, fe.store, "the store is keyed by aid, so a rename never moves it");
            assert_eq!(routed[0].agent.as_deref(), Some("frontend"), "the report must use the current label");
        });
    }

    /// The watch tick (capture_pass) mirrors AND commits — the daemon path manual `snap` now matches.
    /// Guards against regressing the auto-commit half of the watcher: one settled capture must advance
    /// the store's HEAD with an `auto-snap` commit, and count how many it committed.
    #[test]
    fn capture_pass_mirrors_and_commits_the_dump() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code", None).unwrap();

            let before = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            let mut count = 0u64;
            capture_pass("claude-code", &env, false, &mut count).unwrap();

            assert_eq!(count, 1, "one settled capture must record one commit");
            let after = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            assert_ne!(before, after, "capture_pass must commit; the store's HEAD must advance");
            let subject = scope::git_in(&fe.store, &["log", "-1", "--format=%s"]).unwrap();
            assert!(subject.starts_with("auto-snap claude-code"), "the commit must be the auto-snap: {subject}");
        });
    }

    /// `VERIFIED AS <user>` is now reachable end to end. A store created under a HOME whose git identity
    /// is `tester@agit.test` snaps a session whose provenance record carries THAT email (not the old
    /// hardcoded `agit@local`). Fed to the attribution step against a registry that maps that email to an
    /// account holding the session's signing key, it verifies as the person (`VerifiedAs`) — the state the
    /// old `agit@local` could never reach. The control proves the point: the same content signed under
    /// `agit@local` maps to no account and stays `SignedUnregistered`.
    #[test]
    fn a_snapped_session_binds_the_users_email_and_can_verify_as_the_person() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            // The store inherits the HOME git identity (local -> global), NOT a store-local agit@local.
            assert_eq!(
                crate::commands::committer_email(&fe.store),
                "tester@agit.test",
                "the store must inherit the user's git identity, not agit@local"
            );

            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code", None).unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            for r in &routed {
                mirror_owned("claude-code", &env, r).unwrap();
            }

            let dir = fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code");
            let content = std::fs::read_to_string(dir.join("s1.jsonl")).unwrap();
            let side: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("s1.agit.json")).unwrap()).unwrap();
            let prov: crate::commands::Provenance =
                serde_json::from_value(side["provenance"].clone()).expect("a snapped session must be signed");
            assert_eq!(prov.email, "tester@agit.test", "the provenance must bind the USER's email");
            assert_ne!(prov.email, "agit@local", "the old hardcoded handle must NOT be what is signed");

            // A registry that knows tester@agit.test with THIS machine's signing key -> VerifiedAs. The
            // same answer is reused for the control below, so each call gets its own FnOnce closure.
            let signing_key = prov.pubkey.clone();
            let registry = |email: &str| -> anyhow::Result<Option<crate::commands::RegisteredIdentity>> {
                Ok((email == "tester@agit.test").then(|| crate::commands::RegisteredIdentity {
                    username: "tester".into(),
                    ed25519_keys: vec![signing_key.clone()],
                }))
            };
            let status =
                crate::commands::verify_provenance_with_registry(agit_home.path(), &content, Some(&prov), false, |e| {
                    registry(e)
                })
                .unwrap();
            assert!(
                matches!(status, crate::commands::ProvenanceStatus::VerifiedAs { .. }),
                "the user's email + a matching registered key must verify as the person, got {status:?}"
            );

            // Control: the OLD behavior signed agit@local, which maps to NO account under the same registry
            // answer -> SignedUnregistered. This is exactly why VerifiedAs was previously unreachable.
            let key = crate::agent::machine_signing_key().unwrap();
            let control = crate::commands::sign_provenance(&key, &content, &fe.aid, "agit@local", &prov.started);
            let control_status = crate::commands::verify_provenance_with_registry(
                agit_home.path(),
                &content,
                Some(&control),
                false,
                |e| registry(e),
            )
            .unwrap();
            assert!(
                matches!(control_status, crate::commands::ProvenanceStatus::SignedUnregistered { .. }),
                "agit@local maps to no account, so it must NOT verify as anyone, got {control_status:?}"
            );
        });
    }

    /// The gate: a store under a HOME with NO git identity refuses to snap a session rather than binding it
    /// to a synthetic handle. It makes NO commit (HEAD does not advance), leaves NO staged content behind,
    /// and reports a non-zero outcome (`commit_snap` returns `true`) so a scripted `snap && push` stops.
    #[test]
    fn snap_refuses_and_makes_no_commit_when_the_committer_identity_is_unset() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            // Ignore a machine-level /etc/gitconfig too, so a system user.email on the CI box can't
            // resolve an identity behind the removed $HOME/.gitconfig and defeat the precondition.
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            let fe = crate::agent::init_agent("frontend").unwrap();
            // Strip this HOME's git identity so nothing resolves user.email anywhere.
            let _ = std::fs::remove_file(home.path().join(".gitconfig"));
            assert!(
                crate::commands::committer_email(&fe.store).is_empty(),
                "precondition: the committer identity must be unset"
            );

            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code", None).unwrap();

            // Mirror the dump (never gated), then attempt the gated commit that snap performs.
            let (routed, _) = route("claude-code", &env).unwrap();
            for r in &routed {
                mirror_owned("claude-code", &env, r).unwrap();
            }

            let before = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            let mut count = 0u64;
            let blocked = commit_snap(&fe.store, "claude-code", &mut count);

            assert!(blocked, "an unattributable snap must report a non-zero outcome");
            assert_eq!(count, 0, "nothing may be committed when the identity is unset");
            let after = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            assert_eq!(before, after, "the store HEAD must not advance");
            assert_eq!(
                scope::git_in_status(&fe.store, &["diff", "--cached", "--quiet"]).0,
                0,
                "the gate must leave NO staged content behind"
            );
            std::env::remove_var("GIT_CONFIG_NOSYSTEM");
        });
    }
}

#[cfg(test)]
mod merge_capture_tests {
    use super::*;

    /// A minimal valid claude transcript carrying a distinctive note.
    fn transcript(note: &str) -> String {
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"s\",\"uuid\":\"u1\",\"cwd\":\"/proj\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}}}}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"s\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"cwd\":\"/proj\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{note}\"}}]}}}}\n"
        )
    }

    /// The merge's core promise: `commit_merged_session` makes the merged session the store's LATEST and
    /// advances HEAD with a real commit — even against an OLDER session that already carries a recorded
    /// activity (so this proves the ordering, not just that a lone session wins by default).
    #[test]
    fn commit_merged_session_becomes_latest_and_advances_head() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            // A pre-existing, committed session with a DATED sidecar — the competitor the merge must beat.
            let old_dir = fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code");
            std::fs::create_dir_all(&old_dir).unwrap();
            std::fs::write(old_dir.join("old.jsonl"), transcript("older work")).unwrap();
            std::fs::write(
                old_dir.join("old.agit.json"),
                "{\"last_activity\":\"2020-01-01T00:00:00Z\",\"runtime\":\"claude-code\"}\n",
            )
            .unwrap();
            scope::git_in(&fe.store, &["add", "-A"]).unwrap();
            scope::git_in(&fe.store, &["commit", "-q", "--no-verify", "-m", "old session"]).unwrap();

            let before = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            let blocked = commit_merged_session(&fe.store, &env, "claude-code", "merge-xyz", &transcript("reconciled merge"))
                .unwrap();
            assert!(!blocked, "a clean merge transcript must commit, not be gated");

            let after = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            assert_ne!(before, after, "the merge must produce a commit; HEAD must advance");

            let latest = crate::commands::latest_session(&fe.store).expect("the store has sessions");
            assert_eq!(
                latest.path.file_stem().unwrap(),
                "merge-xyz",
                "the merged session must be the store's LATEST, ahead of the older dated session"
            );
            // And it is genuinely in history (committed, tracked), not just on disk.
            let tracked = scope::git_in(&fe.store, &["ls-files", "--", "sessions"]).unwrap();
            assert!(tracked.contains("merge-xyz.jsonl"), "the merged session must be committed: {tracked}");
        });
    }

    /// The gate is IDENTICAL to snap's: a merged transcript carrying a secret is mirrored to disk but
    /// held OUT of history, and the store's HEAD does not advance for it.
    #[test]
    fn commit_merged_session_with_a_secret_is_mirrored_but_not_committed() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let before = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();

            // A real AWS key inside the merged transcript.
            let secret = transcript("aws key AKIAIOSFODNN7EXAMPLE leaked in the merge");
            let blocked = commit_merged_session(&fe.store, &env, "claude-code", "merge-secret", &secret).unwrap();
            assert!(blocked, "a merged transcript carrying a secret must be gated (blocked)");

            let after = scope::git_in(&fe.store, &["rev-parse", "HEAD"]).unwrap();
            assert_eq!(before, after, "the store's HEAD must NOT advance for a gated merge");

            // Mirrored to disk (so nothing is lost)…
            let dst = fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code").join("merge-secret.jsonl");
            assert!(dst.exists(), "the merged transcript must still be mirrored on disk");
            // …but kept out of history: not tracked, not staged.
            let tracked = scope::git_in(&fe.store, &["ls-files", "--", "sessions"]).unwrap();
            assert!(!tracked.contains("merge-secret.jsonl"), "the secret merge must not be committed: {tracked}");
            let staged = scope::git_in(&fe.store, &["diff", "--cached", "--name-only"]).unwrap();
            assert!(staged.trim().is_empty(), "a blocked merge must leave nothing staged to spin on: {staged}");
        });
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn transcript(cwd: &Path) -> String {
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"timestamp\":\"2026-07-16T09:00:00.000Z\",\"message\":{{\"content\":\"hi\"}}}}\n",
            cwd.display()
        )
    }

    /// A store written before partitioning existed must keep working, forever and unmigrated. Several
    /// exist on disk right now, and `store_runtimes` globbing only ONE layout is what made `agit a
    /// merge` die with "No claude-code, codex sessions found to merge" against a store full of them.
    #[test]
    fn a_flat_store_still_reports_its_runtimes() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join(SESSIONS_SUBDIR);
        std::fs::create_dir_all(s.join("claude-code")).unwrap();
        std::fs::write(s.join("claude-code/old.jsonl"), "{}\n").unwrap();

        assert_eq!(store_runtimes(d.path()), vec!["claude-code"], "the pre-partition layout must never stop resolving");
    }

    /// The bug this pairs with: give a store the partitioned layout and a flat-only glob reports ZERO
    /// runtimes — a store visibly full of sessions reads as empty, and every consumer downstream of it
    /// fails claiming there is nothing there.
    #[test]
    fn a_partitioned_store_reports_its_runtimes() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join(SESSIONS_SUBDIR);
        std::fs::create_dir_all(s.join("-code-web/claude-code")).unwrap();
        std::fs::write(s.join("-code-web/claude-code/a.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(s.join("-code-api/codex")).unwrap();
        std::fs::write(s.join("-code-api/codex/b.jsonl"), "{}\n").unwrap();

        assert_eq!(store_runtimes(d.path()), vec!["claude-code", "codex"], "an env-partitioned store must report both");
    }

    /// Both layouts at once — the state every existing store enters the moment it is snapped again.
    /// Neither half may hide the other.
    #[test]
    fn a_half_migrated_store_reports_both_layouts() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join(SESSIONS_SUBDIR);
        std::fs::create_dir_all(s.join("claude-code")).unwrap();
        std::fs::write(s.join("claude-code/legacy.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(s.join("-code-api/codex")).unwrap();
        std::fs::write(s.join("-code-api/codex/new.jsonl"), "{}\n").unwrap();

        assert_eq!(store_runtimes(d.path()), vec!["claude-code", "codex"]);
    }

    /// A directory is not a session. `merges/` (dialogue transcripts) and a store with nothing in it
    /// must not be read as a runtime having sessions.
    #[test]
    fn an_empty_store_and_a_merges_dir_report_no_runtimes() {
        let d = tempfile::tempdir().unwrap();
        let s = d.path().join(SESSIONS_SUBDIR);
        std::fs::create_dir_all(s.join("merges")).unwrap();
        std::fs::write(s.join("merges/a-b.md"), "# transcript\n").unwrap();
        std::fs::create_dir_all(s.join("claude-code")).unwrap();

        assert!(store_runtimes(d.path()).is_empty(), "an empty runtime dir holds no sessions");
    }

    /// The writer half of §12 step 2, and PRD #3's visible payoff: a captured session's PATH records
    /// which environment it was recorded in, so `agit start` in another repo can say where the memory
    /// it is carrying came from.
    #[test]
    fn snap_writes_the_env_partitioned_path() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env)).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code", None).unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            mirror_owned("claude-code", &env, &routed[0]).unwrap();

            let want = fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code/s1.jsonl");
            assert!(want.exists(), "snap must partition by environment: {}", want.display());
            assert!(
                !fe.store.join(SESSIONS_SUBDIR).join("claude-code/s1.jsonl").exists(),
                "a new session must not land in the flat layout"
            );

            // …and the reader reports the partition, which is the half `agit start`'s header prints.
            let s = crate::commands::latest_session(&fe.store).unwrap();
            assert_eq!(s.env_slug.as_deref(), Some(env_slug(&env).as_str()));
        });
    }

    /// A store already holding a session flat keeps it there. Re-mirroring it into the partition would
    /// leave the store holding the SAME session twice — one copy frozen at the moment the layout
    /// changed — and `agit a list` counting it twice. Moving it would rewrite paths in a git repo
    /// nobody asked to rewrite.
    #[test]
    fn an_already_captured_session_is_not_duplicated_into_the_partition() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env)).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code", None).unwrap();

            // the store as a pre-partition agit left it
            let flat = fe.store.join(SESSIONS_SUBDIR).join("claude-code");
            std::fs::create_dir_all(&flat).unwrap();
            std::fs::write(flat.join("s1.jsonl"), transcript(&env)).unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            mirror_owned("claude-code", &env, &routed[0]).unwrap();

            assert!(flat.join("s1.jsonl").exists(), "the session must stay where the store already had it");
            assert!(
                !fe.store.join(SESSIONS_SUBDIR).join(env_slug(&env)).join("claude-code/s1.jsonl").exists(),
                "DUPLICATED: the same session now exists under both layouts"
            );
            assert_eq!(crate::commands::store_sessions(&fe.store).len(), 1, "one session, counted once");
        });
    }

    /// The loss partitioning actually prevents. Sessions are UUID-named and disjoint, so a flat store
    /// merely piles two repos' transcripts together — but `memory/` is ONE fixed name per checkout, so
    /// flat, the second repo to snap overwrote the first's memory, every time (§6's ping-pong).
    #[test]
    fn two_repos_sharing_one_store_keep_their_own_memory() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::init_agent("frontend").unwrap();
            // one agent, two code repos — the shared store the whole design is for
            for (i, env) in [a.path(), b.path()].into_iter().enumerate() {
                let slug = home.path().join(".claude/projects").join(claude_code::slug_for(env));
                std::fs::create_dir_all(slug.join("memory")).unwrap();
                std::fs::write(slug.join(format!("s{i}.jsonl")), transcript(env)).unwrap();
                std::fs::write(slug.join("memory/MEMORY.md"), format!("memory of repo {i}\n")).unwrap();
                crate::commands::record_launch(&format!("s{i}"), &fe.aid, "frontend", env, "claude-code", None).unwrap();

                let (routed, _) = route("claude-code", env).unwrap();
                mirror_owned("claude-code", env, &routed[0]).unwrap();
            }

            let mem = |env: &Path| {
                fe.store.join(SESSIONS_SUBDIR).join(env_slug(env)).join("claude-code/memory/MEMORY.md")
            };
            assert_eq!(std::fs::read_to_string(mem(a.path())).unwrap(), "memory of repo 0\n");
            assert_eq!(
                std::fs::read_to_string(mem(b.path())).unwrap(),
                "memory of repo 1\n",
                "the second repo's snap overwrote the first's memory"
            );
        });
    }
}

#[cfg(test)]
mod store_lock_tests {
    use super::*;

    /// The lock is mutual exclusion, and it releases on drop.
    #[test]
    fn a_second_taker_waits_and_a_released_lock_is_free() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path();
        let held = lock_store(store).unwrap();
        assert!(store.join(".git").join(STORE_LOCK).exists());
        drop(held);
        assert!(!store.join(".git").join(STORE_LOCK).exists(), "the lock must release on drop");
        lock_store(store).unwrap();
    }

    /// A holder killed mid-write (SIGKILL, a crash, a reboot) never releases. That must not wedge the
    /// store forever — but the lock may only be broken once the pid is PROVABLY gone.
    #[test]
    fn a_lock_held_by_a_dead_pid_is_broken_rather_than_waited_out() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path();
        std::fs::create_dir_all(store.join(".git")).unwrap();
        // A zeroed pidfile — what a truncated write leaves behind, and the one value `kill -0` gets
        // wrong: it signals the caller's own process group and succeeds. Believed, it wedges the store
        // forever, since no pid ever proves itself gone.
        std::fs::write(store.join(".git").join(STORE_LOCK), "0\n").unwrap();
        assert!(!pid_alive(0), "pid 0 must never read as alive: `kill -0 0` signals our own process group");

        let t = std::time::Instant::now();
        let _l = lock_store(store).expect("a dead holder's lock must be broken");
        assert!(t.elapsed() < LOCK_WAIT, "waited out the full timeout instead of breaking a dead lock");
    }

    /// The error has to name the holder and say how to clear it — a lock that fails with "resource
    /// busy" is a lock the user cannot act on.
    #[test]
    fn contention_names_the_holder_and_how_to_clear_it() {
        let d = tempfile::tempdir().unwrap();
        let store = d.path();
        let _held = lock_store(store).unwrap();
        // The live holder is THIS process, so the second take can never succeed: it waits out the
        // deadline and reports, rather than stealing its own lock and calling that success.
        let e = lock_store(store).unwrap_err().to_string();
        assert!(e.contains(&format!("pid {}", std::process::id())), "the error must name the holder: {e}");
        assert!(e.contains("rm "), "the error must say how to clear a stale lock: {e}");
        assert!(e.contains(&store.display().to_string()), "the error must name the store: {e}");
    }
}

#[cfg(test)]
mod watcher_registry_tests {
    use super::*;

    fn w(aid: &str, pid: u32) -> Watcher {
        Watcher { aid: aid.into(), env: "/code/web".into(), pid, started: "2026-07-17T00:00:00Z".into() }
    }

    /// The question `agit a list` asks is per-AGENT, and one agent is now watched from any of several
    /// repos. The pidfile is per-environment and cannot answer it; this log can.
    #[test]
    fn a_live_record_reads_as_watching_and_a_dead_one_never_does() {
        let home = tempfile::tempdir().unwrap();
        record_watcher_at(home.path(), &w("agt_01", std::process::id())).unwrap();
        // A watcher that was SIGKILLed leaves its record behind and nothing prunes the log, so
        // liveness — not the record's existence — is the answer.
        record_watcher_at(home.path(), &w("agt_02", 0)).unwrap();

        assert_eq!(watching_pid_at(home.path(), "agt_01"), Some(std::process::id()));
        assert_eq!(watching_pid_at(home.path(), "agt_02"), None, "a stale record must never read as watching");
        assert_eq!(watching_pid_at(home.path(), "agt_never"), None);
    }

    /// Two repos watching ONE shared store is the case the pidfile could not represent: it is one file
    /// per store, so the second watcher overwrote the first's pid and orphaned a live daemon. Both
    /// watchers must be visible, and the agent must read as watched while EITHER lives.
    #[test]
    fn two_environments_can_watch_one_agent() {
        let home = tempfile::tempdir().unwrap();
        record_watcher_at(home.path(), &Watcher { env: "/code/web".into(), ..w("agt_01", 0) }).unwrap();
        record_watcher_at(
            home.path(),
            &Watcher { env: "/code/api".into(), ..w("agt_01", std::process::id()) },
        )
        .unwrap();

        assert_eq!(
            watching_pid_at(home.path(), "agt_01"),
            Some(std::process::id()),
            "a dead watcher in one repo must not hide a live one in another"
        );
        assert_eq!(
            std::fs::read_to_string(watchers_file(home.path())).unwrap().lines().count(),
            2,
            "append-only: both watchers are recorded, neither overwrites the other"
        );
    }
}

#[cfg(test)]
mod claude_ownership_tests {
    use super::*;

    /// The leak this fix exists for: `/tmp/<x>/proj-a` and `/tmp/<x>/proj.a` collapse to ONE claude slug
    /// dir, so snapping project A used to mirror B's transcript into A's store (and push it to A's team).
    #[test]
    fn snap_only_takes_sessions_this_project_owns() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let a = base.path().join("proj-a"); // → slug -tmp-…-proj-a
        let b = base.path().join("proj.a"); // → slug -tmp-…-proj-a  (SAME)
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(
            claude_code::slug_for(&a),
            claude_code::slug_for(&b),
            "test is meaningless unless the two paths really collide"
        );

        // one shared slug dir holding a transcript from each project, plus one with no cwd at all
        let slug_dir = home.path().join(".claude/projects").join(claude_code::slug_for(&a));
        std::fs::create_dir_all(&slug_dir).unwrap();
        let rec = |cwd: &std::path::Path, msg: &str| {
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"s\",\"cwd\":\"{}\",\"message\":{{\"content\":\"{msg}\"}}}}\n",
                cwd.display()
            )
        };
        std::fs::write(slug_dir.join("mine.jsonl"), rec(&a, "mine")).unwrap();
        std::fs::write(slug_dir.join("theirs.jsonl"), rec(&b, "SECRET_OF_B")).unwrap();
        // drift: launch cwd is A, later records cd into a subdir → still A's
        std::fs::write(
            slug_dir.join("drift.jsonl"),
            rec(&a, "start") + &rec(&a.join("sub"), "after cd"),
        )
        .unwrap();
        std::fs::write(slug_dir.join("nocwd.jsonl"), "{\"type\":\"queue-operation\"}\n").unwrap();

        let agit_home = tempfile::tempdir().unwrap();
        testenv::with(home.path(), agit_home.path(), || {
            let owned: Vec<String> = claude_code::project_sessions(&a).into_iter().map(|(_, id)| id).collect();
            assert!(owned.contains(&"mine.jsonl".replace(".jsonl", "")), "own session must be captured");
            assert!(owned.contains(&"drift".to_string()), "cd-drift must not lose ownership");
            assert!(!owned.contains(&"theirs".to_string()), "LEAK: captured the colliding project's session");
            assert!(!owned.contains(&"nocwd".to_string()), "unattributable transcript must fail closed");
            // memory/ is slug-level: unattributable while a foreign session shares the dir
            assert!(!claude_code::slug_dir_is_exclusive(&a), "a foreign session shares this slug");
        });
    }

}


#[cfg(test)]
mod resolve_runtime_tests {
    use super::*;

    /// The pure branches of the no-default-runtime rule. `[only]` is the highest-risk line in the
    /// feature — it decides a runtime WITHOUT asking — so it is pinned here. (The `many` branch reads
    /// stdin and is covered by the integration suite.)
    #[test]
    fn explicit_wins_and_an_unknown_name_fails_loudly() {
        assert_eq!(resolve_runtime(Some("codex"), &[], "snap").unwrap(), "codex");
        // aliases normalize; presence is irrelevant on the explicit branch
        assert_eq!(resolve_runtime(Some("cc"), &[], "snap").unwrap(), "claude-code");
        assert_eq!(resolve_runtime(Some("claude"), &["codex"], "snap").unwrap(), "claude-code");

        let e = resolve_runtime(Some("bogus"), &["codex"], "snap").unwrap_err().to_string();
        assert!(e.contains("bogus"), "{e}");
        assert!(e.contains("claude-code") && e.contains("codex"), "the error must name the real runtimes: {e}");
    }

    #[test]
    fn none_present_fails_and_the_only_one_present_is_chosen_without_asking() {
        let e = resolve_runtime(None, &[], "snap").unwrap_err().to_string();
        assert!(e.contains("claude-code") && e.contains("codex"), "{e}");

        // exactly one present → chosen. NOT because it is claude: assert it for BOTH, so a silent
        // claude default could never satisfy this test.
        assert_eq!(resolve_runtime(None, &["codex"], "snap").unwrap(), "codex");
        assert_eq!(resolve_runtime(None, &["claude-code"], "snap").unwrap(), "claude-code");
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    /// A record with a real user prompt, wired into the parentUuid chain.
    fn user(sid: &str, uuid: &str, parent: Option<&str>, ts: &str, content: &str) -> String {
        let parent = parent.map(|p| format!("\"{p}\"")).unwrap_or_else(|| "null".into());
        format!(
            "{{\"type\":\"user\",\"sessionId\":\"{sid}\",\"uuid\":\"{uuid}\",\"parentUuid\":{parent},\"timestamp\":\"{ts}\",\"cwd\":\"/proj\",\"message\":{{\"role\":\"user\",\"content\":\"{content}\"}}}}"
        )
    }

    fn assistant(sid: &str, uuid: &str, parent: &str, ts: &str, text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"sessionId\":\"{sid}\",\"uuid\":\"{uuid}\",\"parentUuid\":\"{parent}\",\"timestamp\":\"{ts}\",\"cwd\":\"/proj\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{text}\"}}]}}}}"
        )
    }

    // ── A. SUBAGENT ─────────────────────────────────────────────────────────────────────────────
    // A captured session whose sub-thread lives under <id>/subagents/*.jsonl (the real Claude Code
    // shape: isSidechain records + a .meta.json naming the spawning Task) records a { spawn, leaf }
    // branch-point; the spawn resolves to the main-thread turn that issued that Task.

    #[test]
    fn subagent_from_subdir_records_spawn_turn_and_leaf() {
        let d = tempfile::tempdir().unwrap();
        let id = "S1";
        let src = d.path().join(format!("{id}.jsonl"));
        // Main thread: a user turn, then an assistant turn that issues a Task tool_use with a known id.
        let spawn_turn = "spawn-uuid-1";
        let task_id = "toolu_TASK123";
        let main = format!(
            "{}\n{{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"uuid\":\"{spawn_turn}\",\"parentUuid\":\"u1\",\"timestamp\":\"2026-07-20T10:00:01.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"{task_id}\",\"name\":\"Task\",\"input\":{{\"description\":\"do work\"}}}}]}}}}\n",
            user(id, "u1", None, "2026-07-20T10:00:00.000Z", "please do the thing"),
        );
        write(&src, &main);
        // The sub-thread transcript + its meta, under <id>/subagents/.
        let sub = d.path().join(id).join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let sub_leaf = "sub-leaf-9";
        let sub_body = format!(
            "{{\"type\":\"user\",\"uuid\":\"sub-1\",\"parentUuid\":null,\"isSidechain\":true,\"sessionId\":\"{id}\",\"message\":{{\"content\":\"go\"}}}}\n\
             {{\"type\":\"assistant\",\"uuid\":\"{sub_leaf}\",\"parentUuid\":\"sub-1\",\"isSidechain\":true,\"sessionId\":\"{id}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}\n"
        );
        write(&sub.join("agent-x.jsonl"), &sub_body);
        write(
            &sub.join("agent-x.meta.json"),
            &format!("{{\"agentType\":\"general-purpose\",\"toolUseId\":\"{task_id}\",\"name\":\"x\"}}"),
        );

        let lin = detect_lineage("claude-code", &src, id, &[]);
        assert_eq!(lin.subagents.len(), 1, "one sub-thread must be recorded: {lin:?}");
        let sa = &lin.subagents[0];
        assert_eq!(sa.spawn.as_deref(), Some(spawn_turn), "spawn resolves to the main-thread Task turn");
        assert_eq!(sa.leaf, sub_leaf, "leaf is the sub-thread's last record");
        assert!(lin.parent.is_none(), "a subagent host is still a root unless it also branched");
    }

    // An in-process teammate's sub-thread carries a meta with NO `toolUseId` (real dumps: most subagent
    // metas are these). The subagent must still be RECORDED and counted (its leaf is known) with a null
    // spawn, not silently dropped, so `agit a log`'s "+N subagents" reflects reality.
    #[test]
    fn subagent_without_tooluseid_is_still_recorded_with_null_spawn() {
        let d = tempfile::tempdir().unwrap();
        let id = "S1";
        let src = d.path().join(format!("{id}.jsonl"));
        write(&src, &format!("{}\n", user(id, "u1", None, "2026-07-20T10:00:00.000Z", "go")));
        let sub = d.path().join(id).join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        write(
            &sub.join("agent-tm.jsonl"),
            &format!(
                "{{\"type\":\"assistant\",\"uuid\":\"tm-leaf\",\"parentUuid\":null,\"isSidechain\":true,\"sessionId\":\"{id}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"done\"}}]}}}}\n"
            ),
        );
        // Meta WITHOUT a toolUseId, as an in-process teammate writes it.
        write(&sub.join("agent-tm.meta.json"), "{\"agentType\":\"in_process_teammate\",\"spawnDepth\":1}");

        let lin = detect_lineage("claude-code", &src, id, &[]);
        assert_eq!(lin.subagents.len(), 1, "a teammate sub-thread must still be recorded: {lin:?}");
        assert!(lin.subagents[0].spawn.is_none(), "no toolUseId means a null spawn, not a dropped subagent");
        assert_eq!(lin.subagents[0].leaf, "tm-leaf", "the leaf is still the sub-thread's last record");
    }

    #[test]
    fn subagent_inline_sidechain_run_is_detected() {
        // Older shape: sidechain records inline in the transcript. The run is attributed to the Task turn
        // that precedes it, and its leaf is the run's last record.
        let d = tempfile::tempdir().unwrap();
        let id = "S1";
        let src = d.path().join(format!("{id}.jsonl"));
        let body = format!(
            "{}\n\
             {{\"type\":\"assistant\",\"sessionId\":\"{id}\",\"uuid\":\"spawn-2\",\"parentUuid\":\"u1\",\"timestamp\":\"2026-07-20T10:00:01.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_T\",\"name\":\"Task\",\"input\":{{}}}}]}}}}\n\
             {{\"type\":\"user\",\"uuid\":\"sc-1\",\"parentUuid\":null,\"isSidechain\":true,\"sessionId\":\"{id}\",\"message\":{{\"content\":\"go\"}}}}\n\
             {{\"type\":\"assistant\",\"uuid\":\"sc-leaf\",\"parentUuid\":\"sc-1\",\"isSidechain\":true,\"sessionId\":\"{id}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}\n\
             {{\"type\":\"user\",\"sessionId\":\"{id}\",\"uuid\":\"u3\",\"parentUuid\":\"spawn-2\",\"timestamp\":\"2026-07-20T10:00:05.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"thanks\"}}}}\n",
            user(id, "u1", None, "2026-07-20T10:00:00.000Z", "do it"),
        );
        write(&src, &body);
        let lin = detect_lineage("claude-code", &src, id, &[]);
        assert_eq!(lin.subagents.len(), 1, "{lin:?}");
        assert_eq!(lin.subagents[0].spawn.as_deref(), Some("spawn-2"));
        assert_eq!(lin.subagents[0].leaf, "sc-leaf");
    }

    #[test]
    fn no_sidechain_records_no_subagents() {
        let d = tempfile::tempdir().unwrap();
        let id = "S1";
        let src = d.path().join(format!("{id}.jsonl"));
        write(
            &src,
            &format!(
                "{}\n{}\n",
                user(id, "u1", None, "2026-07-20T10:00:00.000Z", "hello"),
                assistant(id, "u2", "u1", "2026-07-20T10:00:01.000Z", "hi"),
            ),
        );
        let lin = detect_lineage("claude-code", &src, id, &[]);
        assert!(lin.subagents.is_empty(), "a session with no sidechains records none: {lin:?}");
    }

    // ── B. /branch PREFIX ───────────────────────────────────────────────────────────────────────
    // Two sessions sharing an identical opening prefix (same record uuids, re-stamped session id - what
    // a runtime /branch leaves) that then diverge: the later one records { parent, branch_point,
    // detected_by:"prefix" }, and the earlier one stays a root.

    #[test]
    fn branch_prefix_links_the_later_session_to_its_parent() {
        let d = tempfile::tempdir().unwrap();
        // Shared opening prefix: u1 (user prompt) + u2 (assistant). Parent PAR diverges at u3a; child
        // CHILD carries the same prefix (session id re-stamped to CHILD) and diverges at u3b later.
        let shared_user = |sid: &str| user(sid, "u1", None, "2026-07-20T09:00:00.000Z", "build a parser");
        let shared_asst = |sid: &str| assistant(sid, "u2", "u1", "2026-07-20T09:00:01.000Z", "starting");

        let par_src = d.path().join("PAR.jsonl");
        write(
            &par_src,
            &format!(
                "{}\n{}\n{}\n",
                shared_user("PAR"),
                shared_asst("PAR"),
                user("PAR", "u3a", Some("u2"), "2026-07-20T09:00:02.000Z", "use recursive descent"),
            ),
        );
        let child_src = d.path().join("CHILD.jsonl");
        write(
            &child_src,
            &format!(
                "{}\n{}\n{}\n",
                shared_user("CHILD"),
                shared_asst("CHILD"),
                user("CHILD", "u3b", Some("u2"), "2026-07-20T10:00:00.000Z", "use a PEG grammar instead"),
            ),
        );

        // The later session (CHILD) sees PAR as a sibling and records the branch.
        let lin = detect_lineage("claude-code", &child_src, "CHILD", &[(par_src.clone(), "PAR".into())]);
        assert_eq!(lin.parent.as_deref(), Some("PAR"), "child links to the earlier parent: {lin:?}");
        assert_eq!(lin.detected_by.as_deref(), Some("prefix"));
        assert_eq!(lin.branch_point.as_deref(), Some("u2"), "branch point is the last shared uuid");

        // The earlier session (PAR) does NOT record CHILD as its parent - the edge is asymmetric.
        let lin_par = detect_lineage("claude-code", &par_src, "PAR", &[(child_src, "CHILD".into())]);
        assert!(lin_par.parent.is_none(), "the earlier session stays a root: {lin_par:?}");
    }

    #[test]
    fn unrelated_sessions_with_different_openings_are_not_a_branch() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("A.jsonl");
        let b = d.path().join("B.jsonl");
        write(&a, &format!("{}\n", user("A", "u1", None, "2026-07-20T09:00:00.000Z", "fix the login bug")));
        write(&b, &format!("{}\n", user("B", "u1", None, "2026-07-20T10:00:00.000Z", "write release notes")));
        let lin = detect_lineage("claude-code", &b, "B", &[(a, "A".into())]);
        assert!(lin.parent.is_none(), "different opening prompts are not a branch: {lin:?}");
    }

    // ── C. /branch LEAF ─────────────────────────────────────────────────────────────────────────
    // SKIPPED as a real-data test: no `type:summary` record exists in any dump under ~/.claude/projects,
    // and every `leafUuid` (carried by `last-prompt` records) resolves within its OWN session - never
    // cross-session. The exact cross-session leaf link could not be confirmed, so per the task it is not
    // faked. The resolver below is still exercised to prove it fires ONLY on a genuine cross-session link
    // and correctly ignores the same-session bookmark that real dumps actually contain.

    #[test]
    fn leaf_link_fires_only_across_sessions_never_on_a_same_session_bookmark() {
        let d = tempfile::tempdir().unwrap();
        let parent = d.path().join("PAR.jsonl");
        write(
            &parent,
            &format!("{}\n", user("PAR", "shared-leaf", None, "2026-07-20T09:00:00.000Z", "start here")),
        );
        // A same-session bookmark: leafUuid points at this session's OWN uuid → not a branch.
        let self_bookmark = d.path().join("SELF.jsonl");
        write(
            &self_bookmark,
            &format!(
                "{}\n{{\"type\":\"last-prompt\",\"sessionId\":\"SELF\",\"leafUuid\":\"own-1\"}}\n",
                user("SELF", "own-1", None, "2026-07-20T09:30:00.000Z", "keep going"),
            ),
        );
        let lin_self = detect_lineage("claude-code", &self_bookmark, "SELF", &[(parent.clone(), "PAR".into())]);
        assert!(lin_self.parent.is_none(), "a same-session bookmark is not a branch: {lin_self:?}");

        // A genuine cross-session link: leafUuid resolves into PAR's uuid → detected_by:"leaf".
        let branched = d.path().join("BR.jsonl");
        write(
            &branched,
            &format!(
                "{{\"type\":\"summary\",\"leafUuid\":\"shared-leaf\"}}\n{}\n",
                user("BR", "b1", None, "2026-07-20T10:00:00.000Z", "diverge now"),
            ),
        );
        let lin = detect_lineage("claude-code", &branched, "BR", &[(parent, "PAR".into())]);
        assert_eq!(lin.parent.as_deref(), Some("PAR"), "cross-session leaf link resolves to the parent: {lin:?}");
        assert_eq!(lin.detected_by.as_deref(), Some("leaf"));
        assert_eq!(lin.branch_point.as_deref(), Some("shared-leaf"));
    }

    // ── D. ROOT ─────────────────────────────────────────────────────────────────────────────────

    #[test]
    fn a_lone_session_is_a_root_with_null_parent_and_no_subagents() {
        let d = tempfile::tempdir().unwrap();
        let id = "ONLY";
        let src = d.path().join(format!("{id}.jsonl"));
        write(
            &src,
            &format!(
                "{}\n{}\n",
                user(id, "u1", None, "2026-07-20T10:00:00.000Z", "hello"),
                assistant(id, "u2", "u1", "2026-07-20T10:00:01.000Z", "hi"),
            ),
        );
        let lin = detect_lineage("claude-code", &src, id, &[]);
        assert_eq!(lin, Lineage::default(), "a lone session is a pure root: {lin:?}");
        // And the committed shape carries explicit nulls + an empty list (round-trips through the reader).
        let j = lin.to_json();
        assert!(j.get("parent").unwrap().is_null());
        assert!(j.get("subagents").unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn read_lineage_round_trips_and_missing_sidecar_is_a_root() {
        let d = tempfile::tempdir().unwrap();
        let transcript = d.path().join("X.jsonl");
        write(&transcript, "{}\n");
        // No sidecar yet → root.
        assert_eq!(read_lineage(&transcript), Lineage::default());
        // Write a sidecar carrying a full lineage and read it back.
        let lin = Lineage {
            parent: Some("PAR".into()),
            branch_point: Some("bp-1".into()),
            detected_by: Some("prefix".into()),
            subagents: vec![Subagent { spawn: Some("sp".into()), leaf: "lf".into() }],
        };
        let side = crate::commands::sidecar_path(&transcript);
        write(&side, &format!("{{\"lineage\":{}}}", lin.to_json()));
        assert_eq!(read_lineage(&transcript), lin, "lineage survives a write/read round-trip");
    }
}
