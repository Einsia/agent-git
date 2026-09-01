//! `agit rc` — the resident daemon (`agitd`) and everything it needs.
//!
//! # What this is
//!
//! Today a session lives in the terminal that launched it. `agitd` moves the
//! session's *body* to a resident process on the machine and turns every
//! terminal, web page and phone into a *view* of it. Close the laptop lid, the
//! session keeps running; open the web page, you see it where it is.
//!
//! One rule decides every design choice below: **anything that would turn a
//! view back into the body is out** — most obviously "ship the TUI's byte
//! stream to the browser". Byte streams lose structure (which part is a tool
//! call, which is prose), can't render an approval as two buttons on a phone,
//! and — fatal for agit — cannot be committed losslessly. agit's whole value is
//! "structured events → turns → commits", so the daemon speaks structured
//! events end to end.
//!
//! # Topology
//!
//! ```text
//!   ┌──────────── machine ─────────────┐        ┌── hub ──┐        ┌ viewers ┐
//!   │ harness ⇄ agitd ──(WSS, outbound)─┼──────▶│  relay  │◀──────▶│ web/app │
//!   │  (claude / codex)   ▲             │        │ project │        └─────────┘
//!   │      transcript ────┘ (tail)      │        └─────────┘
//!   └──────────────────────────────────┘
//! ```
//!
//! * `agitd` connects **out** to the hub (WSS 443). The hub never connects in.
//!   Users' machines sit behind NAT and corporate firewalls; asking them to open
//!   an inbound port is asking them to configure a router, and then nobody uses
//!   the feature.
//! * The **trust boundary is on the machine**. The hub is a relay and a
//!   projection, not a source of authority. `agitd` re-checks every instruction
//!   it receives: does the target workspace belong to this connection, is the
//!   path inside the allowlist, does the action need an approval. If the hub is
//!   compromised, the blast radius is capped by `agitd`'s local policy.
//! * The harness's **own transcript file** is the source of truth for completed
//!   items. `agitd` tails it and runs each new line through the same
//!   `adapter::parse` that `agit show` uses, so the live stream is
//!   line-identical to what `agit commit` will snapshot. stdout is used only for
//!   what the file doesn't have: token deltas, approvals, turn results.
//!
//! # Module map
//!
//! | module | job |
//! |---|---|
//! | [`identity`]  | machine fingerprint + per-hub connection token (`~/.agit/rc/`) |
//! | [`mirror`]    | local mirror of hub workspace definitions → the path allowlist |
//! | [`policy`]    | allowlist enforcement (canonical paths, no `..`, no symlink escapes) |
//! | [`journal`]   | per-session `seq` allocation, ring buffer, durable watermark |
//! | [`tail`]      | transcript file tailer → `(lineno, raw_line)` |
//! | [`harness`]   | drivers: `claude_code` (stream-json), `codex` (app-server JSON-RPC) |
//! | [`supervisor`]| session registry; turns harness events + tailed lines into protocol frames |
//! | [`link`]      | the WSS link to the hub: register, heartbeat, backoff, replay |
//! | [`control`]   | local unix socket for `agit rc status` / `stop` |
//! | [`daemon`]    | wires it all up; the thing `agit rc start` runs |
//!
//! # Why the seq is allocated here and not at the hub
//!
//! Resume-after-disconnect needs a monotonic, gap-free number per stream. Three
//! candidates: the hub, Redis `XADD`, or `agitd`. `agitd` wins because (1) it is
//! the only party that sees the true event order; (2) it keeps numbering while
//! the hub is unreachable, so on reconnect it can say "I'm at 5000, you have
//! 4200, here's the gap"; (3) producer-side numbering gives every consumer an
//! end-to-end hole-detection contract — a dropped frame at any hop is
//! *detected*, not silently lost. The hub's only job is to reject holes.

pub mod control;
pub mod daemon;
pub mod grants;
pub mod harness;
pub mod identity;
pub mod journal;
pub mod lineage;
pub mod link;
pub mod mirror;
pub mod outbound;
pub mod policy;
pub mod roster;
pub mod supervisor;
pub mod tail;
pub mod terminal;
pub mod ticket;
#[cfg(windows)]
mod windows_job;

use std::path::PathBuf;

