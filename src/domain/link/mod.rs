//! Links: what stands for a session in the store.
//!
//! # The store keeps no copy
//!
//! The session file already lives in the runtime's own directory, and it does not disappear (a
//! spot check of `rollout_path` across the 18779 rows of the `threads` table — the newest 400 and
//! the oldest 200 — found 0 missing). Keeping a copy costs something real: 18858 sessions on this
//! machine come to 11.2 GB, and a copying implementation reads and writes the whole file every
//! time, 92 ms for a 36 MB session — paid once per turn of conversation.
//!
//! So a session in the store is one small JSON:
//!
//! ```text
//! ~/.agit/store/claude-code/db57fdab-....json
//! ```
//!
//! ```json
//! ```
//!
//! # Body fields; the runtime and the id are in the path
//!
//! Recording one thing in two places becomes inconsistent one day, and the path is the copy you
//! hold first when looking a session up — so the body does not repeat `runtime` and
//! `session_id`.
//!
//! `cwd` is the **source of truth** for "which project this belongs to": `agit log --here`
//! filters by repo, and the snapshot's `code` field and the naming suggestion both rely on it.
//! Partition slugs collide; cwd does not.
//!
//! `agent` is "which agent this session belongs to". `agit import -n <agent>` writes it in as it
//! records the session's initial version. It is also the **reverse index**: `agit commit <agent>` uses it to
//! get from an agent name back to the session, so no session id is needed afterwards.
//!
//! The only steps that write a link are the ones that bind content to a runtime:
//!
//! * `agit import` writes down the existence of an existing session as it adopts it (`agit hooks
//!   ingest` likewise, only without guessing the ownership).
//! * `agit resume` / `run` / `fork` write the ownership and the baseline at the moment they
//!   materialize into the runtime — lineage not recorded at install time is lost forever.
//! * `agit commit` fills in cwd, ownership and branch as it settles, so committing the same
//!   session again and again needs no session id.
//!
//! `agit clone` does **not** write one: it only fetches, it does not run (see
//! [`crate::commands::clone`]); materializing is `agit run`'s job.
//!
//! # One file per session
//!
//! Not a single `links.json`: writes can fire concurrently (two sessions opened at once), one
//! file per session drops the read-modify-write race, and file names cannot collide.

use crate::Result;
use crate::adapter;
use crate::domain::store::Store;
use crate::domain::turn;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One session link in the store.
///
/// `source` / `session_id` come from the file path and are never persisted (see the module
/// documentation).
#[derive(Debug, Clone)]
pub struct Link {
    /// The runtime: `codex` / `claude-code`.
    pub source: String,
    pub session_id: String,
    /// The working directory the session runs in.
    pub cwd: Option<String>,
    /// The agent name it belongs to. Absent before the first commit.
    pub agent: Option<String>,
    /// The namespace the agent sits in (your own name, or an organization). When absent it is
    /// filled in from the signed-in account — legacy links and repos under your own name are both
    /// this form; an organization repo must be recorded, or the next settlement goes looking for
    /// `<me>/<agent>`.
    pub owner: Option<String>,
    /// The branch this session holds. Absent before the first commit.
    ///
    /// Local evidence for the "one branch, one session" invariant: settle uses it to find the
    /// branch to advance.
    pub branch: Option<String>,
    /// The materialization baseline: the byte count of the live transcript generated at the
    /// moment resume/run/fork installs the VIEW into the runtime. Settlement reads only the bytes
    /// appended **after** the baseline; the baseline content is a materialized copy of history
    /// already in the repo (its ids have been reminted), so comparing it byte for byte against
    /// committed content is neither right nor possible.
    pub baseline_bytes: Option<u64>,
    /// SHA-256 of the baseline region: doctor verifies that "the live transcript has had no
    /// non-append write inside the baseline".
    pub baseline_hash: Option<String>,
    /// The user dismissed this unclaimed session from the naming inbox. This is presentation
    /// state, not ownership evidence; an ignored link is still unmanaged until it is claimed.
    pub naming_ignored: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The on-disk form: optional values are omitted rather than written as `null`, and default
/// boolean state is omitted so a missing key and an explicit false value have one representation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Body {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    baseline_hash: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    naming_ignored: bool,
}

