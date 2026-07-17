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
    match rt {
        "claude-code" => !claude_code::project_sessions(env).is_empty(),
        "codex" => !crate::adapter::codex::project_rollouts(env).is_empty(),
        _ => false,
    }
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
                bail!("{names} all have sessions — say which with --from <runtime>.");
            }
            println!("Sessions exist for {names}. Which runtime should agit {what}?");
            for (i, rt) in many.iter().enumerate() {
                println!("  {}) {rt}", i + 1);
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

impl Routed {
    /// The command that actually commits what was just written. `agit a` resolves the legacy nested
    /// store, so it is the right hint for that store and the wrong one for any other.
    fn commit_hint(&self, rt: &str) -> String {
        match self.agent {
            None => format!("agit a add -A && agit a commit -m 'snap {rt} sessions'"),
            Some(_) => {
                let s = self.store.display();
                format!("git -C {s} add -A && git -C {s} commit -m 'snap {rt} sessions'")
            }
        }
    }
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
            eprintln!(
                "  ⚠ {id} was launched by {} ({}), which this machine no longer has — not captured: {e:#}",
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
/// where they came from.
fn live_sessions(rt: &str, env: &Path) -> Result<(Vec<(PathBuf, String)>, String)> {
    match rt {
        // Claude splits by project slug — which collides, so `project_sessions` decides ownership by
        // each transcript's launch cwd: `/a/b.c`, `/a/b-c` and `/a/b/c` share ONE slug directory, and
        // tree-mirroring it copied a *different* project's transcripts into this store, which a push
        // then shipped to this project's teammates.
        "claude-code" => {
            // source_dir first: "this project has never run in Claude Code" is a better error than an
            // empty list, and it names the HOME agit actually looked under.
            let src = source_dir("claude-code", env)?;
            let owned = claude_code::project_sessions(env);
            let desc = format!("{} ({} owned sessions)", src.display(), owned.len());
            Ok((owned, desc))
        }
        // Codex splits by date with every project mixed together, and a fork/resume rollout embeds the
        // parent session of another project — `project_rollouts` skips the whole file when it sees one.
        "codex" => {
            let owned = crate::adapter::codex::project_rollouts(env);
            let root = crate::adapter::codex::sessions_root()
                .map(|r| r.display().to_string())
                .unwrap_or_default();
            let desc = format!("{root} (cwd={} matched {} rollouts)", env.display(), owned.len());
            Ok((owned, desc))
        }
        other => bail!("session dump for runtime `{other}` isn't wired up yet (see src/session.rs)"),
    }
}

/// `agit a snap [--from <runtime>]` — mirror session dumps into the Agent Store, once.
///
/// With no `--from` there is nothing to default to: claude-code and codex are peers, so snap captures
/// BOTH (the shape `watch` already uses), skipping quietly whichever has no sessions for this project.
/// An explicit `--from` is a different contract — the user named a runtime, so its absence is an error.
pub fn snap(runtime: Option<&str>, capture_harness: bool) -> Result<i32> {
    // A pre-identity repo gets the SAME actionable error here as from every other agent-scoped command.
    // Without this, snap bailed on "no sessions found" before ever resolving the store, so the one
    // command a new user is most likely to type never surfaced the `agit a import` path.
    if let Ok(env) = scope::env_root() {
        if crate::agent::legacy_store(&env).is_some() {
            crate::agent::resolve(None)?;
        }
    }
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
    let live = live_runtimes(&env);
    if live.is_empty() {
        bail!(
            "No {} sessions found for this project.\n\
             (has it been run in either yet? `agit adapter` lists the runtimes; `--from <runtime>` forces one.)",
            runtime_list()
        );
    }
    for rt in &live {
        snap_one(rt, &env, capture_harness)?;
    }
    Ok(0)
}

/// `agit a snap --from <runtime>` — mirror one named runtime's dump into the Agent Store.
/// `capture_harness` also captures the project's MCP/skills/config (redacting secrets); `--no-harness` skips it.
pub fn sync(runtime: &str, capture_harness: bool) -> Result<i32> {
    let env = scope::env_root()?;
    snap_one(&normalize(runtime), &env, capture_harness)
}

fn snap_one(rt: &str, env: &Path, capture_harness: bool) -> Result<i32> {
    let rt = normalize(rt);
    let (routed, source_desc) = route(&rt, env)?;

    println!("Mirrored the session dump for {}:", rt);
    println!("  source : {source_desc}");
    if routed.is_empty() {
        println!("  (no sessions this project owns — nothing to mirror)");
        return Ok(0);
    }

    for r in &routed {
        if let Some(n) = &r.note {
            eprintln!("  note   : {n}");
        }
        // Held across mirror + harness capture: everything this pass writes into the store.
        let _lock = lock_store(&r.store)?;
        let (stats, hits, dst) = mirror_owned(&rt, env, r)?;
        let who = match &r.agent {
            Some(a) => format!("   ({a} · {} session(s))", r.sessions.len()),
            None => String::new(),
        };
        println!("  target : {}{who}", dst.display());
        println!("  files  : {} files ({} updated / {} added), {} bytes", stats.total, stats.updated, stats.added, stats.bytes);
        if hits > 0 {
            eprintln!("  ⚠ Found {hits} likely secrets — the session transcript carries sensitive content the agent has seen.");
            eprintln!("     This will be blocked again before push; run `agit -a scan` first to check, or clear it from the transcript.");
        }

        // Capture the harness (MCP servers / skills / config) alongside the sessions, redacting
        // secrets. The harness is project-scoped, so every agent that worked here carries its own
        // copy — a store a teammate clones has to stand on its own.
        if capture_harness {
            match crate::harness::capture(&r.store, env, &rt) {
                Ok(h) if h.files > 0 => {
                    println!("  harness: {} files ({} secret field(s) redacted)", h.files, h.redactions.len());
                    for w in &h.warnings {
                        eprintln!("  ⚠ {w}");
                    }
                }
                Ok(_) => {}
                // Harness capture must never fail the snap — the session dump is already mirrored.
                Err(e) => eprintln!("  ⚠ harness capture skipped: {e:#}"),
            }
        }
        println!("\n  Commit: {}", r.commit_hint(&rt));
    }
    Ok(0)
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
                 \x20      Another agit is writing this agent — one store is shared by every repo that tracks it.\n\
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
            write_sidecar(&dp, o, env, rt)?;
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
fn write_sidecar(dp: &Path, o: &Owned, env: &Path, rt: &str) -> Result<()> {
    let mut v = serde_json::json!({
        "env": env.display().to_string(),
        "runtime": rt,
        "last_activity": last_activity(&o.src),
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
    }
    let body = format!("{}\n", serde_json::to_string_pretty(&v)?);
    let p = crate::commands::sidecar_path(dp);
    if std::fs::read_to_string(&p).ok().as_deref() != Some(body.as_str()) {
        std::fs::write(&p, body)?;
    }
    Ok(())
}

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
    let rt = normalize(runtime);
    let interval = std::time::Duration::from_secs(interval_secs.max(1));
    let watch = source_path(&rt, &env);

    println!("Auto-snapping {rt} on every change (settling window {interval_secs}s). Ctrl-C to stop.");
    if watch.as_deref().map(|p| !p.exists()).unwrap_or(true) {
        println!("  (waiting for {rt} sessions to appear…)");
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
                Err(e) => eprintln!("  snap failed: {e:#}"),
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
                eprintln!("  snap {rt} skipped this tick: {e:#}");
                continue;
            }
        };
        match mirror_owned(rt, env, r) {
            Ok((stats, hits, _)) if stats.added + stats.updated > 0 => {
                if capture_harness {
                    let _ = crate::harness::capture(&r.store, env, rt);
                }
                commit_snap(&r.store, rt, hits, count);
            }
            Ok(_) => {}
            Err(e) => eprintln!("  snap {rt} failed: {e:#}"),
        }
        drop(lock);
    }
    Ok(routed.into_iter().map(|r| r.store).collect())
}

/// Stage + commit the mirrored dump. Nothing staged → no-op. Commit blocked by the pre-commit secret hook → warn.
fn commit_snap(agent: &Path, rt: &str, hits: usize, count: &mut u64) {
    let _ = scope::git_in_status(agent, &["add", "-A"]);
    // `diff --cached --quiet` exits 1 when something is staged, 0 when nothing is.
    if scope::git_in_status(agent, &["diff", "--cached", "--quiet"]).0 == 0 {
        return;
    }
    let ts = now_iso();
    let (rc, _) = scope::git_in_status(agent, &["commit", "-m", &format!("auto-snap {rt} {ts}")]);
    if rc == 0 {
        *count += 1;
        println!("  ● snapped {ts}  (#{count})");
    } else {
        eprintln!(
            "  ⚠ auto-snap not committed{} — mirrored to disk but the pre-commit hook refused it. `agit -a scan` to see.",
            if hits > 0 { " (likely secrets)" } else { "" }
        );
        let _ = scope::git_in_status(agent, &["reset", "-q"]); // unstage so we don't spin on it
    }
}

/// Where a runtime's session dump for this project lives (no existence check — the watcher waits for it).
fn source_path(rt: &str, env: &Path) -> Option<PathBuf> {
    match rt {
        "claude-code" => claude_code::projects_dir().ok().map(|d| d.join(claude_code::slug_for(env))),
        "codex" => crate::adapter::codex::sessions_root().ok(),
        _ => None,
    }
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

fn normalize(runtime: &str) -> String {
    match runtime {
        "claude" | "cc" | "claude-code" => "claude-code".into(),
        other => other.to_string(),
    }
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
    println!(
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
                    Err(e) => eprintln!("  snap {rt} failed: {e:#}"),
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
            eprintln!("  ⚠ watcher not announced ({e:#}) — `agit a list` will not show this agent as watched.");
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let env = scope::env_root()?;
    let rundir = watch_rundir(&env)?;
    let logp = rundir.join("agit-watch.log");
    let pidp = rundir.join(WATCH_PID);
    if let Some(pid) = read_pid(&pidp) {
        if pid_alive(pid) {
            println!("agit watch already running (pid {pid}). Stop it with: agit watch --stop");
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
        .stderr(log2)
        .process_group(0); // new process group → survives the launching shell's SIGHUP
    let child = cmd.spawn().context("failed to spawn the background watcher")?;
    let pid = child.id();
    std::fs::write(&pidp, pid.to_string())?;
    println!("agit watch started in the background (pid {pid}).");
    println!("  log:    {}", logp.display());
    println!("  status: agit watch --status   ·   stop: agit watch --stop");
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
                println!("Stopped agit watch (pid {pid}).");
            } else {
                println!("No live process for pid {pid}; cleared the stale pidfile.");
            }
            Ok(0)
        }
        None => {
            println!("No background watcher is recorded for this project.");
            Ok(0)
        }
    }
}

/// `agit watch --status` — report whether the background watcher is running.
pub fn watch_status() -> Result<i32> {
    let rundir = watch_rundir(&scope::env_root()?)?;
    match read_pid(&rundir.join(WATCH_PID)) {
        Some(pid) if pid_alive(pid) => {
            println!("agit watch is running (pid {pid}).");
            println!("  log: {}", rundir.join("agit-watch.log").display());
        }
        _ => println!("agit watch is not running for this project."),
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
        f();
        restore("HOME", &old[0]);
        restore("AGIT_HOME", &old[1]);
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
            let fe = crate::agent::new_agent("frontend").unwrap();
            let api = crate::agent::new_agent("api").unwrap();

            // ONE dump folder: claude keys on the project's cwd slug, never on the agent.
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s-fe.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            std::fs::write(slug.join("s-api.jsonl"), transcript(&env, "2026-07-16T10:00:00.000Z")).unwrap();

            crate::commands::record_launch("s-fe", &fe.aid, "frontend", &env, "claude-code").unwrap();
            crate::commands::record_launch("s-api", &api.aid, "api", &env, "claude-code").unwrap();

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
            let fe = crate::agent::new_agent("frontend").unwrap();
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

    /// The record's `name` is a snapshot from launch time, so a rename since then must not orphan it
    /// or make capture report a label that no longer exists: the aid is the identity.
    #[test]
    fn a_stale_label_in_the_record_still_resolves_by_aid() {
        let home = tempfile::tempdir().unwrap();
        let agit_home = tempfile::tempdir().unwrap();
        let envd = tempfile::tempdir().unwrap();
        let env = envd.path().to_path_buf();

        testenv::with(home.path(), agit_home.path(), || {
            let fe = crate::agent::new_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env, "2026-07-16T09:00:00.000Z")).unwrap();
            // launched as `web`, since renamed to `frontend`
            crate::commands::record_launch("s1", &fe.aid, "web", &env, "claude-code").unwrap();

            let (routed, _) = route("claude-code", &env).unwrap();
            assert_eq!(routed.len(), 1);
            assert_eq!(routed[0].store, fe.store, "the store is keyed by aid, so a rename never moves it");
            assert_eq!(routed[0].agent.as_deref(), Some("frontend"), "the report must use the current label");
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
            let fe = crate::agent::new_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env)).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code").unwrap();

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
            let fe = crate::agent::new_agent("frontend").unwrap();
            let slug = home.path().join(".claude/projects").join(claude_code::slug_for(&env));
            std::fs::create_dir_all(&slug).unwrap();
            std::fs::write(slug.join("s1.jsonl"), transcript(&env)).unwrap();
            crate::commands::record_launch("s1", &fe.aid, "frontend", &env, "claude-code").unwrap();

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
            let fe = crate::agent::new_agent("frontend").unwrap();
            // one agent, two code repos — the shared store the whole design is for
            for (i, env) in [a.path(), b.path()].into_iter().enumerate() {
                let slug = home.path().join(".claude/projects").join(claude_code::slug_for(env));
                std::fs::create_dir_all(slug.join("memory")).unwrap();
                std::fs::write(slug.join(format!("s{i}.jsonl")), transcript(env)).unwrap();
                std::fs::write(slug.join("memory/MEMORY.md"), format!("memory of repo {i}\n")).unwrap();
                crate::commands::record_launch(&format!("s{i}"), &fe.aid, "frontend", env, "claude-code").unwrap();

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