/// `~/.agit/rc/` — everything the daemon persists.
pub fn rc_dir() -> crate::Result<PathBuf> {
    #[cfg(test)]
    let home = test_agit_home_override()
        .map(Ok)
        .unwrap_or_else(crate::infra::config::agit_home)?;
    #[cfg(not(test))]
    let home = crate::infra::config::agit_home()?;
    let d = home.join("rc");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// What a workspace confines **right now**: the allowlist plus the command names the owner
/// granted.
///
/// # Why it must be live
///
/// This data **is** the whole basis for "it still holds" — deciding "does this approval go back
/// to the owner" reads it, not a danger-word denylist. So a stale root is a long-lived operator
/// pass: the owner unbinds `/srv/app` in the web interface, [`mirror`] clears it on the next
/// reconnect, yet a session that is still alive holds the roots copied at the moment of launch,
/// and the operator goes on allowing reads and writes in a directory that stopped belonging to
/// this workspace long ago.
///
/// It therefore reaches every session over `tokio::sync::watch` instead of being copied once at
/// `Session::launch`. A read is a synchronous `borrow()` that does not await, so it cannot
/// deadlock against the daemon's big lock.
#[derive(Debug, Clone, Default)]
pub struct Confinement {
    /// The directories bound into this workspace.
    pub roots: policy::CanonicalRoots,
    /// Command names the owner granted the operator to answer itself (see [`grants`]).
    pub operator_heads: std::collections::BTreeSet<String>,
}

/// Read one of `~/.agit/rc/*.json` back, falling back to empty.
///
/// A missing file, broken JSON and an unreadable read all count as empty — these are **caches**,
/// not ledgers; refusing to come up because of one half-written file costs more than losing that
/// much state.
///
/// **Not every file can do this.** `sessions.json` (the roster) is a ledger: the monotonic
/// `ever_dangerous` bit lives only in it, and reading it back as empty launders a session that
/// handed out a shell with no approval. It goes through [`load_ledger`].
pub fn load_json<T: Default + serde::de::DeserializeOwned>(name: &str) -> T {
    rc_dir()
        .ok()
        .and_then(|d| std::fs::read_to_string(d.join(name)).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// What one ledger load can come back as.
///
/// **"empty" and "unreadable" must stay apart.** Empty means "none of that ever happened";
/// unreadable means "it happened, and I do not know which". The roster is where that monotonic
/// `ever_dangerous` bit lives — taking the second for the first washes a security fact away.
pub enum Ledger<T> {
    Loaded(T),
    /// The file does not exist yet — a first run, genuinely empty.
    Missing,
    /// Readable but unusable (a parse failure, or an I/O error other than NotFound).
    Unusable,
}

/// Read a **ledger**. The whole difference from [`load_json`] (a cache) is in the return type.
///
/// It still does not refuse to come up (a daemon that cannot start costs more), but it says so,
/// and it moves the bad file aside instead of letting the next write overwrite it — that file is
/// the only thing anyone can examine afterwards. How to degrade is the caller's decision, because
/// only the caller knows what this ledger records.
pub fn load_ledger<T: serde::de::DeserializeOwned>(name: &str) -> Ledger<T> {
    let Ok(dir) = rc_dir() else {
        return Ledger::Unusable;
    };
    let path = dir.join(name);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ledger::Missing,
        // Permissions, an I/O error, the directory taken by something else — **not** "it never
        // happened".
        Err(e) => {
            eprintln!("agitd: could not read {} ({e})", path.display());
            return Ledger::Unusable;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Ledger::Loaded(v),
        Err(e) => {
            let salvage = path.with_extension("json.corrupt");
            let _ = std::fs::rename(&path, &salvage);
            eprintln!(
                "agitd: {} is unreadable ({e}); moved it to {}.\n  \u{2192} every session will be treated as owner-only until it is restored — that file is the only record of which ones ran without permission checks",
                path.display(),
                salvage.display()
            );
            Ledger::Unusable
        }
    }
}

/// Write one of `~/.agit/rc/*.json` atomically (write-temp-then-rename), so a
/// crash mid-write can't leave a truncated file behind. The file and, on Unix,
/// the containing directory are synced before success: start idempotency uses
/// this return as the hard boundary before launching an external process.
pub fn save_json<T: serde::Serialize>(name: &str, value: &T) -> crate::Result<()> {
    let p = rc_dir()?.join(name);
    let parent = p
        .parent()
        .ok_or_else(|| anyhow::anyhow!("RC ledger path has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(
        &mut temporary,
        serde_json::to_string_pretty(value)?.as_bytes(),
    )?;
    temporary.as_file().sync_all()?;
    temporary.persist(&p).map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Durably remove a superseded RC ledger.
///
/// The directory sync is part of the contract: after a fail-closed snapshot is
/// promoted into its primary file, a crash must not resurrect the old fallback
/// directory entry and roll newer state back on the next launch.
pub fn remove_json(name: &str) -> crate::Result<()> {
    let path = rc_dir()?.join(name);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_rc_dir()
}

/// Confirm the current RC directory entries as a durable startup boundary.
///
/// In particular, a previous unlink may have succeeded before its directory
/// fsync reported failure. A later launch that observes no fallback must sync
/// that absence before treating recovery as complete.
pub fn sync_rc_dir() -> crate::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(rc_dir()?)?.sync_all()?;
    Ok(())
}

/// Best-effort hostname for the default display name.
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME")
        && !h.trim().is_empty()
    {
        return h.trim().to_string();
    }
    if let Ok(s) = std::fs::read_to_string("/etc/hostname")
        && !s.trim().is_empty()
    {
        return s.trim().to_string();
    }
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "unknown-host".into()
}

/// `linux-x86_64`, `macos-aarch64`, …
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
thread_local! {
    /// Every caller of `with_agit_home` runs in a synchronous test or on a current-thread Tokio
    /// runtime. A thread-local stack rather than a process environment variable keeps a test that
    /// never opted in from writing into someone else's temporary ledger.
    static TEST_AGIT_HOME_STACK: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn test_agit_home_override() -> Option<PathBuf> {
    TEST_AGIT_HOME_STACK.with(|stack| stack.borrow().last().cloned())
}

#[cfg(test)]
fn test_agit_home_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
struct TestAgitHomeGuard {
    depth: usize,
    /// Keep the helper's historical outer-scope serialization: one scope's
    /// synchronous fault injection and ledger assertions cannot interleave with
    /// another helper scope. Nested scopes already share this guard.
    _serial: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl Drop for TestAgitHomeGuard {
    fn drop(&mut self) {
        TEST_AGIT_HOME_STACK.with(|stack| stack.borrow_mut().truncate(self.depth));
    }
}

/// Run a piece of test code synchronously with `home` as the home directory.
///
/// The override is visible only to the current thread, so cargo's other tests never touch this
/// ledger even when they call [`rc_dir`] at the same moment. The stack and the RAII guard restore
/// the outer override across nested calls and across a panic unwind. A caller that needs Tokio
/// must use a current-thread runtime; work that depends on this override cannot move to another
/// thread.
#[cfg(test)]
pub(crate) fn with_agit_home<T>(home: &std::path::Path, f: impl FnOnce() -> T) -> T {
    let depth = TEST_AGIT_HOME_STACK.with(|stack| stack.borrow().len());
    let serial = (depth == 0).then(test_agit_home_lock);
    TEST_AGIT_HOME_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        debug_assert_eq!(stack.len(), depth);
        stack.push(home.to_owned());
    });
    let _guard = TestAgitHomeGuard {
        depth,
        _serial: serial,
    };
    f()
}

#[cfg(test)]
mod ledger_tests {
    #[test]
    fn test_home_overrides_are_thread_local_nested_and_panic_safe() {
        let outer = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();

        assert!(super::test_agit_home_override().is_none());
        super::with_agit_home(outer.path(), || {
            assert_eq!(super::rc_dir().unwrap(), outer.path().join("rc"));
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    assert!(
                        super::test_agit_home_override().is_none(),
                        "an unrelated test thread must not inherit this override"
                    );
                });
            });

            let unwind = std::panic::catch_unwind(|| {
                super::with_agit_home(inner.path(), || {
                    assert_eq!(super::rc_dir().unwrap(), inner.path().join("rc"));
                    panic!("exercise nested override cleanup");
                });
            });
            assert!(unwind.is_err());
            assert_eq!(
                super::rc_dir().unwrap(),
                outer.path().join("rc"),
                "unwinding the nested scope restores the outer override"
            );
        });
        assert!(super::test_agit_home_override().is_none());
    }

    /// An unreadable ledger **says so** and leaves the evidence behind.
    ///
    /// An empty cache = a little less speed, recomputed on demand. An empty ledger = none of
    /// that ever happened — and the roster is what records "this session handed out a shell
    /// with no approval". A half-written file read back as an empty ledger brings that session
    /// back with a clean identity.
    #[test]
    fn a_corrupt_ledger_is_set_aside_rather_than_silently_overwritten() {
        let home = tempfile::tempdir().unwrap();
        super::with_agit_home(home.path(), || {
            let dir = super::rc_dir().unwrap();
            let path = dir.join("ledger-probe.json");
            std::fs::write(&path, "{ this is not json").unwrap();

            let got: super::Ledger<std::collections::BTreeMap<String, String>> =
                super::load_ledger("ledger-probe.json");
            assert!(
                matches!(got, super::Ledger::Unusable),
                "an unreadable ledger is unusable, not empty — the two degrade in opposite directions"
            );
            // A file that does not exist, on the other hand, genuinely is empty.
            assert!(matches!(
                super::load_ledger::<std::collections::BTreeMap<String, String>>(
                    "never-written.json"
                ),
                super::Ledger::Missing
            ));
            assert!(
                !path.exists(),
                "a corrupt file is moved aside; the next write must not overwrite the only evidence"
            );
            assert!(
                path.with_extension("json.corrupt").exists(),
                "the evidence stays alongside it"
            );
        });
    }
}