impl Link {
    pub fn new(source: &str, session_id: &str, cwd: Option<&Path>) -> Link {
        Link {
            source: source.to_string(),
            session_id: session_id.to_string(),
            cwd: cwd.map(|p| p.to_string_lossy().to_string()),
            agent: None,
            owner: None,
            branch: None,
            baseline_bytes: None,
            baseline_hash: None,
            naming_ignored: false,
        }
    }

    fn body(&self) -> Body {
        Body {
            cwd: self.cwd.clone(),
            agent: self.agent.clone(),
            owner: self.owner.clone(),
            branch: self.branch.clone(),
            baseline_bytes: self.baseline_bytes,
            baseline_hash: self.baseline_hash.clone(),
            naming_ignored: self.naming_ignored,
        }
    }

    /// The on-disk JSON. Shared by the tests and `write`, so what you see is what is written.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&self.body())?)
    }

    /// Look up the real transcript file.
    pub fn resolve(&self) -> Option<PathBuf> {
        let ad = adapter::get(&self.source).ok()?;
        ad.resolve(&self.session_id, self.cwd.as_ref().map(Path::new))
    }

    /// Read the transcript (raw bytes).
    ///
    /// Bytes rather than `String`: the transcript's raw bytes go into the git blob unchanged (the
    /// root snapshot's session identity is computed from them too), and any encoding round trip
    /// can alter them.
    pub fn read_bytes(&self) -> Result<Vec<u8>> {
        let p = self.resolve().ok_or_else(|| {
            anyhow::anyhow!(
                "cannot find the transcript of session {} ({}).\n  \
                 The runtime may have deleted it, or this session comes from another machine.",
                short(&self.session_id),
                self.source
            )
        })?;
        std::fs::read(&p).with_context(|| format!("cannot read {}", p.display()))
    }

    /// Read the transcript.
    pub fn read(&self) -> Result<String> {
        let bytes = self.read_bytes()?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Parse into the intermediate representation (IR).
    pub fn parse(&self) -> Result<adapter::Session> {
        let text = self.read()?;
        // Identify the source runtime from the content rather than trusting the path — the file
        // may have been moved or renamed.
        let rt = adapter::infer_runtime(&text).unwrap_or(self.source.as_str());
        adapter::get(rt)?.parse(&text)
    }

    /// Compute the turn chain.
    pub fn chain(&self) -> Result<turn::Chain> {
        Ok(turn::chain_of(&self.parse()?))
    }
}

/// `<runtime>/<id>.json`
pub fn link_path(store: &Store, source: &str, session_id: &str) -> PathBuf {
    store.root().join(source).join(format!("{session_id}.json"))
}

/// Write one link.
/// Cross-process mutual exclusion for one link.
///
/// A writer's read-modify-write critical sections (import's claim snapshot and its restore on
/// failure, settlement advancing the watermark) all run under this lock, which is released along
/// with the returned handle. The lock file is the link's name plus a `.lock` suffix and its
/// contents mean nothing; [`list`] accepts only `.json`, so it never takes it for a link.
pub fn lock(store: &Store, source: &str, session_id: &str) -> Result<std::fs::File> {
    use fs2::FileExt as _;
    let dir = store.root().join(source);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let lp = dir.join(format!("{session_id}.json.lock"));
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lp)
        .with_context(|| format!("cannot open {}", lp.display()))?;
    f.lock_exclusive()
        .with_context(|| format!("cannot lock {}", lp.display()))?;
    Ok(f)
}

pub fn write(store: &Store, link: &Link) -> Result<PathBuf> {
    let dir = store.root().join(&link.source);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let lp = link_path(store, &link.source, &link.session_id);
    std::fs::write(&lp, format!("{}\n", link.to_json()?))
        .with_context(|| format!("cannot write {}", lp.display()))?;
    Ok(lp)
}

/// Read one link.
///
/// `runtime` comes from the parent directory name and `session_id` from the file name, so the
/// path is where those two fields come from.
pub fn read(path: &Path) -> Option<Link> {
    let body: Body = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let source = path.parent()?.file_name()?.to_str()?.to_string();
    let session_id = path.file_stem()?.to_str()?.to_string();
    Some(Link {
        source,
        session_id,
        cwd: body.cwd,
        agent: body.agent,
        owner: body.owner,
        branch: body.branch,
        baseline_bytes: body.baseline_bytes,
        baseline_hash: body.baseline_hash,
        naming_ignored: body.naming_ignored,
    })
}

/// List every link in the store.
///
/// Accepts only `<registered runtime>/<id>.json`. Anything lying directly in the store root (the
/// allowlist file, an editor's temporary file) is not taken for a link.
pub fn list(store: &Store) -> Vec<Link> {
    let root = store.root();
    if !root.exists() {
        return vec![];
    }
    let mut out = vec![];
    for e in walkdir::WalkDir::new(root)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let in_runtime_dir = p
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|x| x.to_str())
            .is_some_and(|rt| adapter::RUNTIMES.contains(&rt));
        if !in_runtime_dir {
            continue;
        }
        if let Some(l) = read(p) {
            out.push(l);
        }
    }
    out.sort_by(|a, b| (&a.source, &a.session_id).cmp(&(&b.source, &b.session_id)));
    out
}

/// When a link was last updated.
///
/// Takes the link file's time and not the transcript's: the transcript's needs the file looked up
/// first (one glob each). And the link's mtime answers exactly the question the user is asking —
/// "which one did I adopt or commit most recently".
pub fn touched_at(store: &Store, link: &Link) -> std::time::SystemTime {
    std::fs::metadata(link_path(store, &link.source, &link.session_id))
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// The link touched most recently.
///
/// Used by the commands that still take the global-latest strategy. It must pick by time and not
/// take the first of the list: the list is sorted by (runtime, id), so taking the first picks by
/// lexicographic uuid order, which has nothing to do with "most recent".
pub fn latest(store: &Store) -> Option<Link> {
    list(store).into_iter().max_by_key(|l| touched_at(store, l))
}

/// Read one specific link from the store (None when it does not exist).
///
/// Used when re-importing an already adopted session: fill in the **existing** one instead of
/// making a new one. This is the one that cannot be filled in after the fact.
pub fn get(store: &Store, source: &str, session_id: &str) -> Option<Link> {
    read(&link_path(store, source, session_id))
}

/// Keep an unclaimed session out of the naming inbox.
///
/// The read-modify-write is locked because a stale screen action can race with a runtime hook or
/// an import. A session that became managed while the screen was open wins that race and is left
/// unchanged; ownership must never acquire presentation state from an obsolete row.
pub fn dismiss_naming(
    store: &Store,
    source: &str,
    session_id: &str,
    cwd: Option<&Path>,
) -> Result<Link> {
    let _guard = lock(store, source, session_id)?;
    let mut link =
        get(store, source, session_id).unwrap_or_else(|| Link::new(source, session_id, cwd));
    if is_managed(&link) {
        return Ok(link);
    }
    if link.cwd.is_none() {
        link.cwd = cwd.map(|p| p.to_string_lossy().to_string());
    }
    link.naming_ignored = true;
    write(store, &link)?;
    Ok(link)
}

/// Whether there is enough ownership evidence to treat the session as managed by AgentGit.
///
/// A link that carries only a hook pre-registration or `--link-only` still has no agent, no
/// branch and no baseline; it has no recoverable version identity yet, so the current
/// conversation stays under `new`'s protection.
pub fn is_managed(link: &Link) -> bool {
    link.agent.is_some() || link.branch.is_some() || link.baseline_bytes.is_some()
}

/// One session recorded under an agent name.
///
/// The same memory can be materialized into more than one runtime (`agit resume --as` carries on
/// in another harness), so "one agent to many sessions" is a normal state, not an anomaly.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub link: Link,
    /// The transcript file is newer than the link file — this session has had new content since
    /// the last import / commit.
    ///
    /// The test uses two `stat` calls and **never opens the transcript**. It holds because the
    /// write order on both sides is fixed: import and commit both write the link only after
    /// reading the transcript, so "the link is newer" means "the content as of that moment is
    /// already recorded"; whatever the transcript grows after that carries its mtime past the
    /// link's.
    pub touched: bool,
}

/// List the sessions recorded under an agent name.
///
/// Reads the link files and the transcripts' mtimes only; parses no content.
pub fn for_agent(store: &Store, agent: &str) -> Vec<Candidate> {
    list(store)
        .into_iter()
        .filter(|l| l.agent.as_deref() == Some(agent))
        .map(|l| {
            let link_at = touched_at(store, &l);
            let touched = l
                .resolve()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
                .is_some_and(|t| t > link_at);
            Candidate { link: l, touched }
        })
        .collect()
}

/// The unique candidate, None when there is none (the caller shows the candidate list to the
/// user).
///
/// Two tests, each demanding a unique answer:
///
/// 1. There is only one candidate — nothing is left to choose.
/// 2. Only one of several candidates is "touched". Once the same memory is materialized into two
///    runtimes you work in one of them only, and the other's transcript stops at the moment of
///    install; "which one has new content" is then a fact, not a guess.
///
/// Neither holds and it returns None: **never** pick one among sessions that were all touched (or
/// none touched) — picking wrong records a stretch of work into another lineage, and is not
/// noticed right away.
pub fn only_one(cands: &[Candidate]) -> Option<&Candidate> {
    if let [only] = cands {
        return Some(only);
    }
    let mut touched = cands.iter().filter(|c| c.touched);
    match (touched.next(), touched.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Find a link by id or prefix.
///
/// An ambiguous prefix **must error** instead of taking the first — taking the wrong session
/// leaves the user carrying on in a completely unrelated context, and is not noticed right away.
pub fn find(store: &Store, selector: &str) -> Result<Link> {
    let sel = selector.trim();
    if sel.is_empty() {
        bail!("session selector must not be empty");
    }
    let matches: Vec<Link> = list(store)
        .into_iter()
        .filter(|l| l.session_id.starts_with(sel))
        .collect();

    match matches.len() {
        0 => bail!(
            "no session `{sel}` in the store.\n  \
             `agit import {sel} -n <agent-name>` links it and records a first version, or `agit log` lists what you have."
        ),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let ids: Vec<String> = matches
                .iter()
                .take(6)
                .map(|l| short(&l.session_id))
                .collect();
            bail!(
                "`{sel}` matches {n} sessions; give a longer prefix: {}",
                ids.join(", ")
            )
        }
    }
}

/// The short form of an id (for display).
pub fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("store");
        std::fs::create_dir_all(&root).unwrap();
        let s = Store::at(&root);
        (d, s)
    }

    /// `runtime` / `session_id` stay in the path; writing them down again adds one more place that
    /// can disagree.
    #[test]
    fn body_omits_path_identity_fields() {
        let (_d, s) = store();
        let mut l = Link::new("codex", "AB", Some(Path::new("/repo/one")));
        l.agent = Some("photo".into());
        let p = write(&s, &l).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();

        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        // `Value` is backed by a BTreeMap, so this compares the set, not the order.
        assert_eq!(keys, vec!["agent", "cwd"]);
        for absent in ["runtime", "source", "session_id"] {
            assert!(
                v.get(absent).is_none(),
                "{absent} belongs in the path, not in the body"
            );
        }
    }

    #[test]
    fn optional_fields_are_omitted_not_null() {
        // A newly adopted session has no ownership and no lineage yet. Serializing must not
        // emit `"agent": null`.
        let l = Link::new("claude-code", "CD", None);
        let j = l.to_json().unwrap();
        assert_eq!(j, "{}", "a link that knows nothing is an empty object: {j}");
        assert!(!j.contains("null"));
    }

    #[test]
    fn naming_dismissal_is_backward_compatible_and_persistent() {
        let (_d, s) = store();
        let path = link_path(&s, "codex", "AB");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();

        let old = read(&path).unwrap();
        assert!(!old.naming_ignored, "an absent field means not dismissed");

        let dismissed = dismiss_naming(&s, "codex", "AB", Some(Path::new("/repo"))).unwrap();
        assert!(dismissed.naming_ignored);
        assert_eq!(dismissed.cwd.as_deref(), Some("/repo"));
        let saved = read(&path).unwrap();
        assert!(saved.naming_ignored);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("\"naming_ignored\": true")
        );
    }

    #[test]
    fn only_a_claimed_link_is_managed() {
        let mut link = Link::new("codex", "s", None);
        assert!(!is_managed(&link));
        link.naming_ignored = true;
        assert!(!is_managed(&link), "a dismissal is not an adoption");

        let mut claimed = link;
        claimed.agent = Some("paper".into());
        assert!(is_managed(&claimed));
    }

    #[test]
    fn a_stale_dismissal_does_not_mark_a_managed_session() {
        let (_d, s) = store();
        let mut claimed = Link::new("codex", "AB", Some(Path::new("/repo")));
        claimed.agent = Some("photo".into());
        claimed.branch = Some("work".into());
        write(&s, &claimed).unwrap();

        let result = dismiss_naming(&s, "codex", "AB", Some(Path::new("/other"))).unwrap();
        assert!(!result.naming_ignored);
        assert_eq!(result.cwd.as_deref(), Some("/repo"));
        assert!(!get(&s, "codex", "AB").unwrap().naming_ignored);
    }

    #[test]
    fn runtime_and_id_come_back_from_the_path() {
        let (_d, s) = store();
        let l = Link::new("claude-code", "db57fdab-1234", Some(Path::new("/r")));
        write(&s, &l).unwrap();

        let back = read(&link_path(&s, "claude-code", "db57fdab-1234")).unwrap();
        assert_eq!(back.source, "claude-code");
        assert_eq!(back.session_id, "db57fdab-1234");
        assert_eq!(back.cwd.as_deref(), Some("/r"));
        assert!(back.agent.is_none());
    }

    #[test]
    fn links_from_both_runtimes_coexist() {
        let (_d, s) = store();
        write(&s, &Link::new("codex", "AB", None)).unwrap();
        write(&s, &Link::new("claude-code", "CD", None)).unwrap();
        assert_eq!(list(&s).len(), 2);
        // Links with the same id under different runtimes do not overwrite each other (the path
        // carries the runtime).
        write(&s, &Link::new("codex", "SAME", None)).unwrap();
        write(&s, &Link::new("claude-code", "SAME", None)).unwrap();
        assert_eq!(list(&s).len(), 4);
    }

    #[test]
    fn stray_files_are_not_links() {
        // Other things can sit in the store root (the allowlist, an editor's temporary file) and
        // must not be taken for links.
        let (_d, s) = store();
        write(&s, &Link::new("codex", "AB", None)).unwrap();
        std::fs::write(s.root().join("credentials.json"), "{}").unwrap();
        std::fs::create_dir_all(s.root().join("not-a-runtime")).unwrap();
        std::fs::write(s.root().join("not-a-runtime").join("X.json"), "{}").unwrap();
        let all = list(&s);
        assert_eq!(all.len(), 1, "only <runtime>/<id>.json counts");
        assert_eq!(all[0].session_id, "AB");
    }

    /// "Use the most recent one when the session argument is omitted" must really pick by time.
    ///
    /// The list is sorted by (runtime, id), so taking the first picks by lexicographic uuid order
    /// — which has nothing to do with "most recent", and "most recent" is what `agit show` /
    /// `agit resume` say in their help.
    #[test]
    fn latest_is_by_time_not_by_id_order() {
        let (_d, s) = store();
        // `zzz` is written first and `aaa` second: lexicographic order is the reverse of time.
        write(&s, &Link::new("codex", "zzz-old", None)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&s, &Link::new("codex", "aaa-new", None)).unwrap();

        assert_eq!(
            list(&s)[0].session_id,
            "aaa-new",
            "the list is sorted by id"
        );
        assert_eq!(
            latest(&s).unwrap().session_id,
            "aaa-new",
            "the two agree here; the test below is the real one"
        );

        // Touch the old one again (the equivalent of committing to it).
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut old = read(&link_path(&s, "codex", "zzz-old")).unwrap();
        old.agent = Some("photo".into());
        write(&s, &old).unwrap();
        assert_eq!(
            list(&s)[0].session_id,
            "aaa-new",
            "the list order is unaffected"
        );
        assert_eq!(
            latest(&s).unwrap().session_id,
            "zzz-old",
            "the latest is the one just touched"
        );
    }

    #[test]
    fn ambiguous_prefix_errors_instead_of_guessing() {
        let (_d, s) = store();
        write(&s, &Link::new("codex", "abc111", None)).unwrap();
        write(&s, &Link::new("codex", "abc222", None)).unwrap();
        let e = find(&s, "abc").unwrap_err().to_string();
        assert!(
            e.contains("matches 2 sessions"),
            "an ambiguous prefix must be reported: {e}"
        );
        assert_eq!(find(&s, "abc1").unwrap().session_id, "abc111");
    }

    #[test]
    fn missing_session_suggests_import() {
        let (_d, s) = store();
        let e = find(&s, "nope").unwrap_err().to_string();
        assert!(
            e.contains("agit import"),
            "the error must give the next step: {e}"
        );
        assert!(e.contains("-n"), "the next step carries an agent name: {e}");
    }

    fn cand(id: &str, touched: bool) -> Candidate {
        let mut l = Link::new("codex", id, None);
        l.agent = Some("photo".into());
        Candidate { link: l, touched }
    }

    /// The reverse lookup behind `agit commit <agent>`: agent name → session.
    #[test]
    fn for_agent_is_the_reverse_index() {
        let (_d, s) = store();
        let mut mine = Link::new("codex", "AB", None);
        mine.agent = Some("photo".into());
        write(&s, &mine).unwrap();
        let mut other = Link::new("codex", "CD", None);
        other.agent = Some("recon".into());
        write(&s, &other).unwrap();
        // A session with no version recorded yet belongs to no agent.
        write(&s, &Link::new("claude-code", "EF", None)).unwrap();

        let got = for_agent(&s, "photo");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].link.session_id, "AB");
        assert!(for_agent(&s, "nobody").is_empty());
        // A missing transcript (the test environment has no real runtime directory) counts as
        // "not touched".
        assert!(!got[0].touched);
    }

    /// A single candidate goes straight through — asking a question with one possible answer
    /// wastes the user's time.
    #[test]
    fn a_single_candidate_needs_no_disambiguation() {
        let one = [cand("AB", false)];
        assert_eq!(only_one(&one).unwrap().link.session_id, "AB");
        assert!(only_one(&[]).is_none());
    }

    /// Once one memory is installed into two runtimes, "which one has new content" is a fact,
    /// not a guess.
    ///
    /// You work in one of them only; the other's transcript stops at the moment of install.
    #[test]
    fn two_runtimes_from_one_install_resolve_to_the_one_worked_in() {
        let both = [cand("codex-side", false), cand("cc-side", true)];
        assert_eq!(only_one(&both).unwrap().link.session_id, "cc-side");
    }

    /// With both touched (or neither), the user has to say which.
    #[test]
    fn genuinely_ambiguous_candidates_are_refused() {
        assert!(
            only_one(&[cand("AB", true), cand("CD", true)]).is_none(),
            "picking one while both have new content records the work into another lineage"
        );
        assert!(
            only_one(&[cand("AB", false), cand("CD", false)]).is_none(),
            "with neither touched there is equally nothing to go on"
        );
    }

    /// Re-importing an already adopted session must not erase the lineage.
    ///
    /// Regression test for a real bug: building a brand-new `Link` on every `import` drops
    /// whatever the existing one holds.
    #[test]
    fn re_reading_an_existing_link_preserves_lineage() {
        let (_d, s) = store();
        let mut l = Link::new("codex", "AB", Some(Path::new("/r")));
        l.agent = Some("photo".into());
        write(&s, &l).unwrap();

        let back = get(&s, "codex", "AB").expect("an adopted session must read back");
        assert_eq!(back.agent.as_deref(), Some("photo"));
        assert!(get(&s, "codex", "NOPE").is_none());
    }

    #[test]
    fn rewriting_a_link_is_idempotent() {
        // Importing the same session twice is the same path with the same content, a no-op.
        let (_d, s) = store();
        let l = Link::new("codex", "AB", Some(Path::new("/r")));
        let p1 = write(&s, &l).unwrap();
        let c1 = std::fs::read_to_string(&p1).unwrap();
        let p2 = write(&s, &l).unwrap();
        assert_eq!(p1, p2);
        assert_eq!(c1, std::fs::read_to_string(&p2).unwrap());
    }
}
