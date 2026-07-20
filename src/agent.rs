//! Agent identity, the local registry, binding, and resolution (design §3/§4).
//!
//! An agent is a **memory**: a git repo of transcripts, named for what it knows (`frontend`,
//! `payments-api`), not for a person and not for a repo. Three consequences shape everything here:
//!
//! * **Identity is the `aid`** (`agt_<uuid>`), minted once and committed into the store's own
//!   `agent.toml`. It is *not* the URL — a URL is a locator (`git@…` and `https://…` are one agent,
//!   and you mint an agent *before* any remote exists). It is *not* the name — a name is a mutable
//!   label that collides.
//! * **The store is keyed by the aid** — `$AGIT_HOME/agents/<aid>/` — so rename and publish are
//!   metadata edits: no directory ever moves, and a running watcher is never orphaned.
//! * **`registry.json` is a cache, never a truth.** Each store's `agent.toml` owns the fact; the
//!   registry is name→aid for lookup, and is rebuildable by scanning (`agit a list --repair`).
//!
//! Resolution (§4) is `--agent` → `$AGIT_AGENT` → the active pointer → `.agit.toml [defaults]` →
//! an actionable error. Never a silent fallback.

use crate::convo;
use crate::hub::identity::{is_aid, parse_agent_toml, toml_string, Identity};
use crate::scope;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The binding: COMMITTED at the code-repo root. This is what makes collaboration work — a fresh
/// clone learns which agents this repo works with, and which one to activate.
pub const BINDING_FILE: &str = ".agit.toml";
const BINDING_VERSION: u32 = 1;

/// The active pointer, relative to `git rev-parse --git-path`: **per-worktree** and local (this
/// machine has 231 worktrees of one repo; a shared pointer would make them fight). Living inside
/// `.git` it is untracked by construction, and its absence is fully recoverable — delete it and
/// resolution falls back to `[defaults]`. That recoverability is the whole difference from the
/// `.agit/store` pointer this design deletes.
const ACTIVE_PTR: &str = "agit/active";

const REGISTRY_FILE: &str = "registry.json";
const REGISTRY_VERSION: u32 = 1;

/// A resolved agent. `store` is always `$AGIT_HOME/agents/<aid>/`; `remote` is read from the store,
/// not from the binding — the binding's URL is a hint for a fresh clone, the store's origin is fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub aid: String,
    pub name: String,
    pub store: PathBuf,
    pub remote: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Identity — the store's own agent.toml
// ---------------------------------------------------------------------------------------------

/// Mint a new aid. Minted **once**, at `agit a init`, before any remote exists.
pub fn mint_aid() -> String {
    format!("agt_{}", convo::fresh_id("agent-identity"))
}

/// What a store's `agent.toml` says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreIdentity {
    pub aid: String,
    pub name: String,
    pub created: String,
}

/// Read a store's identity. Refuses a store whose `agent.toml` carries no `agt_…` id (including the
/// shared `unnamed-agent` placeholder). Reuses the hub's parser (`hub::identity`) rather than growing
/// a second opinion.
pub fn read_identity(store: &Path) -> Result<StoreIdentity> {
    let p = store.join("agent.toml");
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("{} has no agent.toml — it is not an agent store", store.display()))?;
    let aid = match parse_agent_toml(&text) {
        Identity::Aid(a) => a,
        Identity::Unidentified => bail!(
            "{} has no `agt_…` id in agent.toml — it is not an identified agent store.",
            p.display()
        ),
    };
    Ok(StoreIdentity {
        // A nameless store is still an agent: identity does not depend on the label.
        name: toml_string(&text, Some("agent"), "name").unwrap_or_else(|| aid.clone()),
        created: toml_string(&text, Some("agent"), "created").unwrap_or_default(),
        aid,
    })
}

fn write_agent_toml(store: &Path, id: &StoreIdentity) -> Result<()> {
    check_toml_value("name", &id.name)?;
    std::fs::write(
        store.join("agent.toml"),
        format!(
            "# Agent identity — committed, so the aid travels with the store's history.\n\
             # The id is minted once and never changes; the name is a label and may be renamed.\n\
             [agent]\n\
             id      = \"{}\"\n\
             name    = \"{}\"\n\
             created = \"{}\"\n",
            id.aid, id.name, id.created
        ),
    )
    .with_context(|| format!("failed to write {}/agent.toml", store.display()))
}

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Names are labels, but they land in TOML, in paths printed to users, and in shell suggestions.
///
/// A leading `.` or `~` is refused because `looks_like_url` reads either as a path: an agent named
/// `.foo` mints fine and is then **untrackable by name** — `agit a clone .foo` treats it as a local
/// path and refuses it as an unclassifiable remote. A name a teammate can never track is not a name.
/// A leading `-` is refused for the same class of reason: git would read it as a flag.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '.', '~'])
        // `..` is banned so a locally-minted name is always hostable: the hub reads names into URL paths
        // and refuses `..`, and a name you can mint but can never publish is a trap. `payments.api` (one
        // dot) stays fine. Keep this in step with `hub::net::valid_agent_name`.
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        bail!(
            "`{name}` is not a usable agent name (letters, digits, `-`, `_`, `.`; max 64; \
             no `..`; not starting with `-`, `.` or `~`)"
        );
    }
    if is_aid(name) {
        bail!("`{name}` looks like an aid; a name must be a label, or `agit a switch {name}` becomes ambiguous");
    }
    Ok(())
}

/// The same rule as `validate_name`, asked rather than enforced — so a prompt can check an answer, and
/// offer a suggestion, without a second opinion about what a name is.
pub fn is_usable_name(name: &str) -> bool {
    validate_name(name).is_ok()
}

/// TOML here is written by hand (no toml crate — see `hub::identity`), so a value that would need
/// escaping is refused at the door rather than silently producing a file that no longer parses.
fn check_toml_value(what: &str, v: &str) -> Result<()> {
    if v.contains('"') || v.contains('\\') || v.chars().any(|c| c.is_control()) {
        bail!("{what} `{v}` contains characters agit will not write into {BINDING_FILE}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Store paths — id-keyed, so nothing ever moves
// ---------------------------------------------------------------------------------------------

fn agents_dir_in(home: &Path) -> PathBuf {
    home.join("agents")
}


fn store_remote(store: &Path) -> Option<String> {
    match scope::git_in_status(store, &["remote", "get-url", "origin"]) {
        (0, url) if !url.trim().is_empty() => Some(url.trim().to_string()),
        _ => None,
    }
}

/// Every git remote configured on the store, as `(name, url)` in git's own listing order (which is
/// alphabetical). Empty URLs are skipped. This is the multi-remote counterpart to `store_remote` — the
/// push fan-out and the binding sync both enumerate remotes through here.
pub fn store_remotes(store: &Path) -> Vec<(String, String)> {
    let (code, names) = scope::git_in_status(store, &["remote"]);
    if code != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
        let (c, url) = scope::git_in_status(store, &["remote", "get-url", name]);
        let url = url.trim();
        if c == 0 && !url.is_empty() {
            out.push((name.to_string(), url.to_string()));
        }
    }
    out
}

/// The name of the store's primary remote — its identity anchor. `origin` if present (the anchor
/// `clone`/`pull` track by construction), otherwise the first remote in git's alphabetical order.
/// None only when the store has no remotes at all.
pub fn primary_remote_name(store: &Path) -> Option<String> {
    let remotes = store_remotes(store);
    if remotes.iter().any(|(n, _)| n == "origin") {
        Some("origin".to_string())
    } else {
        remotes.first().map(|(n, _)| n.clone())
    }
}

fn agent_at(store: PathBuf, id: StoreIdentity) -> Agent {
    Agent {
        remote: store_remote(&store),
        aid: id.aid,
        name: id.name,
        store,
    }
}

// ---------------------------------------------------------------------------------------------
// Registry — a rebuildable cache
// ---------------------------------------------------------------------------------------------

fn registry_path_in(home: &Path) -> PathBuf {
    home.join(REGISTRY_FILE)
}


/// Load name→aid. Missing or corrupt reads as empty: this is a cache, and a cache that errors is
/// worse than one that misses — every lookup falls back to scanning the stores, which are the truth.
fn registry_load(home: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(registry_path_in(home)) else {
        return BTreeMap::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(m) = v.get("agents").and_then(|a| a.as_object()) {
        for (k, val) in m {
            if let Some(aid) = val.as_str().filter(|a| is_aid(a)) {
                out.insert(k.clone(), aid.to_string());
            }
        }
    }
    out
}

fn registry_save(home: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let body = serde_json::json!({ "version": REGISTRY_VERSION, "agents": map });
    write_atomic(
        &registry_path_in(home),
        &format!("{}\n", serde_json::to_string_pretty(&body)?),
    )
}

fn registry_put(home: &Path, name: &str, aid: &str) -> Result<()> {
    let mut m = registry_load(home);
    if m.get(name).map(String::as_str) == Some(aid) {
        return Ok(());
    }
    m.insert(name.to_string(), aid.to_string());
    registry_save(home, &m)
}

/// Rewrite the cache from the stores themselves — `agit a list --repair`.
pub fn repair() -> Result<Vec<Agent>> {
    let home = scope::agit_home()?;
    repair_in(&home)
}

fn repair_in(home: &Path) -> Result<Vec<Agent>> {
    let agents = list_in(home)?;
    let mut m = BTreeMap::new();
    for a in &agents {
        // Names collide by design (they are labels). First one by name-then-aid order wins the cache
        // entry; the loser is still reachable by aid, and `list` reports both.
        m.entry(a.name.clone()).or_insert_with(|| a.aid.clone());
    }
    registry_save(home, &m)?;
    Ok(agents)
}

fn write_atomic(path: &Path, body: &str) -> Result<()> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body).with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to write {}", path.display()))
}

// ---------------------------------------------------------------------------------------------
// The active pointer — local, per-worktree, recoverable
// ---------------------------------------------------------------------------------------------

/// `git rev-parse --git-path agit/active`, per worktree. git answers relative to its cwd, so it is
/// asked from `env_root` and joined back onto it.
pub fn active_path(env_root: &Path) -> Result<PathBuf> {
    let p = scope::git_in(env_root, &["rev-parse", "--git-path", ACTIVE_PTR])?;
    let p = PathBuf::from(p.trim());
    Ok(if p.is_absolute() { p } else { env_root.join(p) })
}

/// The active selector, or None. Missing **and** blank both read as None: absence must fall back to
/// `[defaults]`, never error.
pub fn read_active(env_root: &Path) -> Result<Option<String>> {
    let p = active_path(env_root)?;
    Ok(match std::fs::read_to_string(p) {
        Ok(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Err(_) => None,
    })
}

/// Record the aid — not the name — so a later rename does not orphan the pointer.
pub fn write_active(env_root: &Path, aid: &str) -> Result<()> {
    let p = active_path(env_root)?;
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    write_atomic(&p, &format!("{aid}\n"))
}

pub fn clear_active(env_root: &Path) -> Result<()> {
    let p = active_path(env_root)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("failed to remove {}", p.display())),
    }
}

// ---------------------------------------------------------------------------------------------
// Binding — .agit.toml, committed at the code-repo root
// ---------------------------------------------------------------------------------------------

/// One named remote recorded next to an agent in the committed binding. A store can push to several
/// remotes (a shared team repo, a personal central hub); exactly one is the `primary` — the identity
/// anchor that `clone`/`pull` resolve from. The others are additional read locators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundRemote {
    pub name: String,
    pub url: String,
    pub primary: bool,
}

/// One `[[agent]]` entry. `id` is the **integrity check**: if the store behind the primary remote
/// carries a different aid, agit refuses rather than binding you to a different agent wearing the same
/// name. `remotes` is list-aware: zero, one, or many, with exactly one primary whenever any exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAgent {
    pub id: String,
    pub name: String,
    pub remotes: Vec<BoundRemote>,
}

impl BoundAgent {
    /// The identity anchor: the remote marked primary, else the first (a hand-edited file with none
    /// marked still resolves deterministically).
    pub fn primary(&self) -> Option<&BoundRemote> {
        self.remotes.iter().find(|r| r.primary).or_else(|| self.remotes.first())
    }

    /// The primary remote's URL — what a fresh clone resolves a bare name to.
    pub fn primary_url(&self) -> Option<&str> {
        self.primary().map(|r| r.url.as_str())
    }

    /// Build a single-remote entry from an optional URL, recording it as the sole primary `origin`.
    /// This is the clone/init/rebind path: they only ever record the store's own origin.
    fn single(id: String, name: String, url: Option<String>) -> BoundAgent {
        let remotes = url
            .into_iter()
            .map(|u| BoundRemote { name: "origin".into(), url: u, primary: true })
            .collect();
        BoundAgent { id, name, remotes }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub version: u32,
    pub agents: Vec<BoundAgent>,
    /// `[defaults] agent` — what a FRESH clone activates, not what you have active.
    pub default: Option<String>,
}

impl Default for Binding {
    fn default() -> Self {
        Binding { version: BINDING_VERSION, agents: Vec::new(), default: None }
    }
}

impl Binding {
    pub fn load(env_root: &Path) -> Result<Option<Binding>> {
        let p = env_root.join(BINDING_FILE);
        match std::fs::read_to_string(&p) {
            Ok(t) => Ok(Some(
                Binding::parse(&t).with_context(|| format!("failed to read {}", p.display()))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", p.display())),
        }
    }

    pub fn save(&self, env_root: &Path) -> Result<()> {
        for a in &self.agents {
            check_toml_value("name", &a.name)?;
            for r in &a.remotes {
                check_toml_value("remote name", &r.name)?;
                check_toml_value("remote url", &r.url)?;
            }
        }
        if let Some(d) = &self.default {
            check_toml_value("default agent", d)?;
        }
        write_atomic(&env_root.join(BINDING_FILE), &self.to_toml())
    }

    /// Only the schema of §4 is recognized. An `id` that is not an aid is refused: an integrity
    /// check that cannot check is worse than none.
    pub fn parse(text: &str) -> Result<Binding> {
        let mut b = Binding { version: BINDING_VERSION, agents: Vec::new(), default: None };
        let mut section = String::new();
        // Whether the CURRENT agent has opened any `[[agent.remote]]` sub-table yet. Reset on each new
        // `[[agent]]`. The first sub-table means the sub-tables are authoritative and any remote already
        // collected from the redundant forward-compat `remote = "…"` line must be dropped.
        let mut saw_subtable_for_current = false;
        for line in text.lines() {
            let line = crate::hub::identity::strip_comment(line).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                let inner = inner.trim();
                if let Some(arr) = inner.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    section = arr.trim().to_string();
                    if section == "agent" {
                        b.agents.push(BoundAgent { id: String::new(), name: String::new(), remotes: Vec::new() });
                        saw_subtable_for_current = false;
                    } else if section == "agent.remote" {
                        // A remote sub-table belongs to the agent opened just above. A malformed file
                        // with a `[[agent.remote]]` before any `[[agent]]` has nowhere to hang it —
                        // skip, matching the None-guards below.
                        if let Some(agent) = b.agents.last_mut() {
                            if !saw_subtable_for_current {
                                // First sub-table for this agent: the sub-tables are authoritative, so any
                                // remote already collected came from the redundant forward-compat
                                // `remote = "<primary-url>"` line (a duplicate of the primary). Drop it so
                                // it does not survive as a phantom `origin` remote.
                                agent.remotes.clear();
                                saw_subtable_for_current = true;
                            }
                            agent.remotes.push(BoundRemote { name: String::new(), url: String::new(), primary: false });
                        }
                    }
                } else {
                    section = inner.to_string();
                }
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match (section.as_str(), k) {
                ("", "version") => {
                    b.version = v
                        .parse()
                        .with_context(|| format!("{BINDING_FILE}: version must be a number, got `{v}`"))?
                }
                ("agent", _) => {
                    let Some(cur) = b.agents.last_mut() else { continue };
                    let Some(v) = unquote(v) else { continue };
                    match k {
                        "id" => cur.id = v,
                        "name" => cur.name = v,
                        // The LEGACY single-remote line: `remote = "<url>"` under [[agent]] becomes one
                        // primary origin remote, so old files parse unchanged. An empty value falls
                        // through to `_` (no remote), matching the old `remote: None` behavior.
                        "remote" if !v.is_empty() => {
                            cur.remotes.push(BoundRemote { name: "origin".into(), url: v, primary: true });
                        }
                        _ => {}
                    }
                }
                ("agent.remote", _) => {
                    let Some(agent) = b.agents.last_mut() else { continue };
                    let Some(r) = agent.remotes.last_mut() else { continue };
                    match k {
                        "name" => {
                            if let Some(v) = unquote(v) {
                                r.name = v;
                            }
                        }
                        "url" => {
                            if let Some(v) = unquote(v) {
                                r.url = v;
                            }
                        }
                        // A bare bool: `primary = true` (unquote returns None for it, by design).
                        "primary" => r.primary = v.trim() == "true",
                        _ => {}
                    }
                }
                ("defaults", "agent") => b.default = unquote(v).filter(|s| !s.is_empty()),
                _ => {}
            }
        }
        if b.version > BINDING_VERSION {
            bail!("{BINDING_FILE} is version {} — this agit understands {BINDING_VERSION}. Upgrade agit.", b.version);
        }
        // Normalize each agent's remotes so downstream code can assume the invariants — even for a
        // hand-edited file: drop empty-url entries, dedupe by name (last wins), and guarantee exactly
        // one primary whenever at least one remote exists.
        for a in &mut b.agents {
            a.remotes.retain(|r| !r.url.is_empty());
            let mut deduped: Vec<BoundRemote> = Vec::new();
            for r in a.remotes.drain(..) {
                match deduped.iter_mut().find(|e| e.name == r.name) {
                    Some(existing) => *existing = r, // last wins, keeping first-seen position
                    None => deduped.push(r),
                }
            }
            a.remotes = deduped;
            let mut have_primary = false;
            for r in &mut a.remotes {
                if r.primary {
                    if have_primary {
                        r.primary = false; // keep only the first primary
                    } else {
                        have_primary = true;
                    }
                }
            }
            if !have_primary {
                if let Some(first) = a.remotes.first_mut() {
                    first.primary = true;
                }
            }
        }
        for a in &b.agents {
            if !is_aid(&a.id) {
                bail!(
                    "{BINDING_FILE}: [[agent]] {} has id `{}`, which is not an `agt_…` identity.\n\
                     \x20      The id is what stops a recreated remote silently binding you to a different agent.",
                    if a.name.is_empty() { "<unnamed>" } else { &a.name },
                    a.id
                );
            }
        }
        Ok(b)
    }

    pub fn to_toml(&self) -> String {
        let mut s = String::from(
            "# agit binding — COMMITTED. Which agents this repo works with.\n\
             # The id is the identity (a remote is only a locator); the name is a label.\n",
        );
        s.push_str(&format!("version = {}\n", self.version));
        for a in &self.agents {
            s.push_str("\n[[agent]]\n");
            s.push_str(&format!("id     = \"{}\"\n", a.id));
            s.push_str(&format!("name   = \"{}\"\n", a.name));
            // Three rules, chosen so a single-remote binding stays byte-identical to what agit wrote
            // before multi-remote existed (old files round-trip, old agit still reads them):
            //   0 remotes                -> emit nothing.
            //   exactly 1 named "origin" -> the LEGACY `remote = "<url>"` line.
            //   otherwise                -> the LEGACY `remote = "<primary-url>"` line for forward-compat
            //                               PLUS one [[agent.remote]] sub-table each, primary on the anchor.
            match a.remotes.as_slice() {
                [] => {}
                [r] if r.name == "origin" => {
                    s.push_str(&format!("remote = \"{}\"\n", r.url));
                }
                remotes => {
                    // Forward-compat: also emit the primary URL as the legacy `remote = "…"` line, so an
                    // OLDER agit — which reads only that line and would otherwise see NO remote and drop
                    // them all on the next rewrite — still finds the primary. A current agit parses the
                    // sub-tables (authoritative) and treats this line as redundant (see `parse`).
                    if let Some(p) = a.primary() {
                        s.push_str(&format!("remote = \"{}\"\n", p.url));
                    }
                    for r in remotes {
                        s.push_str("[[agent.remote]]\n");
                        s.push_str(&format!("name    = \"{}\"\n", r.name));
                        s.push_str(&format!("url     = \"{}\"\n", r.url));
                        if r.primary {
                            s.push_str("primary = true\n");
                        }
                    }
                }
            }
        }
        if let Some(d) = &self.default {
            s.push_str(&format!("\n[defaults]\nagent = \"{d}\"\n"));
        }
        s
    }

    /// Look an entry up the way a user names one: by label or by identity.
    pub fn find(&self, sel: &str) -> Option<&BoundAgent> {
        self.agents.iter().find(|a| a.id == sel || a.name == sel)
    }

    /// Match on the **id**: a rename must edit the entry, not add a second one.
    /// Both the aid and the name are unique within a binding — the aid is the identity, the name is the
    /// routing key — so an entry colliding on EITHER is the same slot and must be replaced, not
    /// appended. Matching on the aid alone missed a rebind (name fixed, aid changes) and left two
    /// entries wearing one name; matching on the name alone would miss a rename. Any stale entry that
    /// collides on either axis is dropped, so a rebind cannot leave the old identity behind.
    pub fn upsert(&mut self, e: BoundAgent) {
        self.agents.retain(|a| a.id != e.id && a.name != e.name);
        self.agents.push(e);
    }
}

fn unquote(v: &str) -> Option<String> {
    for q in ['"', '\''] {
        if let Some(inner) = v.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return Some(inner.to_string());
        }
    }
    None
}

/// §3: the aid in `.agit.toml` is an integrity check. A recreated `frontend.git`, or DNS moving under
/// us, would otherwise bind the repo to a *different* agent wearing the same name.
pub fn check_binding(bound: &BoundAgent, actual_aid: &str) -> Result<()> {
    if bound.id == actual_aid {
        return Ok(());
    }
    bail!(
        "this repo is bound to {} ({}), but {} is {}\n\
         \x20      If intentional: agit agent rebind {} --remote <url>",
        bound.id,
        bound.name,
        bound.primary_url().unwrap_or("the local store"),
        actual_aid,
        bound.name
    );
}

/// Run the integrity check for a resolved agent against the committed binding.
///
/// Keyed on the **resolved agent**, never on the selector: the active pointer holds an aid, so a
/// selector-keyed lookup finds no `[[agent]]` entry (they are found by label) and the check silently
/// passes — exactly in the case it exists to catch. An entry that already agrees on the id is
/// settled, whatever label it carries; an entry wearing the same *name* with a different id is the
/// recreated-remote case, and is refused.
pub fn check_resolved(binding: &Binding, agent: &Agent) -> Result<()> {
    if binding.agents.iter().any(|e| e.id == agent.aid) {
        return Ok(());
    }
    match binding.agents.iter().find(|e| e.name == agent.name) {
        Some(e) => check_binding(e, &agent.aid),
        None => Ok(()),
    }
}

/// Record an agent in the committed binding. `agit a clone` and `agit a init` both go through here, so
/// a teammate's fresh clone can find the same agents.
pub fn bind_here(agent: &Agent, env_root: &Path, set_default: bool) -> Result<()> {
    crate::init::ensure_gitignore(env_root)?;
    let mut b = Binding::load(env_root)?.unwrap_or_default();
    b.upsert(BoundAgent::single(agent.aid.clone(), agent.name.clone(), agent.remote.clone()));
    if set_default || b.default.is_none() {
        b.default = Some(agent.name.clone());
    }
    b.save(env_root)
}

// ---------------------------------------------------------------------------------------------
// Resolution (§4)
// ---------------------------------------------------------------------------------------------

/// Which rung of §4 supplied the selector. Carried into errors: a wrong answer must say where it
/// came from, or the user cannot fix it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Flag,
    Env,
    Active,
    Default,
}

impl Source {
    pub fn describe(self) -> &'static str {
        match self {
            Source::Flag => "--agent",
            Source::Env => "$AGIT_AGENT",
            Source::Active => "the active agent (agit a switch)",
            Source::Default => concat!("[defaults] agent in ", ".agit.toml"),
        }
    }
}

/// The order itself, as data: each rung wins over the next, and a **blank** rung falls through
/// rather than winning with an empty selector (a truncated active pointer must not break the repo).
fn pick<'a>(
    explicit: Option<&'a str>,
    var: Option<&'a str>,
    active: Option<&'a str>,
    default: Option<&'a str>,
) -> Option<(&'a str, Source)> {
    [
        (explicit, Source::Flag),
        (var, Source::Env),
        (active, Source::Active),
        (default, Source::Default),
    ]
    .into_iter()
    .find_map(|(v, s)| v.map(str::trim).filter(|v| !v.is_empty()).map(|v| (v, s)))
}

/// Resolve the agent for this invocation: `--agent` → `$AGIT_AGENT` → active pointer → `.agit.toml
/// [defaults]` → an actionable error.
pub fn resolve(explicit: Option<&str>) -> Result<Agent> {
    let home = scope::agit_home()?;
    // Outside a git repo the last two rungs simply do not exist; `--agent` / $AGIT_AGENT still work.
    let env = scope::env_root().ok();
    let binding = match env.as_deref() {
        Some(e) => Binding::load(e)?,
        None => None,
    };
    let active = match env.as_deref() {
        Some(e) => read_active(e)?,
        None => None,
    };
    let var = std::env::var("AGIT_AGENT").ok();
    resolve_in(&home, explicit, var.as_deref(), active.as_deref(), binding.as_ref())
}

fn resolve_in(
    home: &Path,
    explicit: Option<&str>,
    var: Option<&str>,
    active: Option<&str>,
    binding: Option<&Binding>,
) -> Result<Agent> {
    let default = binding.and_then(|b| b.default.as_deref());
    let Some((sel, src)) = pick(explicit, var, active, default) else {
        bail!(no_agent_error(home, binding));
    };
    let agent = find_in(home, sel).with_context(|| format!("selected by {}", src.describe()))?;
    if let Some(b) = binding {
        check_resolved(b, &agent)?;
    }
    Ok(agent)
}

fn no_agent_error(home: &Path, binding: Option<&Binding>) -> String {
    let mut s = String::from("no agent selected — agit will not guess which memory you meant.\n");
    let known: Vec<String> = list_in(home)
        .unwrap_or_default()
        .into_iter()
        .map(|a| a.name)
        .collect();
    if !known.is_empty() {
        s.push_str(&format!("  known agents: {}\n", known.join(", ")));
    }
    s.push_str("  agit a switch <name>        set this worktree's agent\n");
    s.push_str("  agit a init <name>          mint one\n");
    s.push_str("  agit start --agent <name>   just this invocation\n");
    if binding.map(|b| b.agents.is_empty()).unwrap_or(true) {
        s.push_str(&format!("  or commit a [defaults] agent in {BINDING_FILE}\n"));
    }
    s
}

// ---------------------------------------------------------------------------------------------
// The verbs
// ---------------------------------------------------------------------------------------------

/// Every local agent, read from the stores themselves (the truth), sorted by name.
pub fn list() -> Result<Vec<Agent>> {
    list_in(&scope::agit_home()?)
}

fn list_in(home: &Path) -> Result<Vec<Agent>> {
    let dir = agents_dir_in(home);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        // An unreadable directory is skipped, not fatal: `list` is how you find out what you have.
        if let Ok(id) = read_identity(&p) {
            out.push(agent_at(p, id));
        }
    }
    out.sort_by(|a, b| (&a.name, &a.aid).cmp(&(&b.name, &b.aid)));
    Ok(out)
}

/// Find an agent by name or aid.
pub fn info(sel: &str) -> Result<Agent> {
    find_in(&scope::agit_home()?, sel)
}

fn find_in(home: &Path, sel: &str) -> Result<Agent> {
    let sel = sel.trim();
    if is_aid(sel) {
        let store = agents_dir_in(home).join(sel);
        let id = read_identity(&store).with_context(|| format!("no agent {sel} on this machine"))?;
        return Ok(agent_at(store, id));
    }
    // The registry is a cache: trust a hit only if the store still says that name.
    if let Some(aid) = registry_load(home).get(sel) {
        let store = agents_dir_in(home).join(aid);
        if let Ok(id) = read_identity(&store) {
            if id.name == sel {
                return Ok(agent_at(store, id));
            }
        }
    }
    // Miss or stale → the truth, and heal the cache on the way past.
    if let Some(a) = list_in(home)?.into_iter().find(|a| a.name == sel) {
        let _ = registry_put(home, sel, &a.aid);
        return Ok(a);
    }
    bail!(
        "unknown agent `{sel}`.\n\
         \x20      agit a list            what this machine has\n\
         \x20      agit a init {sel}       mint it\n\
         \x20      agit a clone <url>     clone it from its remote"
    );
}

/// `agit a switch <name|aid>` — sets MY default in THIS worktree. A default, not a lock: two agents can
/// run in one repo at once, each selected per-invocation with `--agent`.
pub fn switch_agent(sel: &str) -> Result<Agent> {
    let home = scope::agit_home()?;
    let env = scope::env_root()?;
    let agent = find_in(&home, sel)?;
    if let Some(b) = Binding::load(&env)? {
        check_resolved(&b, &agent)?;
    }
    write_active(&env, &agent.aid)?;
    Ok(agent)
}

/// `agit a init <name>` — mint an agent. Works with no remote: identity exists before any URL does.
pub fn init_agent(name: &str) -> Result<Agent> {
    new_agent_in(&scope::agit_home()?, name)
}

fn new_agent_in(home: &Path, name: &str) -> Result<Agent> {
    validate_name(name)?;
    if let Ok(a) = find_in(home, name) {
        bail!(
            "an agent named `{name}` already exists ({}).\n\
             \x20      Names are labels, so agit will not mint a second one wearing the same name.",
            a.aid
        );
    }
    let id = StoreIdentity { aid: mint_aid(), name: name.to_string(), created: now() };
    let store = agents_dir_in(home).join(&id.aid);
    if store.exists() {
        bail!("{} already exists — refusing to overwrite a store", store.display());
    }
    std::fs::create_dir_all(&store)
        .with_context(|| format!("failed to create the agent store at {}", store.display()))?;
    scaffold_store(&store, &id)?;
    registry_put(home, name, &id.aid)?;
    Ok(agent_at(store, id))
}

/// A store is a git repo whose first commit carries the identity. The user/email are set **locally**:
/// agit must never touch the developer's global git identity, and a store with no committer config
/// cannot commit at all.
///
/// agit's own metadata commits (mint/rename/adopt) pass `--no-verify`. They stage nothing but the
/// `agent.toml` agit itself just wrote — an aid, a validated name, a timestamp, with quotes,
/// backslashes and control characters refused at the door — so there is nothing there to scan. The
/// gate exists for what the AGENT saw: sessions, which arrive by snap and by the user's own commits,
/// and which are never part of these.
fn scaffold_store(store: &Path, id: &StoreIdentity) -> Result<()> {
    scope::git_in(store, &["init", "-q", "-b", "main"])?;
    scope::git_in(store, &["config", "user.name", "agit"])?;
    scope::git_in(store, &["config", "user.email", "agit@local"])?;
    write_agent_toml(store, id)?;
    std::fs::create_dir_all(store.join("sessions"))?;
    std::fs::write(store.join("sessions/.gitkeep"), "")?;
    scope::git_in(store, &["add", "-A"])?;
    scope::git_in(store, &["commit", "-q", "--no-verify", "-m", &format!("agit: mint agent {} ({})", id.name, id.aid)])?;
    // After the mint commit: the identity is agit's own and has nothing to scan, and a store must not
    // be able to reach its first real session without the gate already in place.
    crate::init::install_hooks(store)
}

/// `agit a clone <name|url>` — adopt an agent this machine does not have yet. A bare name is looked
/// up in the committed binding, which is how a fresh clone gets its team's agents.
///
/// Tracking **activates** by default (`--no-use` opts out): `track` then `use` was two commands for
/// one intent.
///
/// `init` is the `--init` mode: mint a fresh agent INTO an empty store and push, rather than adopt an
/// existing one. It only makes sense for a URL/binding target whose store is empty — a target already on
/// disk, or a store that already carries an agent, is refused (drop `--init` to adopt it).
pub fn clone_agent(target: &str, activate: bool, init: bool) -> Result<Agent> {
    let home = scope::agit_home()?;
    let env = scope::env_root().ok();
    let binding = match env.as_deref() {
        Some(e) => Binding::load(e)?,
        None => None,
    };

    let agent = match find_in(&home, target) {
        // Already on disk: `track` is idempotent, and re-cloning would be a second copy of one memory.
        // `--init` on a target this machine already has is contradictory — there is nothing empty to mint.
        Ok(_) if !looks_like_url(target) && init => bail!(
            "`{target}` already has an agent on this machine — drop --init to use it (agit a clone {target})."
        ),
        Ok(a) if !looks_like_url(target) => a,
        _ => {
            let url = if looks_like_url(target) {
                target.to_string()
            } else {
                binding
                    .as_ref()
                    .and_then(|b| b.find(target))
                    .and_then(|e| e.primary_url().map(str::to_string))
                    .with_context(|| {
                        format!(
                            "no agent `{target}` on this machine, and {BINDING_FILE} declares no remote for it.\n\
                             \x20      agit a clone <url>   clone it from somewhere"
                        )
                    })?
            };
            clone_in(&home, &url, init)?
        }
    };

    if let Some(b) = &binding {
        check_resolved(b, &agent)?;
    }
    if let Some(e) = env.as_deref() {
        bind_here(&agent, e, false)?;
        if activate {
            write_active(e, &agent.aid)?;
        }
    }
    Ok(agent)
}

/// A URL, or a name? `track` must not treat `frontend` as a relative path.
fn looks_like_url(t: &str) -> bool {
    t.contains("://") || t.contains('@') || t.starts_with('/') || t.starts_with('.') || t.starts_with('~')
}

/// Does this bare target name a KNOWN agent — one already on this machine, or one the committed binding
/// declares? This is the cheap, network-free half of `agit clone <target>`'s smart routing: a bare name
/// that resolves to a local agent (or a `[[agent]]` the team committed) is adopted via the agent path
/// instead of git-cloning a directory called `<name>`. A URL is NEVER a "known name" here — URLs are
/// classified by probing the hub — and an unknown name returns `false`, so git's own clone still runs.
pub fn is_known_local_agent(target: &str) -> bool {
    let t = target.trim();
    if t.is_empty() || looks_like_url(t) {
        return false;
    }
    let Ok(home) = scope::agit_home() else {
        return false;
    };
    if find_in(&home, t).is_ok() {
        return true;
    }
    // A committed binding that declares this agent (by name or aid) is adoptable by name — a fresh clone
    // gets its team's agents exactly this way.
    matches!(scope::env_root().ok().and_then(|e| Binding::load(&e).ok().flatten()), Some(b) if b.find(t).is_some())
}

/// Transports agit will hand to `git clone`. An allowlist, because the danger is a REMOTE THIS MACHINE
/// DID NOT CHOOSE: `.agit.toml` is committed, so `agit a clone frontend` clones a URL whoever wrote the
/// repo picked. That is code execution on `git clone` if the URL names a transport helper.
///
/// Verified against git 2.43: `git clone 'ext::<cmd>'` RUNS `<cmd>` — the clone then reports "Could not
/// read from remote repository", but the payload has already executed. `--` does NOT stop it (it is a
/// scheme, not a flag), and it is gated only by the victim's own `protocol.ext.allow`.
const SAFE_SCHEMES: &[&str] = &["https://", "http://", "ssh://", "git://", "file://"];

/// A remote agit is willing to clone. Rejects rather than sanitizes: a URL we cannot classify is one we
/// cannot vouch for, and `track` has an obvious safe answer — make the human paste it themselves.
fn check_remote(url: &str) -> Result<()> {
    let u = url.trim();
    if u.is_empty() {
        bail!("empty remote URL");
    }
    // `git clone -<anything>` reads as a flag. Not currently exploitable here (the destination does not
    // exist yet, so clone dies before `--upload-pack` runs) — but that is an accident of argument order,
    // not a control, and it would silently become one again if the order ever changed.
    if u.starts_with('-') {
        bail!("refusing a remote that starts with `-` — git would read `{u}` as a flag, not a URL");
    }
    // Remote-helper syntax `<transport>::<address>`, which is what makes ext:: run commands. Checked
    // before the scheme allowlist: `https://` contains no `::`, so nothing legitimate is caught here.
    if let Some(h) = u.split("::").next().filter(|h| *h != u && !h.contains('/')) {
        bail!(
            "refusing the `{h}::` transport — git remote helpers run commands, and `{h}::…` would \
             execute this machine's shell on a URL that came from {BINDING_FILE}.\n\
             \x20      Allowed: {}, git@host:path, or a local path.",
            SAFE_SCHEMES.join(", ")
        );
    }
    let scp_like = || match u.split_once('@') {
        // scp syntax `user@host:path` — a colon after the host, and no scheme.
        Some((user, rest)) => !user.is_empty() && rest.contains(':') && !rest.starts_with('/'),
        None => false,
    };
    let local = u.starts_with('/') || u.starts_with("./") || u.starts_with("../") || u.starts_with('~');
    if SAFE_SCHEMES.iter().any(|s| u.starts_with(s)) || scp_like() || local {
        return Ok(());
    }
    bail!(
        "refusing a remote agit cannot classify: `{u}`\n\
         \x20      Allowed: {}, git@host:path, or a local path.",
        SAFE_SCHEMES.join(", ")
    )
}

/// Clone into a temp dir first: the destination is keyed by the aid, which only the clone can tell us.
///
/// `init` (the `--init` mode) mints a fresh agent into an EMPTY store and pushes, instead of adopting an
/// existing one. Whether `init` is set or not, an empty store (created but never pushed to) is surfaced as
/// a clear, actionable message rather than the raw `agent.toml … No such file` os-error.
fn clone_in(home: &Path, url: &str, init: bool) -> Result<Agent> {
    check_remote(url)?;
    let tmp = home.join("tmp").join(format!("clone-{}", convo::fresh_id(url)));
    std::fs::create_dir_all(tmp.parent().unwrap_or(home))?;
    // Inherited stdio: capturing would swallow git's errors and block credential prompts.
    // Defence in depth behind `check_remote`: `-c` denies the command-running transports outright (a
    // victim with `protocol.ext.allow=always` in their own gitconfig is otherwise one step from RCE),
    // and `--` stops any future `-`-prefixed URL from being read as a flag. Neither is sufficient alone
    // — `--` does not stop `ext::`, and the config does not stop flag smuggling.
    let ok = Command::new("git")
        .args(["-c", "protocol.ext.allow=never", "-c", "protocol.fd.allow=never"])
        .args(["clone", "--quiet", "--", url])
        .arg(&tmp)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("git clone {} failed", crate::hubapi::redact_url(url));
    }
    // An EMPTY store: the clone succeeded but the remote published NO refs at all — a hub store created
    // but never pushed to. This is the case that used to die with a raw `agent.toml … No such file (os
    // error 2)` after read_identity found nothing. Detect it BY EMPTINESS (no refs), distinct from "a git
    // repo that is not an agent store" (which HAS commits, just no agent.toml). `--init` mints a fresh
    // agent into it; without `--init`, surface the actionable message.
    let has_any_ref = !scope::git_in_status(&tmp, &["for-each-ref"]).1.trim().is_empty();
    if !has_any_ref {
        let _ = std::fs::remove_dir_all(&tmp);
        if init {
            return init_into_empty_store(home, url);
        }
        let shown = crate::hubapi::redact_url(url);
        bail!(
            "{shown} is an empty store - nothing has been pushed to it yet.\n\
             \x20      Initialize a fresh agent here with `agit a clone --init {shown}`,\n\
             \x20      or push an existing agent with `agit a push {shown}`."
        );
    }
    // The store is not empty. `--init` asked to mint into an empty one, so refuse rather than silently
    // adopt an agent the caller did not expect to already exist.
    if init {
        let _ = std::fs::remove_dir_all(&tmp);
        let shown = crate::hubapi::redact_url(url);
        bail!("{shown} already has an agent — drop --init to adopt it (agit a clone {shown}).");
    }
    // A store IS just a git repo, and a self-hosted bare hub (`git init --bare` with
    // `init.defaultBranch` unset) has HEAD → `refs/heads/master` while agit's stores are on `main`. The
    // clone then checks out NO working tree ("remote HEAD refers to nonexistent ref"), so agent.toml is
    // absent from the tree even though the store is fully present under a remote branch. Recover by
    // checking out whichever branch the remote actually has, rather than declaring the store invalid.
    if scope::git_in_status(&tmp, &["rev-parse", "--verify", "--quiet", "HEAD"]).0 != 0 {
        let (_, refs) = scope::git_in_status(&tmp, &["for-each-ref", "--format=%(refname:short)", "refs/remotes/origin/"]);
        let branch = refs.lines().find(|b| b.ends_with("/main"))
            .or_else(|| refs.lines().find(|b| b.ends_with("/master")))
            .or_else(|| refs.lines().find(|b| !b.ends_with("/HEAD")));
        if let Some(rb) = branch {
            let local = rb.rsplit('/').next().unwrap_or("main");
            let _ = scope::git_in(&tmp, &["checkout", "-B", local, rb]);
        }
    }
    let id = match read_identity(&tmp) {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e).with_context(|| format!("{} is not an agent store", crate::hubapi::redact_url(url)));
        }
    };
    let dest = agents_dir_in(home).join(&id.aid);
    if dest.exists() {
        // Same aid ⇒ same agent: this machine already has that memory. Keep the copy that has history.
        let _ = std::fs::remove_dir_all(&tmp);
    } else {
        std::fs::create_dir_all(agents_dir_in(home))?;
        std::fs::rename(&tmp, &dest)
            .with_context(|| format!("failed to move the clone into {}", dest.display()))?;
    }
    let id = read_identity(&dest)?;
    // Hooks live in .git/hooks, which no clone carries: a tracked agent would otherwise push a secret
    // its owner's machine would have caught.
    crate::init::install_hooks(&dest)?;
    registry_put(home, &id.name, &id.aid)?;
    Ok(agent_at(dest, id))
}

/// `agit a clone --init <url>` into an EMPTY store: mint a fresh agent locally (fresh aid + agent.toml,
/// the same `new_agent_in` path a bare `agit a init` uses), point its `origin` at `<url>`, and push so the
/// once-empty store becomes a valid, adoptable agent. The name is derived from the store URL's last path
/// segment (a store URL is `.../<name>.git`); a collision or an unusable segment falls back to a minted
/// name, so `--init` always succeeds against an empty store.
fn init_into_empty_store(home: &Path, url: &str) -> Result<Agent> {
    let shown = crate::hubapi::redact_url(url);
    let name = mint_name_for(home, url);
    let a = new_agent_in(home, &name)?;
    // Point the fresh store at the empty remote and publish it. A push failure here leaves a perfectly
    // good LOCAL agent behind (the mint already succeeded), so report it but keep the agent.
    scope::git_in(&a.store, &["remote", "add", "origin", url])
        .with_context(|| format!("failed to set origin to {shown}"))?;
    let branch = scope::git_in_status(&a.store, &["rev-parse", "--abbrev-ref", "HEAD"]).1.trim().to_string();
    let branch = if branch.is_empty() { "main".to_string() } else { branch };
    let code = scope::git_in_inherit(&a.store, &["push", "--no-verify", "-u", "origin", &branch]);
    if code != 0 {
        bail!(
            "minted {} ({}) locally, but the push to {shown} failed (exit {code}).\n\
             \x20      Fix the remote, then publish it with `agit a push`.",
            a.name,
            a.aid
        );
    }
    Ok(a)
}

/// The agent name to mint for `agit a clone --init <url>`: the store URL's last path segment with a
/// trailing `.git` removed (`https://hub/alice/frontend.git` → `frontend`), when it is a usable, free
/// name; otherwise a minted `agent-<short>` fallback that is guaranteed usable and unique.
fn mint_name_for(home: &Path, url: &str) -> String {
    let derived = store_name_from_url(url).filter(|n| is_usable_name(n) && find_in(home, n).is_err());
    if let Some(n) = derived {
        return n;
    }
    // No usable/free name from the URL — mint one. A few attempts makes a collision vanishingly unlikely.
    for _ in 0..16 {
        let cand = format!("agent-{}", &mint_aid()[4..12]);
        if is_usable_name(&cand) && find_in(home, &cand).is_err() {
            return cand;
        }
    }
    format!("agent-{}", &mint_aid()[4..20])
}

/// The last path segment of a store URL, with any `.git` suffix stripped — the natural agent name for a
/// store published at `.../<name>.git`. `None` when the URL carries no usable segment.
fn store_name_from_url(url: &str) -> Option<String> {
    // Drop any `?query`/`#fragment`, then take the final path segment. Works for http(s)/ssh/scp/local.
    let path = url.split(['?', '#']).next().unwrap_or(url).trim_end_matches('/');
    // scp syntax `user@host:owner/name.git` puts the path after the LAST ':'; a URL path after the LAST
    // '/'. Taking the segment after whichever separator comes last handles both.
    let seg = path.rsplit(['/', ':']).next().unwrap_or(path);
    let seg = seg.strip_suffix(".git").unwrap_or(seg);
    (!seg.is_empty()).then(|| seg.to_string())
}

/// `agit a rename <old> <new>` — metadata only. The store is keyed by the aid, so nothing moves and
/// a running watcher keeps working.
/// A URL with any credential stripped, for writing into the COMMITTED binding.
///
/// `https://x:ghp_…@github.com/me/f.git` → `https://github.com/me/f.git`. For http(s) the whole userinfo
/// goes: git needs none of it to clone, and a bare username is still someone's account name in a file
/// the whole team reads. For ssh/scp the user is part of the ADDRESS, not a credential — `git@github.com`
/// stops resolving without it — so only a password is removed there.
fn locator(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // No scheme → scp-like `[user[:pw]@]host:path`. The user is the ssh address and stays, but a
        // password (non-standard, but a hand-written `x:ghp_…@host:path` reaches here) is stripped, so
        // even a colon-userinfo scp URL cannot carry a secret into the committed binding.
        if let Some((userinfo, hostpath)) = url.split_once('@') {
            let looks_scp = hostpath.contains(':') && !hostpath.starts_with('/')
                && !url.starts_with('/') && !url.starts_with('.') && !url.starts_with('~');
            if looks_scp {
                let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
                return if user.is_empty() { hostpath.to_string() } else { format!("{user}@{hostpath}") };
            }
        }
        return url.to_string();
    };
    // Userinfo lives only in the authority (up to the first `/`); a `@` in the path is just a character.
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    // The userinfo/host boundary is the LAST `@` in the authority — a password may itself contain `@`
    // (`user:p@ss@host`), so splitting on the first `@` would leave part of the password behind. That
    // was a real credential leak into a committed file.
    let Some((userinfo, host)) = authority.rsplit_once('@') else { return url.to_string() };
    // ssh/scp keep the user (it is the address: `git@github.com` stops resolving without it); http(s)
    // drop the whole userinfo, since a bare username is still an account name in a shared file.
    let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
    let authority = match (matches!(scheme, "ssh" | "git"), user.is_empty()) {
        (true, false) => format!("{user}@{host}"),
        _ => host.to_string(),
    };
    match path {
        Some(p) => format!("{scheme}://{authority}/{p}"),
        None => format!("{scheme}://{authority}"),
    }
}

/// The locator to COMMIT for a remote — `locator`, then a scanner backstop. `locator` strips userinfo,
/// which is where git carries credentials; but a token can hide elsewhere (a `?token=` query, the path),
/// and this file gets committed and pushed to the team. Rather than enumerate every place a secret could
/// sit, refuse to write any locator agit's own scanner still flags. Defense in depth behind `locator`,
/// not a replacement for it.
fn committed_locator(url: &str) -> Result<String> {
    let loc = locator(url);
    if !crate::scan::scan_text(&loc).is_empty() {
        bail!(
            "that remote still looks like it carries a secret after stripping credentials, and {BINDING_FILE}\n\
             \x20      is committed. Use a URL whose secret is NOT in the address (a credential helper, or\n\
             \x20      ssh), so nothing sensitive is written to a file your team clones."
        );
    }
    Ok(loc)
}

/// One remote recorded (or that would be recorded) into the committed binding by a push sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedRemote {
    pub name: String,
    /// The credential-stripped locator actually written to the binding.
    pub locator: String,
    pub primary: bool,
    /// True when a credential was removed from the store's raw remote URL on the way in.
    pub stripped: bool,
}

/// The result of reconciling the committed binding with ALL of the store's git remotes. Replaces the
/// single-remote `BindingSync`: a store may push to several remotes, so the sync records each shareable
/// one and reports the rest, marking the primary (the `origin`/first-remote identity anchor).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoteSyncSummary {
    /// The remotes recorded next to the aid (empty when the store had none, or none were shareable).
    pub recorded: Vec<RecordedRemote>,
    /// `(name, raw url)` for each remote skipped because it is not a transport agit will write into a
    /// committed file (an `ext::`/`-` helper, or a URL still carrying a secret after stripping). The
    /// local push to it is git's own business — this only refuses to *record* it.
    pub skipped: Vec<(String, String)>,
    /// Whether the binding's stored remotes for this agent actually changed.
    pub changed: bool,
}

/// Reconcile the committed binding with EVERY git remote on the store: record each shareable remote's
/// credential-stripped locator next to the aid (marking the `origin`/first-remote primary), so a
/// teammate's fresh clone can find this agent and its extra read locators. Idempotent — an unchanged
/// remote set writes nothing.
///
/// This is what makes `agit a push` the agent-context push rather than a bare git push: the locators are
/// only useful to anyone else once committed next to the aid, and a push is exactly the moment a store
/// first gains (or changes) the remotes worth recording.
pub fn sync_remotes_to_binding(aid: &str, env_root: &Path) -> Result<RemoteSyncSummary> {
    sync_remotes_to_binding_in(&scope::agit_home()?, aid, env_root)
}

fn sync_remotes_to_binding_in(home: &Path, aid: &str, env_root: &Path) -> Result<RemoteSyncSummary> {
    // Re-read from the store's own remotes, so this is the same answer every other reader gets.
    let a = find_in(home, aid)?;
    let raw_remotes = store_remotes(&a.store);
    let primary_name = primary_remote_name(&a.store);
    let mut summary = RemoteSyncSummary::default();
    let mut bound: Vec<BoundRemote> = Vec::new();
    let mut stripped: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (name, raw) in &raw_remotes {
        // An `ext::`/`-` remote must never land in the COMMITTED binding (the RCE-on-clone vector), and
        // `committed_locator` refuses anything still holding a secret after credential stripping. Either
        // is reported and skipped — one bad extra remote never aborts recording the good ones.
        if check_remote(raw).is_err() {
            summary.skipped.push((name.clone(), raw.clone()));
            continue;
        }
        let locator = match committed_locator(raw) {
            Ok(loc) => loc,
            Err(_) => {
                summary.skipped.push((name.clone(), raw.clone()));
                continue;
            }
        };
        if &locator != raw {
            stripped.insert(name.clone());
        }
        let primary = primary_name.as_deref() == Some(name.as_str());
        bound.push(BoundRemote { name: name.clone(), url: locator, primary });
    }
    // The primary (origin) may itself have been skipped; guarantee exactly one primary among what we
    // can actually record, so the binding always has a resolvable anchor.
    if !bound.iter().any(|r| r.primary) {
        if let Some(first) = bound.first_mut() {
            first.primary = true;
        }
    }
    summary.recorded = bound
        .iter()
        .map(|r| RecordedRemote {
            name: r.name.clone(),
            locator: r.url.clone(),
            primary: r.primary,
            stripped: stripped.contains(&r.name),
        })
        .collect();

    // Compare against what the binding already records for this agent.
    let current = Binding::load(env_root)?
        .and_then(|b| b.agents.into_iter().find(|e| e.id == a.aid).map(|e| e.remotes));
    summary.changed = current.as_deref() != Some(bound.as_slice());

    // Only write when there is something shareable to record and it changed. A store with no shareable
    // remote must not wipe what a previous push committed — leave the binding untouched.
    if !bound.is_empty() && summary.changed {
        crate::init::ensure_gitignore(env_root)?;
        let mut b = Binding::load(env_root)?.unwrap_or_default();
        b.upsert(BoundAgent { id: a.aid.clone(), name: a.name.clone(), remotes: bound });
        if b.default.is_none() {
            b.default = Some(a.name.clone());
        }
        b.save(env_root)?;
    }
    Ok(summary)
}


pub fn rename(old: &str, new: &str) -> Result<Agent> {
    let home = scope::agit_home()?;
    // Refuse if `new` is already a DIFFERENT agent in the committed binding — even one not cloned
    // locally (rename_in only sees local stores). Without this, the rename's binding upsert would
    // silently drop that other agent's entry (it collides on the name), and a teammate's `agit init`
    // would then no longer clone it. Names are labels: two agents may never share one.
    if let Ok(env) = scope::env_root() {
        if let Some(b) = Binding::load(&env)? {
            let mine = find_in(&home, old).map(|a| a.aid).ok();
            if let Some(other) = b.find(new) {
                if Some(&other.id) != mine.as_ref() {
                    bail!(
                        "`{new}` is already declared in {BINDING_FILE} for a different agent ({}).\n\
                         \x20      Names are labels; rename to something else, or remove that entry first.",
                        other.id
                    );
                }
            }
        }
    }
    let agent = rename_in(&home, old, new)?;
    // The binding names agents by label, so a rename that skipped it would leave `[defaults] api`
    // pointing at nothing.
    if let Ok(env) = scope::env_root() {
        if let Some(mut b) = Binding::load(&env)? {
            // Rename is a metadata edit on a fixed aid: preserve whatever remotes the binding already
            // records (the fan-out may have recorded several) rather than collapsing to the store's
            // origin — only the label changes.
            if let Some(remotes) = b.find(&agent.aid).map(|e| e.remotes.clone()) {
                let was_default = b.default.as_deref() == Some(old);
                b.upsert(BoundAgent {
                    id: agent.aid.clone(),
                    name: agent.name.clone(),
                    remotes,
                });
                if was_default {
                    b.default = Some(agent.name.clone());
                }
                b.save(&env)?;
            }
        }
    }
    Ok(agent)
}

fn rename_in(home: &Path, old: &str, new: &str) -> Result<Agent> {
    validate_name(new)?;
    let agent = find_in(home, old)?;
    if agent.name == new {
        return Ok(agent);
    }
    if let Ok(other) = find_in(home, new) {
        bail!("`{new}` is already taken by {}", other.aid);
    }
    let cur = read_identity(&agent.store)?;
    let id = StoreIdentity { aid: cur.aid, name: new.to_string(), created: cur.created };
    write_agent_toml(&agent.store, &id)?;
    scope::git_in(&agent.store, &["add", "agent.toml"])?;
    scope::git_in(
        &agent.store,
        &["commit", "-q", "--no-verify", "-m", &format!("agit: rename agent {old} -> {new}")],
    )?;
    let mut m = registry_load(home);
    m.remove(old);
    m.insert(new.to_string(), id.aid.clone());
    registry_save(home, &m)?;
    Ok(agent_at(agent.store, id))
}

// ---------------------------------------------------------------------------------------------
// import — the one-shot adoption of a legacy nested store
// ---------------------------------------------------------------------------------------------

/// `kill -0 <pid>`: asks the kernel whether the process exists without delivering a signal.
fn pid_alive(pid: u32) -> bool {
    // pid 0 is not a real process: `kill -0 0` signals the caller's own process group and succeeds, so
    // a zeroed or corrupt pidfile would read as "a watcher is live" and wedge import/rebind forever.
    // Guard it, exactly as session.rs does.
    pid != 0
        && Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

/// Every pidfile a live watcher could be recorded in. Two locations, deliberately: `session::watch_daemon`
/// writes it inside the store's own `.git`, and the design moves it to `<env>/.agit/` (a shared store
/// means two repos would otherwise collide on one pidfile). Import must refuse under either, so it stays
/// correct across that move rather than silently losing its pre-flight the day the pidfile relocates.
fn watcher_pidfiles(env: &Path, store: &Path) -> [PathBuf; 2] {
    [store.join(".git/agit-watch.pid"), env.join(".agit/agit-watch.pid")]
}

fn live_watcher(env: &Path, store: &Path) -> Option<u32> {
    watcher_pidfiles(env, store).into_iter().find_map(|p| {
        let pid: u32 = std::fs::read_to_string(&p).ok()?.trim().parse().ok()?;
        pid_alive(pid).then_some(pid)
    })
}

/// EXDEV. `$AGIT_HOME` and the code repo routinely sit on different filesystems, where `rename` cannot
/// work at all — a container's bind-mounted workspace against a home on the image, say.
const EXDEV: i32 = 18;

/// Move a directory, atomically when the kernel allows it. Only a genuine cross-device error falls back
/// to copy: any other failure is reported as itself, rather than being retried as a slow copy that will
/// fail again for the same reason and blame the wrong step.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => return Ok(()),
        Err(e) if e.raw_os_error() == Some(EXDEV) => {}
        Err(e) => {
            return Err(e).with_context(|| format!("failed to move {} → {}", from.display(), to.display()))
        }
    }
    copy_dir(from, to).with_context(|| format!("failed to copy {} → {}", from.display(), to.display()))?;
    std::fs::remove_dir_all(from)
        .with_context(|| format!("copied to {}, but failed to remove {}", to.display(), from.display()))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ft = e.file_type()?;
        if ft.is_dir() {
            copy_dir(&src, &dst)?;
        } else if ft.is_symlink() {
            // Copying the target would silently turn a link into a second copy of the file.
            let target = std::fs::read_link(&src)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&target, &dst)?;
            #[cfg(windows)]
            {
                // Windows has symlinks too, but creating one needs privilege (admin, or Developer Mode)
                // and it distinguishes file from directory links. Make the right kind; only if Windows
                // denies it do we fall back to copying a file target, so the store copy still succeeds.
                use std::os::windows::fs::{symlink_dir, symlink_file};
                let made = if src.is_dir() {
                    symlink_dir(&target, &dst)
                } else {
                    symlink_file(&target, &dst)
                };
                if made.is_err() && src.is_file() {
                    std::fs::copy(&src, &dst)?;
                } else {
                    made?;
                }
            }
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// `agit a rebind <name> --remote <url>` — deliberately override the integrity check (§4).
///
/// The binding records `name → (aid, remote)` as an integrity check: a recreated `frontend.git`, or DNS
/// moving under you, would otherwise silently bind this repo to a *different* agent wearing the same
/// name, and `check_binding` refuses. Rebind is the "yes, I meant that" — it rewrites the binding entry
/// to whatever identity the named store actually holds, and points it at the new remote.
///
/// `--new-id` is the other referenced use: a store cloned from a FORK carries its source's aid (the fork
/// wears the source's identity until re-minted). `--new-id` gives this store a fresh aid, so a fork
/// becomes a genuinely independent memory rather than a second claimant on one identity.
pub fn rebind(sel: Option<&str>, remote: Option<&str>, new_id: bool) -> Result<Agent> {
    let home = scope::agit_home()?;
    let env = scope::env_root()?;
    let sel = sel.map(str::trim).filter(|s| !s.is_empty());

    if new_id {
        // Re-mint: change the store's committed identity, which moves it (the store is keyed by aid on
        // disk), then repoint the registry, binding and active pointer. Same shape as `import`, minus
        // the move-from-legacy.
        //
        // Resolve the EXACT store named. `find_in` (not `resolve`) bypasses the integrity check that a
        // fork trips by construction — but it still honors the name. A bad name must error, never fall
        // through to the active agent: re-mint MOVES the store and rewrites its identity, and doing
        // that to the wrong store on a typo is not something to guess at. Only with no name given do we
        // fall back to the active pointer.
        let agent = match sel {
            Some(s) => find_in(&home, s)
                .with_context(|| format!("no agent `{s}` on this machine to re-mint — agit a clone <url> first"))?,
            None => {
                let aid = read_active(&env)?.context("no agent selected to re-mint — agit a switch <name> first")?;
                find_in(&home, &aid)?
            }
        };
        // Re-mint MOVES the store (it is keyed by aid). Moving it out from under a live watcher zombies
        // the daemon onto the old inode — the same silent-capture-loss import refuses, so refuse here too.
        if let Some(pid) = live_watcher(&env, &agent.store) {
            bail!(
                "a watcher is running (pid {pid}) — refusing to re-mint the store out from under it.\n\
                 \x20      Stop it, re-mint, then start it again:\n\
                 \x20        agit watch --stop\n\
                 \x20        agit a rebind --new-id\n\
                 \x20        agit watch --daemon"
            );
        }
        let fresh = mint_aid();
        let dest = agents_dir_in(&home).join(&fresh);
        if dest.exists() {
            bail!("{} already exists — refusing to overwrite a store", dest.display());
        }
        // Re-minting changes the IDENTITY, and the store is shared across repos by design: any other
        // repo or worktree still bound to the old aid will stop resolving until it re-tracks the fork.
        // Said, never silent — the old aid genuinely no longer exists after this.
        eprintln!(
            "  note: re-minting gives this store a new identity ({} → a fresh aid). Any OTHER repo bound\n\
             \x20      to the old aid must `agit a clone` the fork to follow it.",
            agent.aid
        );
        let id = StoreIdentity { aid: fresh.clone(), name: agent.name.clone(), created: now() };
        move_dir(&agent.store, &dest)?;
        write_agent_toml(&dest, &id)?;
        scope::git_in(&dest, &["add", "agent.toml"])?;
        scope::git_in(&dest, &["commit", "-q", "--no-verify", "-m",
            &format!("agit: re-mint identity {} → {} ({})", agent.aid, id.name, fresh)])?;
        // The registry is keyed by NAME, which is unchanged, so this overwrites the old aid mapping;
        // the old aid-keyed directory no longer exists (it was moved), and the cache self-heals via
        // `repair` regardless.
        registry_put(&home, &id.name, &fresh)?;
        let rebound = agent_at(dest, id);
        bind_here(&rebound, &env, false)?;
        write_active(&env, &rebound.aid)?;
        return Ok(rebound);
    }

    let sel = sel.map(str::trim).filter(|s| !s.is_empty())
        .context("agit a rebind: name a bound agent, or pass --new-id\n  usage: agit a rebind <name> --remote <url>")?;
    // The store this name actually resolves to, whatever aid it holds — that identity is what the
    // binding must be corrected to. find_in bypasses the integrity check on purpose: overriding it is
    // the whole point of this verb.
    let agent = find_in(&home, sel)
        .with_context(|| format!("no agent `{sel}` on this machine to rebind — agit a clone <url> first"))?;
    let mut b = Binding::load(&env)?.unwrap_or_default();
    // Every remote written to the COMMITTED binding goes through committed_locator — the new one if
    // `--remote` is given, else the store's EXISTING origin. The `--remote`-omitted fallback used to
    // write `agent.remote` raw (the credentialed `git remote get-url origin`) straight into the file
    // publish tells you to commit: a token into the team and into git history. Never again.
    let committed = match remote {
        Some(r) => {
            check_remote(r)?;
            let loc = committed_locator(r)?; // fails before any mutation if it would leak
            match scope::git_in_status(&agent.store, &["remote", "get-url", "origin"]).0 {
                0 => scope::git_in(&agent.store, &["remote", "set-url", "origin", r])?,
                _ => scope::git_in(&agent.store, &["remote", "add", "origin", r])?,
            };
            Some(loc)
        }
        None => agent.remote.as_deref().map(committed_locator).transpose()?,
    };
    b.upsert(BoundAgent::single(agent.aid.clone(), agent.name.clone(), committed.clone()));
    b.save(&env)?;
    write_active(&env, &agent.aid)?;
    // Return the LOCATOR, not the store's raw origin: a caller printing `remote` must not echo a token
    // to the terminal or a CI log (the same hygiene publish already keeps).
    Ok(Agent { remote: committed, ..agent })
}

// ---------------------------------------------------------------------------------------------
// Provenance: the machine's ed25519 signing identity
// ---------------------------------------------------------------------------------------------
//
// Attribution today is a plaintext launch record: it SAYS who produced a session but proves nothing.
// A signing key ties a session to the machine that captured it. The keypair is per-machine, minted
// once on first use and reused forever, so provenance is fully offline: no server, no enrollment.
//
// Cross-team pubkey→person trust is the hub's job (out of scope here). What lives here is client-side:
// mint the key, sign a session's digest, and self-verify a recorded signature against its own pubkey.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};

/// `$AGIT_HOME/identity/` — where the machine keypair lives, spanning every repo like `launches.jsonl`.
fn identity_dir(home: &Path) -> PathBuf {
    home.join("identity")
}

/// The private signing key, stored 0600. Hex of the 32 raw ed25519 secret bytes.
fn signing_key_path(home: &Path) -> PathBuf {
    identity_dir(home).join("ed25519")
}

/// Load this machine's signing key, minting it once on first use. The public path resolves `$AGIT_HOME`.
pub fn machine_signing_key() -> Result<SigningKey> {
    load_or_create_signing_key(&scope::agit_home()?)
}

/// Load-or-create, taking `home` explicitly so a test can point it at a temp dir.
///
/// A key already on disk is reused verbatim: rotating it would strand every signature it ever made.
pub fn load_or_create_signing_key(home: &Path) -> Result<SigningKey> {
    let path = signing_key_path(home);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let raw = hex::decode(text.trim())
            .with_context(|| format!("{} is not valid hex — the machine identity is corrupt", path.display()))?;
        let bytes: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is not a 32-byte ed25519 key", path.display()))?;
        return Ok(SigningKey::from_bytes(&bytes));
    }
    // First use on this machine: mint the keypair and persist it before returning, so the very first
    // signature is reproducible on the next run.
    std::fs::create_dir_all(identity_dir(home))
        .with_context(|| format!("cannot create {}", identity_dir(home).display()))?;
    let mut csprng = rand::rngs::OsRng;
    let key = SigningKey::generate(&mut csprng);
    write_secret_0600(&path, &hex::encode(key.to_bytes()))?;
    // A world-readable public half is a convenience, never a secret: teammates need it to verify.
    let _ = std::fs::write(
        identity_dir(home).join("ed25519.pub"),
        format!("{}\n", hex::encode(key.verifying_key().to_bytes())),
    );
    Ok(key)
}

/// The machine's public verifying key as hex, minting the keypair if this machine has none yet.
pub fn machine_pubkey_hex() -> Result<String> {
    Ok(hex::encode(machine_signing_key()?.verifying_key().to_bytes()))
}

/// Write a private file created with 0600 from the start (no world-readable window). On non-unix the
/// permission bits do not apply, so it is a plain write there.
pub(crate) fn write_secret_0600(path: &Path, contents: &str) -> Result<()> {
    let body = format!("{contents}\n");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("cannot create {}", path.display()))?;
        f.write_all(body.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, body).with_context(|| format!("cannot create {}", path.display()))?;
    }
    Ok(())
}

/// Sign `message` with a signing key, returning the 64-byte signature as hex.
pub fn sign_hex(key: &SigningKey, message: &[u8]) -> String {
    hex::encode(key.sign(message).to_bytes())
}

/// Verify `sig_hex` over `message` against `pubkey_hex`. Every malformed input is a plain `false`, never
/// an error or a panic: a signature that cannot be parsed is exactly as unverified as one that does not
/// match, and provenance verification must never block on bad data.
pub fn verify_hex(pubkey_hex: &str, message: &[u8], sig_hex: &str) -> bool {
    let Ok(pk_raw) = hex::decode(pubkey_hex.trim()) else { return false };
    let Ok(pk_bytes) = <[u8; 32]>::try_from(pk_raw.as_slice()) else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&pk_bytes) else { return false };
    let Ok(sig_raw) = hex::decode(sig_hex.trim()) else { return false };
    let Ok(sig_bytes) = <[u8; 64]>::try_from(sig_raw.as_slice()) else { return false };
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    vk.verify(message, &sig).is_ok()
}

// ---------------------------------------------------------------------------------------------
// Encryption identity: the X25519 half, derived from the SAME ed25519 secret (Wave 1)
// ---------------------------------------------------------------------------------------------
//
// The encryption-recipients design (docs/plans/2026-07-20-encryption-recipients-design.md) folds one
// on-disk secret into two roles: the ed25519 key above signs pushes/provenance, and an X25519 keypair
// DERIVED from the same secret unwraps content keys (Wave 2+). Deriving rather than minting a second
// key means one thing to back up and one registry row per person.
//
// The map is the standard ed25519 -> curve25519 (Edwards -> Montgomery) one, computed with
// curve25519-dalek's own clamp/basepoint helpers rather than a hand-rolled clamp:
//   * secret scalar = clamp_integer(SHA-512(seed)[..32]) — byte-for-byte the ed25519 signing scalar,
//   * public        = secret · Montgomery-basepoint, which equals to_montgomery(ed25519_public).
// Determinism falls out of both steps being pure functions of the seed.

use curve25519_dalek::constants::X25519_BASEPOINT;
use curve25519_dalek::edwards::CompressedEdwardsY;
use curve25519_dalek::scalar::clamp_integer;
use sha2::{Digest, Sha512};

/// The 32-byte X25519 secret scalar derived from this machine's ed25519 signing key. Pure and
/// deterministic: the same signing key always yields the same scalar. Already clamped, so it is a
/// valid X25519 scalar ready for ECDH (Wave 2+ key-unwrap). Never uploaded — only its public half is.
pub fn derive_x25519_secret(signing: &SigningKey) -> [u8; 32] {
    // The ed25519 secret scalar derivation: SHA-512 the 32-byte seed, take the low 32 bytes, clamp.
    let hash = Sha512::digest(signing.to_bytes());
    let mut prefix = [0u8; 32];
    prefix.copy_from_slice(&hash[..32]);
    clamp_integer(prefix)
}

/// The X25519 public key for an already-derived secret scalar: `scalar · basepoint`. `mul_clamped`
/// clamps again, which is idempotent on an already-clamped scalar, so this stays the plain basepoint
/// multiplication of `derive_x25519_secret`'s output.
pub fn x25519_public_from_secret(secret: &[u8; 32]) -> [u8; 32] {
    X25519_BASEPOINT.mul_clamped(*secret).0
}

/// The birational cross-check: the X25519 public that corresponds to an ed25519 public key, via
/// Edwards decompression + `to_montgomery`. Equal to `x25519_public_from_secret(derive_x25519_secret)`
/// for the same identity. Returns `None` for a public key that is not a valid Edwards point.
pub fn x25519_public_from_ed25519_public(ed_pub: &[u8; 32]) -> Option<[u8; 32]> {
    Some(CompressedEdwardsY(*ed_pub).decompress()?.to_montgomery().0)
}

/// This machine's X25519 public key as hex, minting/deriving from the ed25519 identity as needed.
pub fn machine_x25519_pubkey_hex() -> Result<String> {
    let sk = machine_signing_key()?;
    Ok(hex::encode(x25519_public_from_secret(&derive_x25519_secret(&sk))))
}

/// The exact bytes signed by an identity-registry enrollment: `enroll_sig` proves the caller holds the
/// private ed25519 key for the `ed25519_pub` it submits, binding it to the `x25519_pub`, the `epoch`,
/// and the `username`. A version tag domain-separates it from provenance (and every other agit
/// signature); newline joins are unambiguous because a username has no newline (see `valid_username`),
/// the pubkeys are hex, and the epoch is decimal. The client computes it, the hub re-computes and
/// verifies it against the SUBMITTED `ed25519_pub`, so the hub can only ever replace a row, never mint
/// a valid one for a key it does not hold.
pub fn identity_enroll_message(username: &str, epoch: i64, ed25519_pub: &str, x25519_pub: &str) -> Vec<u8> {
    format!("agit-identity-enroll-v1\n{username}\n{epoch}\n{ed25519_pub}\n{x25519_pub}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A code repo, with no global git config in reach (the committer identity is set per store).
    fn env_repo(dir: &Path) -> PathBuf {
        let r = dir.join("code");
        std::fs::create_dir_all(&r).unwrap();
        scope::git_in(&r, &["init", "-q", "-b", "main"]).unwrap();
        r
    }

    /// The machine key is minted once and its private half is not world-readable: a signing key any
    /// process on the box can read is not an identity.
    #[test]
    fn machine_key_is_minted_once_and_private() {
        let h = tmp();
        let k1 = load_or_create_signing_key(h.path()).unwrap();
        let priv_path = h.path().join("identity").join("ed25519");
        let pub_path = h.path().join("identity").join("ed25519.pub");
        assert!(priv_path.exists(), "the private key must be written on first use");
        assert!(pub_path.exists(), "the public half must be written too, for verifiers");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&priv_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the private key must be 0600, got {mode:o}");
        }

        // Reload returns the same key — signatures made now must still verify later.
        let k2 = load_or_create_signing_key(h.path()).unwrap();
        assert_eq!(k1.to_bytes(), k2.to_bytes(), "the key must be stable across loads");

        // A signature round-trips through the hex helpers.
        let sig = sign_hex(&k1, b"hello");
        let pk = hex::encode(k1.verifying_key().to_bytes());
        assert!(verify_hex(&pk, b"hello", &sig), "a valid signature must verify");
        assert!(!verify_hex(&pk, b"tampered", &sig), "a different message must not verify");
    }

    /// The X25519 derivation is a pure function of the ed25519 secret (deterministic), and the public
    /// it produces is genuinely `scalar · basepoint` — proved two ways: directly via the basepoint
    /// multiplication, and independently via the birational `to_montgomery(ed25519_public)` map. If the
    /// clamp or the map were wrong these two would diverge.
    #[test]
    fn x25519_derivation_is_deterministic_and_public_matches_the_scalar() {
        let h = tmp();
        let sk = load_or_create_signing_key(h.path()).unwrap();

        // Deterministic: the same key yields the same secret and public every time.
        let s1 = derive_x25519_secret(&sk);
        let s2 = derive_x25519_secret(&sk);
        assert_eq!(s1, s2, "the derived X25519 secret must be stable for a given identity");

        let pub_from_scalar = x25519_public_from_secret(&s1);
        assert_eq!(pub_from_scalar, x25519_public_from_secret(&s2), "the derived public must be stable");

        // The public is the scalar's basepoint multiple: cross-check it against the Edwards->Montgomery
        // image of the ed25519 public key, which is the same point by the birational equivalence.
        let ed_pub = sk.verifying_key().to_bytes();
        let via_montgomery = x25519_public_from_ed25519_public(&ed_pub).expect("ed25519 public is a valid point");
        assert_eq!(pub_from_scalar, via_montgomery, "scalar-basepoint public must equal to_montgomery(ed_public)");

        // The clamp is real: a valid X25519 scalar has its low 3 bits cleared and the top two bits fixed.
        assert_eq!(s1[0] & 0b0000_0111, 0, "low 3 bits must be clamped off");
        assert_eq!(s1[31] & 0b1100_0000, 0b0100_0000, "high bits must be clamped");

        // A different identity derives a different keypair (no accidental constant).
        let h2 = tmp();
        let sk2 = load_or_create_signing_key(h2.path()).unwrap();
        assert_ne!(pub_from_scalar, x25519_public_from_secret(&derive_x25519_secret(&sk2)), "distinct identities differ");
    }

    #[test]
    fn aid_minting_is_shaped_stable_and_unique() {
        let a = mint_aid();
        assert!(is_aid(&a), "{a} must pass the shape gate the hub enforces");
        assert!(a.starts_with("agt_"));
        let ids: std::collections::BTreeSet<String> = (0..50).map(|_| mint_aid()).collect();
        assert_eq!(ids.len(), 50, "aids must be unique");

        // Stable: minted once, then read back from the store's committed agent.toml forever.
        let h = tmp();
        let agent = new_agent_in(h.path(), "frontend").unwrap();
        assert_eq!(read_identity(&agent.store).unwrap().aid, agent.aid);
        assert_eq!(find_in(h.path(), "frontend").unwrap().aid, agent.aid);
        assert_eq!(
            agent.store,
            h.path().join("agents").join(&agent.aid),
            "the store is keyed by aid, so rename/publish never move it"
        );
        // ...and committed, not merely on disk.
        let show = scope::git_in(&agent.store, &["show", "HEAD:agent.toml"]).unwrap();
        assert!(show.contains(&agent.aid), "the aid must travel with the store's history");
    }

    #[test]
    fn the_legacy_placeholder_is_not_an_identity() {
        let h = tmp();
        let store = h.path().join("agents/legacy");
        std::fs::create_dir_all(&store).unwrap();
        // Byte-for-byte what agit scaffolded into every store before identity existed.
        std::fs::write(store.join("agent.toml"), "# Agent identity\nid = \"unnamed-agent\"\n").unwrap();
        let e = read_identity(&store).unwrap_err().to_string();
        assert!(e.contains("not an identified agent store"), "got: {e}");
        // And it must not surface as an agent anywhere else.
        assert!(list_in(h.path()).unwrap().is_empty());
        assert!(find_in(h.path(), "legacy").is_err());
    }

    /// A rebind changes the aid for a fixed name; a rename changes the name for a fixed aid. Both must
    /// leave ONE entry, not two. `upsert` keying on the aid alone appended on rebind and left the repo
    /// bound to two agents wearing one name — the integrity check would then pick whichever came first.
    #[test]
    fn upsert_replaces_on_either_the_aid_or_the_name() {
        let mut b = Binding::default();
        b.upsert(BoundAgent { id: "agt_old".into(), name: "frontend".into(), remotes: vec![] });
        // rebind: same name, new aid → replaces, does not append.
        b.upsert(BoundAgent { id: "agt_new".into(), name: "frontend".into(), remotes: vec![] });
        assert_eq!(b.agents.len(), 1, "a rebind left a duplicate name: {:?}", b.agents);
        assert_eq!(b.agents[0].id, "agt_new");
        // rename: same aid, new name → replaces the same slot.
        b.upsert(BoundAgent { id: "agt_new".into(), name: "web".into(), remotes: vec![] });
        assert_eq!(b.agents.len(), 1, "a rename left a duplicate aid: {:?}", b.agents);
        assert_eq!(b.agents[0].name, "web");
        // a genuinely different agent is a new entry.
        b.upsert(BoundAgent { id: "agt_other".into(), name: "api".into(), remotes: vec![] });
        assert_eq!(b.agents.len(), 2);
    }

    #[test]
    fn binding_round_trips() {
        let b = Binding {
            version: 1,
            agents: vec![
                BoundAgent {
                    id: "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60".into(),
                    name: "frontend".into(),
                    remotes: vec![BoundRemote {
                        name: "origin".into(),
                        url: "https://hub.acme.com/frontend.git".into(),
                        primary: true,
                    }],
                },
                // No remote: an agent exists before it is published.
                BoundAgent { id: "agt_0190f4b7-9d81-7c02-b6aa-2f5e8c7d3a11".into(), name: "api".into(), remotes: vec![] },
            ],
            default: Some("api".into()),
        };
        assert_eq!(Binding::parse(&b.to_toml()).unwrap(), b);

        // The schema of the design doc, read verbatim.
        let doc = r#"
version = 1

[[agent]]
id     = "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60"
name   = "frontend"
remote = "https://hub.acme.com/frontend.git"

[defaults]
agent = "frontend"        # what a FRESH clone activates
"#;
        let p = Binding::parse(doc).unwrap();
        assert_eq!(p.agents.len(), 1);
        assert_eq!(p.agents[0].name, "frontend");
        assert_eq!(p.agents[0].primary_url(), Some("https://hub.acme.com/frontend.git"));
        assert_eq!(p.default.as_deref(), Some("frontend"), "the trailing comment is not part of the value");

        let d = tmp();
        let env = env_repo(d.path());
        assert_eq!(Binding::load(&env).unwrap(), None, "no binding is not an error");
        b.save(&env).unwrap();
        assert_eq!(Binding::load(&env).unwrap(), Some(b));
    }

    #[test]
    fn binding_refuses_an_id_it_cannot_check() {
        // Name/URL identity is exactly what the id exists to defend against.
        let e = Binding::parse("[[agent]]\nid = \"frontend\"\nname = \"frontend\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("not an `agt_"), "got: {e}");
        assert!(Binding::parse("[[agent]]\nid = \"unnamed-agent\"\nname = \"x\"\n").is_err());
        assert!(Binding::parse("version = 99\n").is_err(), "a newer schema must not be half-read");
    }

    #[test]
    fn a_different_aid_at_the_same_name_is_refused() {
        let bound = BoundAgent {
            id: "agt_01J".into(),
            name: "frontend".into(),
            remotes: vec![BoundRemote { name: "origin".into(), url: "https://hub/frontend.git".into(), primary: true }],
        };
        assert!(check_binding(&bound, "agt_01J").is_ok());
        let e = check_binding(&bound, "agt_02X").unwrap_err().to_string();
        assert!(e.contains("this repo is bound to agt_01J (frontend)"), "got: {e}");
        assert!(e.contains("https://hub/frontend.git is agt_02X"), "got: {e}");
        assert!(e.contains("agit agent rebind frontend --remote"), "got: {e}");
    }

    #[test]
    fn resolution_order_each_rung_wins_over_the_next() {
        let h = tmp();
        let flag = new_agent_in(h.path(), "flag-agent").unwrap();
        let var = new_agent_in(h.path(), "var-agent").unwrap();
        let active = new_agent_in(h.path(), "active-agent").unwrap();
        let deflt = new_agent_in(h.path(), "default-agent").unwrap();
        let binding = Binding { default: Some("default-agent".into()), ..Binding::default() };
        let b = Some(&binding);

        let r = |e, v, a| resolve_in(h.path(), e, v, a, b).unwrap().aid;
        assert_eq!(r(Some("flag-agent"), Some("var-agent"), Some("active-agent")), flag.aid);
        assert_eq!(r(None, Some("var-agent"), Some("active-agent")), var.aid);
        assert_eq!(r(None, None, Some("active-agent")), active.aid);
        assert_eq!(r(None, None, None), deflt.aid);

        // An aid selects too — that is what the active pointer stores.
        assert_eq!(r(Some(&active.aid), None, None), active.aid);

        // Rung 5: an actionable error, never a silent fallback.
        let e = resolve_in(h.path(), None, None, None, None).unwrap_err().to_string();
        assert!(e.contains("no agent selected"), "got: {e}");
        assert!(e.contains("agit a switch <name>"), "got: {e}");
        assert!(e.contains("known agents: active-agent"), "got: {e}");
    }

    #[test]
    fn a_blank_rung_falls_through_rather_than_winning() {
        let h = tmp();
        let deflt = new_agent_in(h.path(), "default-agent").unwrap();
        let binding = Binding { default: Some("default-agent".into()), ..Binding::default() };
        for blank in ["", "   ", "\n"] {
            assert_eq!(
                resolve_in(h.path(), None, Some(blank), Some(blank), Some(&binding)).unwrap().aid,
                deflt.aid,
                "a blank rung must fall through to [defaults], not select nothing"
            );
        }
        assert_eq!(pick(None, None, None, None), None);
    }

    #[test]
    fn the_active_pointer_is_per_worktree_and_recoverable() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let agent = new_agent_in(h.path(), "frontend").unwrap();

        // Missing → None → resolution falls back, never errors.
        assert_eq!(read_active(&env).unwrap(), None);
        let binding = Binding { default: Some("frontend".into()), ..Binding::default() };
        assert_eq!(
            resolve_in(h.path(), None, None, read_active(&env).unwrap().as_deref(), Some(&binding)).unwrap().aid,
            agent.aid
        );

        write_active(&env, &agent.aid).unwrap();
        assert_eq!(read_active(&env).unwrap().as_deref(), Some(agent.aid.as_str()));
        assert!(
            active_path(&env).unwrap().starts_with(env.join(".git")),
            "the pointer lives inside .git, so it is untracked by construction and cannot travel"
        );

        // Blank (a truncated write) reads as absent.
        std::fs::write(active_path(&env).unwrap(), "\n").unwrap();
        assert_eq!(read_active(&env).unwrap(), None);

        // Deleting it is fully recoverable — the rule that separates this from the .agit/store hack.
        clear_active(&env).unwrap();
        assert_eq!(read_active(&env).unwrap(), None);
        clear_active(&env).unwrap(); // idempotent

        // Per-worktree: a second worktree of the same repo has its own pointer.
        scope::git_in(&env, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "x"])
            .unwrap();
        let wt = d.path().join("wt");
        scope::git_in(&env, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "side"]).unwrap();
        write_active(&env, &agent.aid).unwrap();
        assert_eq!(read_active(&wt).unwrap(), None, "231 worktrees of one repo must not share one pointer");
        assert_ne!(active_path(&env).unwrap(), active_path(&wt).unwrap());
    }

    #[test]
    fn the_registry_is_a_cache_not_a_truth() {
        let h = tmp();
        let a = new_agent_in(h.path(), "frontend").unwrap();
        let b = new_agent_in(h.path(), "api").unwrap();
        assert_eq!(registry_load(h.path()).get("frontend"), Some(&a.aid));

        // Delete it: every lookup still works, from the stores themselves.
        std::fs::remove_file(registry_path_in(h.path())).unwrap();
        assert_eq!(find_in(h.path(), "api").unwrap().aid, b.aid);
        assert_eq!(registry_load(h.path()).get("api"), Some(&b.aid), "and the cache heals on the way past");

        // Corrupt it: a lie must not win over agent.toml.
        std::fs::write(registry_path_in(h.path()), "{\"agents\":{\"frontend\":\"agt_bogus\"}}").unwrap();
        assert_eq!(find_in(h.path(), "frontend").unwrap().aid, a.aid);
        std::fs::write(registry_path_in(h.path()), "not json at all").unwrap();
        assert_eq!(find_in(h.path(), "frontend").unwrap().aid, a.aid);

        // --repair rebuilds it by scanning.
        let names: Vec<String> = repair_in(h.path()).unwrap().into_iter().map(|x| x.name).collect();
        assert_eq!(names, vec!["api", "frontend"]);
        assert_eq!(registry_load(h.path()).len(), 2);
    }

    #[test]
    fn rename_is_metadata_only() {
        let h = tmp();
        let before = new_agent_in(h.path(), "web").unwrap();
        let after = rename_in(h.path(), "web", "frontend").unwrap();
        assert_eq!(after.aid, before.aid, "identity survives the label");
        assert_eq!(after.store, before.store, "no directory moves — a running watcher is never orphaned");
        assert_eq!(read_identity(&after.store).unwrap().name, "frontend");
        assert!(find_in(h.path(), "web").is_err());
        assert_eq!(find_in(h.path(), "frontend").unwrap().aid, before.aid);
        assert_eq!(find_in(h.path(), &before.aid).unwrap().name, "frontend");
        assert!(
            scope::git_in(&after.store, &["show", "HEAD:agent.toml"]).unwrap().contains("frontend"),
            "the new label is committed, so it travels"
        );
        assert!(scope::git_in(&after.store, &["status", "--porcelain"]).unwrap().is_empty());
    }

    #[test]
    fn names_are_labels_and_must_not_collide_or_impersonate_an_aid() {
        let h = tmp();
        new_agent_in(h.path(), "frontend").unwrap();
        assert!(new_agent_in(h.path(), "frontend").is_err(), "two agents, one name, no way to pick");
        assert!(new_agent_in(h.path(), "agt_x").is_err(), "a name that reads as an aid is ambiguous");
        assert!(new_agent_in(h.path(), "").is_err());
        assert!(new_agent_in(h.path(), "has space").is_err());
        assert!(new_agent_in(h.path(), "quote\"name").is_err());
        assert!(new_agent_in(h.path(), "--agent").is_err());
        assert!(rename_in(h.path(), "frontend", "bad name").is_err());

        // A name `track` could never resolve is not a name. `looks_like_url` reads a leading `.` or
        // `~` as a path, so `agit a clone .tmp9ndKZa` refuses it as an unclassifiable remote rather
        // than finding the agent — i.e. minting one strands the teammate who needs it.
        for path_like in [".tmp9ndKZa", ".hidden", "~home", "./rel"] {
            assert!(
                new_agent_in(h.path(), path_like).is_err(),
                "`{path_like}` must be refused at mint: looks_like_url reads it as a path, so no \
                 teammate could ever `agit a clone {path_like}`"
            );
            assert!(rename_in(h.path(), "frontend", path_like).is_err(), "and rename must not sneak one in");
        }
        assert!(new_agent_in(h.path(), "payments.api").is_ok(), "a dot INSIDE a name is still fine");
    }

    #[test]
    fn a_zeroed_pidfile_is_not_a_live_watcher() {
        // `kill -0 0` succeeds (it signals our own process group), so without the guard a zeroed
        // pidfile would read as a live watcher and wedge import/rebind forever.
        assert!(!pid_alive(0), "pid 0 must never read as alive");
    }

    #[test]
    fn a_push_records_origin_in_the_binding_credential_stripped() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let a = new_agent_in(h.path(), "frontend").unwrap();
        bind_here(&a, &env, false).unwrap();

        // No origin yet — a push has nothing to point a clone at.
        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        assert!(s.recorded.is_empty() && !s.changed, "no remote, nothing recorded: {s:?}");

        // `git remote add` sets an origin that carries a credential, as a person typing a token URL would.
        scope::git_in(
            &a.store,
            &["remote", "add", "origin", "https://alice:ghp_secrettoken00000000000000000000@hub.example.com/frontend.git"],
        )
        .unwrap();

        // The first push records the locator with the credential stripped out.
        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        assert!(s.changed, "the first push must record the origin");
        assert_eq!(s.recorded.len(), 1);
        let rec = &s.recorded[0];
        assert!(rec.stripped, "a credential in the origin must be stripped on the way into the binding");
        assert!(rec.primary, "the sole origin is the primary anchor");
        assert_eq!(rec.locator, "https://hub.example.com/frontend.git");
        assert!(!rec.locator.contains("ghp_"), "no token may reach the committed binding: {}", rec.locator);
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents[0].primary_url(), Some("https://hub.example.com/frontend.git"));

        // Idempotent: pushing again with an unchanged origin writes nothing.
        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        assert!(!s.changed, "an unchanged remote set must not rewrite the binding: {s:?}");

        // An `ext::` origin (the RCE-on-clone transport) is refused for the committed file — the local
        // push is git's business, but agit will not hand a teammate a shell command to clone.
        scope::git_in(&a.store, &["remote", "set-url", "origin", "ext::sh -c evil"]).unwrap();
        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        assert!(s.recorded.is_empty(), "an ext:: origin is not recorded");
        assert_eq!(s.skipped.len(), 1);
        assert!(s.skipped[0].1.contains("ext::"), "got: {:?}", s.skipped);
        // ...and the last good locator still stands, untouched.
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents[0].primary_url(), Some("https://hub.example.com/frontend.git"));
    }

    // ---- multi-remote bindings -------------------------------------------------------------------

    /// An OLD single-remote file (`remote = "url"`) parses as one primary origin remote, and re-emits
    /// the identical legacy line — never a `[[agent.remote]]` sub-table. This is the back-compat anchor.
    #[test]
    fn parses_legacy_single_remote_as_one_primary() {
        let doc = "\
version = 1

[[agent]]
id     = \"agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60\"
name   = \"frontend\"
remote = \"https://hub.acme.com/frontend.git\"
";
        let p = Binding::parse(doc).unwrap();
        assert_eq!(p.agents.len(), 1);
        assert_eq!(p.agents[0].remotes.len(), 1);
        assert_eq!(p.agents[0].remotes[0].name, "origin");
        assert!(p.agents[0].remotes[0].primary, "the sole remote is the primary anchor");
        assert_eq!(p.agents[0].primary_url(), Some("https://hub.acme.com/frontend.git"));

        // Re-serializes to the identical legacy line — no sub-table, byte-for-byte round-trip.
        let out = p.to_toml();
        assert!(out.contains("remote = \"https://hub.acme.com/frontend.git\""), "got:\n{out}");
        assert!(!out.contains("[[agent.remote]]"), "a single origin must stay legacy-shaped:\n{out}");
        assert_eq!(Binding::parse(&out).unwrap(), p, "legacy form must round-trip");
    }

    /// Two `[[agent.remote]]` sub-tables parse into two remotes with exactly one primary, and round-trip
    /// through the sub-table form.
    #[test]
    fn parses_and_roundtrips_multi_remote_subtables() {
        let doc = "\
version = 1

[[agent]]
id     = \"agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60\"
name   = \"frontend\"
[[agent.remote]]
name    = \"origin\"
url     = \"https://git.acme.com/frontend.git\"
primary = true
[[agent.remote]]
name    = \"hub\"
url     = \"https://hub.alice.dev/frontend.git\"
";
        let p = Binding::parse(doc).unwrap();
        assert_eq!(p.agents.len(), 1);
        let rs = &p.agents[0].remotes;
        assert_eq!(rs.len(), 2, "both remotes must parse: {rs:?}");
        assert_eq!(rs.iter().filter(|r| r.primary).count(), 1, "exactly one primary");
        assert!(rs.iter().find(|r| r.name == "origin").unwrap().primary, "origin is the primary");
        assert!(!rs.iter().find(|r| r.name == "hub").unwrap().primary);
        assert_eq!(p.agents[0].primary_url(), Some("https://git.acme.com/frontend.git"));

        // A non-legacy remote set serializes as sub-tables AND a redundant forward-compat legacy line
        // carrying the primary URL, and still round-trips through a current parse.
        let out = p.to_toml();
        assert!(out.contains("[[agent.remote]]"), "multi-remote must use sub-tables:\n{out}");
        assert!(
            out.contains("remote = \"https://git.acme.com/frontend.git\""),
            "the forward-compat legacy line must carry the primary URL:\n{out}"
        );
        assert_eq!(Binding::parse(&out).unwrap(), p, "multi-remote must round-trip");
    }

    /// Forward-compat anchor: a multi-remote binding emits BOTH the legacy `remote = "<primary-url>"`
    /// line AND the `[[agent.remote]]` sub-tables. A current agit parses the sub-tables and treats the
    /// legacy line as redundant (no phantom `origin`); an OLD agit — which reads ONLY the legacy line —
    /// still finds the primary, so it never drops the remotes on a rewrite.
    #[test]
    fn multi_remote_carries_legacy_line_for_old_agit() {
        // A multi-remote agent whose primary is NOT named "origin" — the hard case, where a naive legacy
        // duplicate would otherwise resurrect as a phantom `origin` remote.
        let p = Binding {
            version: 1,
            agents: vec![BoundAgent {
                id: "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60".into(),
                name: "frontend".into(),
                remotes: vec![
                    BoundRemote { name: "hub".into(), url: "https://hub.alice.dev/f.git".into(), primary: true },
                    BoundRemote { name: "backup".into(), url: "https://b/f.git".into(), primary: false },
                ],
            }],
            default: None,
        };
        let out = p.to_toml();

        // BOTH forms present: the legacy line (primary URL) and the sub-tables.
        assert!(out.contains("[[agent.remote]]"), "sub-tables present:\n{out}");
        assert!(
            out.contains("remote = \"https://hub.alice.dev/f.git\""),
            "legacy line carries the PRIMARY url:\n{out}"
        );

        // A CURRENT parse: the sub-tables are authoritative, the legacy line is redundant — exactly two
        // remotes, no phantom `origin`, and it round-trips.
        let cur = Binding::parse(&out).unwrap();
        assert_eq!(cur, p, "current agit round-trips, dropping the redundant legacy line");
        assert_eq!(cur.agents[0].remotes.len(), 2, "no phantom origin remote resurrected");
        assert!(!cur.agents[0].remotes.iter().any(|r| r.name == "origin"));

        // An OLD agit understands ONLY the legacy `remote = "…"` line under `[[agent]]`. Simulate it by
        // stripping the sub-tables: it still finds the PRIMARY, so it never binds to nothing.
        let legacy_only: String = out
            .lines()
            .take_while(|l| !l.trim_start().starts_with("[[agent.remote]]"))
            .collect::<Vec<_>>()
            .join("\n");
        let old = Binding::parse(&legacy_only).unwrap();
        assert_eq!(old.agents.len(), 1);
        assert_eq!(
            old.agents[0].primary_url(),
            Some("https://hub.alice.dev/f.git"),
            "an old agit reads the primary from the legacy line"
        );
    }

    /// Normalization guarantees exactly one primary even for a hand-edited file: none marked → the first
    /// wins; several marked → only the first survives.
    #[test]
    fn normalize_enforces_single_primary() {
        // Zero primary=true anywhere → the first remote becomes primary.
        let none = "\
[[agent]]
id     = \"agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60\"
name   = \"frontend\"
[[agent.remote]]
name    = \"origin\"
url     = \"https://a/x.git\"
[[agent.remote]]
name    = \"hub\"
url     = \"https://b/x.git\"
";
        let p = Binding::parse(none).unwrap();
        assert_eq!(p.agents[0].remotes.iter().filter(|r| r.primary).count(), 1);
        assert!(p.agents[0].remotes[0].primary, "the first remote is promoted when none is marked");

        // Two primary=true → only the first is kept.
        let two = "\
[[agent]]
id     = \"agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60\"
name   = \"frontend\"
[[agent.remote]]
name    = \"origin\"
url     = \"https://a/x.git\"
primary = true
[[agent.remote]]
name    = \"hub\"
url     = \"https://b/x.git\"
primary = true
";
        let p = Binding::parse(two).unwrap();
        assert_eq!(p.agents[0].remotes.iter().filter(|r| r.primary).count(), 1, "only one primary survives");
        assert_eq!(p.agents[0].primary().unwrap().name, "origin", "the first primary wins");
    }

    /// A store with two remotes records BOTH into the binding, origin marked primary, each stripped
    /// through `committed_locator`; `primary_url()` resolves the origin.
    #[test]
    fn sync_records_all_store_remotes_origin_primary() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let a = new_agent_in(h.path(), "frontend").unwrap();
        let bare_a = d.path().join("a.git");
        let bare_b = d.path().join("b.git");
        std::fs::create_dir_all(&bare_a).unwrap();
        std::fs::create_dir_all(&bare_b).unwrap();
        scope::git_in(&bare_a, &["init", "-q", "--bare"]).unwrap();
        scope::git_in(&bare_b, &["init", "-q", "--bare"]).unwrap();
        scope::git_in(&a.store, &["remote", "add", "origin", &format!("file://{}", bare_a.display())]).unwrap();
        scope::git_in(&a.store, &["remote", "add", "hub", &format!("file://{}", bare_b.display())]).unwrap();

        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        assert!(s.changed);
        assert_eq!(s.recorded.len(), 2, "both remotes recorded: {:?}", s.recorded);
        let b = Binding::load(&env).unwrap().unwrap();
        let e = b.find("frontend").unwrap();
        assert_eq!(e.remotes.len(), 2);
        assert!(e.remotes.iter().find(|r| r.name == "origin").unwrap().primary, "origin is primary");
        assert!(!e.remotes.iter().find(|r| r.name == "hub").unwrap().primary);
        assert_eq!(e.primary_url(), Some(format!("file://{}", bare_a.display()).as_str()));
    }

    /// A credentialed hub remote is token-stripped on the way into the binding; the origin is untouched.
    #[test]
    fn sync_strips_credentials_per_remote() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let a = new_agent_in(h.path(), "frontend").unwrap();
        scope::git_in(&a.store, &["remote", "add", "origin", "https://git.acme.com/frontend.git"]).unwrap();
        scope::git_in(
            &a.store,
            &["remote", "add", "hub", "https://alice:ghp_secrettoken00000000000000000000@hub.alice.dev/frontend.git"],
        )
        .unwrap();

        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        let b = Binding::load(&env).unwrap().unwrap();
        let e = b.find("frontend").unwrap();
        let hub = e.remotes.iter().find(|r| r.name == "hub").unwrap();
        assert_eq!(hub.url, "https://hub.alice.dev/frontend.git", "the hub token must be stripped");
        assert!(!hub.url.contains("ghp_"), "no token may reach the binding: {}", hub.url);
        let origin = e.remotes.iter().find(|r| r.name == "origin").unwrap();
        assert_eq!(origin.url, "https://git.acme.com/frontend.git", "the origin is unaffected");
        assert!(origin.primary);
        assert!(s.recorded.iter().any(|r| r.name == "hub" && r.stripped), "hub must be reported stripped");
    }

    /// A good origin plus a non-shareable `ext::` extra: the origin is still recorded, the ext:: hub is
    /// reported skipped, and the binding is not corrupted.
    #[test]
    fn sync_skips_a_nonshareable_extra_without_aborting() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let a = new_agent_in(h.path(), "frontend").unwrap();
        scope::git_in(&a.store, &["remote", "add", "origin", "https://git.acme.com/frontend.git"]).unwrap();
        scope::git_in(&a.store, &["remote", "add", "hub", "ext::sh -c evil"]).unwrap();

        let s = sync_remotes_to_binding_in(h.path(), &a.aid, &env).unwrap();
        assert_eq!(s.recorded.len(), 1, "only the shareable origin is recorded: {:?}", s.recorded);
        assert_eq!(s.recorded[0].name, "origin");
        assert!(s.recorded[0].primary);
        assert_eq!(s.skipped.len(), 1, "the ext:: hub is skipped: {:?}", s.skipped);
        assert_eq!(s.skipped[0].0, "hub");
        assert!(s.skipped[0].1.contains("ext::"));
        // The binding parses cleanly and carries only the good remote.
        let b = Binding::load(&env).unwrap().unwrap();
        let e = b.find("frontend").unwrap();
        assert_eq!(e.remotes.len(), 1);
        assert_eq!(e.primary_url(), Some("https://git.acme.com/frontend.git"));
    }

    /// `primary_remote_name` designates the anchor: `origin` when present, else the first in git's
    /// alphabetical order; `store_remotes` lists every remote with its URL.
    #[test]
    fn store_remotes_and_primary_name_designate_the_anchor() {
        let h = tmp();
        let a = new_agent_in(h.path(), "frontend").unwrap();
        assert!(store_remotes(&a.store).is_empty(), "a fresh store has no remotes");
        assert_eq!(primary_remote_name(&a.store), None);

        // No origin: the first in git's alphabetical order (`alpha` before `zeta`) is the anchor.
        scope::git_in(&a.store, &["remote", "add", "zeta", "https://z/x.git"]).unwrap();
        scope::git_in(&a.store, &["remote", "add", "alpha", "https://a/x.git"]).unwrap();
        let names: Vec<String> = store_remotes(&a.store).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["alpha".to_string(), "zeta".to_string()], "git lists alphabetically");
        assert_eq!(primary_remote_name(&a.store).as_deref(), Some("alpha"));

        // Add origin: it always wins the anchor, wherever it sorts.
        scope::git_in(&a.store, &["remote", "add", "origin", "https://o/x.git"]).unwrap();
        assert_eq!(primary_remote_name(&a.store).as_deref(), Some("origin"));
    }

    /// The resolution a bare `agit a clone <name>` performs: a binding with two remotes resolves to the
    /// PRIMARY (origin) url — the shared anchor a teammate without hub write can still reach.
    #[test]
    fn clone_resolves_primary() {
        let b = Binding {
            version: 1,
            agents: vec![BoundAgent {
                id: "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60".into(),
                name: "frontend".into(),
                remotes: vec![
                    BoundRemote { name: "hub".into(), url: "https://hub.alice.dev/frontend.git".into(), primary: false },
                    BoundRemote { name: "origin".into(), url: "https://git.acme.com/frontend.git".into(), primary: true },
                ],
            }],
            default: Some("frontend".into()),
        };
        // clone_agent looks up `find(target).primary_url()` — the origin, not the personal hub.
        let resolved = b.find("frontend").and_then(|e| e.primary_url().map(str::to_string));
        assert_eq!(resolved.as_deref(), Some("https://git.acme.com/frontend.git"));
    }

    #[test]
    fn bind_here_records_what_a_fresh_clone_needs() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let a = new_agent_in(h.path(), "frontend").unwrap();
        bind_here(&a, &env, false).unwrap();
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents, vec![BoundAgent { id: a.aid.clone(), name: "frontend".into(), remotes: vec![] }]);
        assert_eq!(b.default.as_deref(), Some("frontend"), "the first agent bound becomes the default");

        // Upsert matches on the id: a rename edits the entry, it does not add a second.
        let renamed = rename_in(h.path(), "frontend", "web").unwrap();
        bind_here(&renamed, &env, false).unwrap();
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents.len(), 1);
        assert_eq!(b.agents[0].name, "web");

        let other = new_agent_in(h.path(), "api").unwrap();
        bind_here(&other, &env, true).unwrap();
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents.len(), 2);
        assert_eq!(b.default.as_deref(), Some("api"));
    }

    #[test]
    fn resolve_refuses_when_the_store_is_a_different_agent_than_the_binding_says() {
        let h = tmp();
        let a = new_agent_in(h.path(), "frontend").unwrap();
        // The repo committed a binding to a `frontend` that is NOT this store (recreated remote).
        let binding = Binding {
            version: 1,
            agents: vec![BoundAgent {
                id: "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60".into(),
                name: "frontend".into(),
                remotes: vec![BoundRemote { name: "origin".into(), url: "https://hub/frontend.git".into(), primary: true }],
            }],
            default: Some("frontend".into()),
        };
        let e = resolve_in(h.path(), None, None, None, Some(&binding)).unwrap_err().to_string();
        assert!(e.contains("this repo is bound to"), "got: {e}");
        assert!(e.contains(&a.aid), "the error must name the aid actually found: {e}");

        // The check must not depend on HOW the agent was selected. The active pointer holds an
        // **aid**, which matches no [[agent]] entry (they are found by label) — keying the lookup on
        // the selector let exactly this case through silently.
        for sel in [Some(a.aid.as_str()), Some("frontend")] {
            assert!(
                resolve_in(h.path(), sel, None, None, Some(&binding)).is_err(),
                "a store that is not the bound agent must be refused however it was selected ({sel:?})"
            );
            assert!(resolve_in(h.path(), None, sel, None, Some(&binding)).is_err());
            assert!(resolve_in(h.path(), None, None, sel, Some(&binding)).is_err());
        }

        // An entry that agrees on the id is settled, whatever label it wears (a stale rename hint).
        let ok = Binding {
            agents: vec![BoundAgent { id: a.aid.clone(), name: "old-label".into(), remotes: vec![] }],
            ..Binding::default()
        };
        assert_eq!(resolve_in(h.path(), Some(&a.aid), None, None, Some(&ok)).unwrap().aid, a.aid);
        // An agent the binding says nothing about is not this check's business.
        let unrelated = new_agent_in(h.path(), "infra").unwrap();
        assert_eq!(resolve_in(h.path(), Some("infra"), None, None, Some(&binding)).unwrap().aid, unrelated.aid);
    }

    /// The remote comes from COMMITTED .agit.toml, so it is attacker-controlled input on a repo you
    /// merely cloned. Verified against git 2.43: `git clone 'ext::<cmd>'` executed <cmd> — the clone
    /// then failed, but the payload had already run, and `--` did not stop it (a scheme, not a flag).
    #[test]
    fn a_remote_that_could_run_a_command_is_refused() {
        for evil in [
            "ext::sh -c touch /tmp/pwned",
            "ext::/tmp/evil.sh",
            "fd::7",
            "-u",
            "--upload-pack=touch /tmp/pwned",
            "--template=/tmp/evil",
            "",
            "  ",
        ] {
            assert!(check_remote(evil).is_err(), "must refuse `{evil}`");
        }
    }

    /// The refusal must not eat the remotes people actually use — a security check that blocks the
    /// normal case gets turned off.
    #[test]
    fn the_remotes_people_actually_use_still_clone() {
        for ok in [
            "https://hub.acme.com/frontend.git",
            "http://10.0.0.2:8080/f.git",
            "ssh://git@github.com/me/f.git",
            "git://example.com/f.git",
            "file:///srv/agents/f.git",
            "git@github.com:me/f.git",
            "/srv/agents/f.git",
            "./local-store",
            "~/stores/f.git",
        ] {
            assert!(check_remote(ok).is_ok(), "must allow `{ok}`: {:?}", check_remote(ok).err());
        }
    }

    /// A store holds whole transcripts, so it holds whatever the agent saw — and pushing one publishes
    /// that to the team. Under the old model `agit init` built the store, so init could be the only
    /// place the gate was installed; identity mints stores now, and every door must fit it.
    #[test]
    fn every_store_gets_the_secret_gate_however_it_was_created() {
        let h = tmp();
        let minted = new_agent_in(h.path(), "frontend").unwrap();
        for hook in ["pre-commit", "pre-push"] {
            assert!(
                minted.store.join(".git/hooks").join(hook).exists(),
                "agit a init must install {hook} — a store minted without it scans nothing, silently"
            );
        }
    }

    /// A bare `git init --bare` repo cloned by agit: the store exists but nothing was ever pushed. This
    /// must NOT die with the raw `agent.toml … No such file (os error 2)` — it is the empty-store case,
    /// and the message must be clear and actionable (`--init` to mint, `agit a push` to publish).
    #[test]
    fn cloning_an_empty_store_reports_it_actionably_not_an_os_error() {
        let h = tmp();
        let d = tmp();
        let bare = d.path().join("empty.git");
        Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&bare).status().unwrap();

        let e = clone_in(h.path(), bare.to_str().unwrap(), false).unwrap_err().to_string();
        assert!(e.contains("empty store"), "must name the empty-store case: {e}");
        assert!(e.contains("--init"), "must point at --init to mint one: {e}");
        assert!(e.contains("agit a push"), "must point at push to publish an existing one: {e}");
        assert!(!e.contains("os error"), "must not surface the raw os-error: {e}");
        assert!(!e.contains("No such file"), "must not surface the raw os-error: {e}");
    }

    /// `agit a clone --init <empty-store>` mints a fresh agent into it and pushes, so the once-empty
    /// store becomes a valid, adoptable agent — a second machine can clone it by identity.
    #[test]
    fn init_mints_a_fresh_agent_into_an_empty_store_and_publishes_it() {
        let h1 = tmp();
        let d = tmp();
        let bare = d.path().join("frontend.git");
        Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&bare).status().unwrap();

        // --init against the empty store mints locally and pushes.
        let a = clone_in(h1.path(), bare.to_str().unwrap(), true).unwrap();
        assert!(a.aid.starts_with("agt_"), "a fresh identity was minted: {}", a.aid);
        assert_eq!(a.name, "frontend", "the name is derived from the store URL's last segment");
        assert!(read_identity(&a.store).unwrap().aid == a.aid, "the local store carries the minted identity");

        // The store is no longer empty: the bare remote now has an agent.toml on main.
        let toml = scope::git_in(&bare, &["show", "main:agent.toml"]).unwrap();
        assert!(toml.contains(&a.aid), "the pushed store publishes the minted aid: {toml}");

        // And a DIFFERENT machine can now adopt it by a plain clone (no --init) — it is a real agent.
        let h2 = tmp();
        let adopted = clone_in(h2.path(), bare.to_str().unwrap(), false).unwrap();
        assert_eq!(adopted.aid, a.aid, "the second machine adopts the same identity");
    }

    /// `--init` is only for an EMPTY store. Pointed at a store that already carries an agent, it must
    /// refuse (drop --init to adopt it), never silently mint a colliding second identity.
    #[test]
    fn init_refuses_a_store_that_already_has_an_agent() {
        let h1 = tmp();
        let d = tmp();
        let bare = d.path().join("taken.git");
        Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&bare).status().unwrap();
        // Populate the store with a real agent first.
        clone_in(h1.path(), bare.to_str().unwrap(), true).unwrap();

        // A second --init against the now-populated store is refused.
        let h2 = tmp();
        let e = clone_in(h2.path(), bare.to_str().unwrap(), true).unwrap_err().to_string();
        assert!(e.contains("already has an agent"), "must refuse a non-empty store under --init: {e}");
        assert!(e.contains("--init"), "must tell the user to drop --init: {e}");
    }

    #[test]
    fn store_name_is_derived_from_the_url_last_segment() {
        assert_eq!(store_name_from_url("https://hub/alice/frontend.git").as_deref(), Some("frontend"));
        assert_eq!(store_name_from_url("https://hub/alice/frontend").as_deref(), Some("frontend"));
        assert_eq!(store_name_from_url("git@github.com:me/payments-api.git").as_deref(), Some("payments-api"));
        assert_eq!(store_name_from_url("/srv/agents/infra.git/").as_deref(), Some("infra"));
        assert_eq!(store_name_from_url("https://hub/x/y.git?token=zzz").as_deref(), Some("y"));
    }

    #[test]
    fn url_shapes_are_not_mistaken_for_names() {
        for u in ["https://hub/f.git", "git@github.com:me/f.git", "/srv/agents/f.git", "./f", "~/f"] {
            assert!(looks_like_url(u), "{u}");
        }
        for n in ["frontend", "payments-api", "agt_abc"] {
            assert!(!looks_like_url(n), "{n}");
        }
    }

    /// The binding is COMMITTED, so `publish` must write a LOCATOR, never a credential — a token here is
    /// A name you can mint locally must be one the hub can host, and vice versa — otherwise you hit a
    /// mint-then-fail-at-publish trap. The two validators live in different modules (client vs server),
    /// so this asserts they agree on a battery of names rather than trusting they were kept in step.
    #[test]
    fn local_and_hub_name_rules_agree() {
        for good in ["frontend", "payments-api", "payments.api", "a_b", "x1", &"z".repeat(64)] {
            assert!(validate_name(good).is_ok(), "local rejects a good name: {good}");
            assert!(crate::hub::net::valid_agent_name(good), "hub rejects a good name: {good}");
        }
        for bad in ["a..b", "-x", ".x", "~x", "", "a/b", "a b", "x..", &"z".repeat(65), "agt_00000000-0000-4000-8000-000000000000"] {
            let local_ok = validate_name(bad).is_ok();
            let hub_ok = crate::hub::net::valid_agent_name(bad);
            assert!(!local_ok, "local accepts a bad name: {bad}");
            // the aid-shaped name is a local-only concern (ambiguity with `agit a switch`), not a hub path
            // rule, so exempt exactly that one from the hub side of the agreement.
            if !is_aid(bad) {
                assert!(!hub_ok, "hub accepts a name local rejects: {bad}");
            }
        }
    }

    /// The backstop behind `locator`: a secret in a place `locator` does not strip (a query string, the
    /// path) must still not reach the committed binding. `committed_locator` scans the result and refuses.
    #[test]
    fn committed_locator_refuses_a_secret_locator_cannot_strip() {
        // an AKIA key in the query string — locator only touches userinfo, so it survives the strip, and
        // the scanner must catch it before it is committed.
        assert!(committed_locator("https://github.com/me/f.git?aws=AKIAIOSFODNN7EXAMPLE").is_err());
        // a clean locator passes through unchanged.
        assert_eq!(
            committed_locator("https://x:ghp_tok@github.com/me/f.git").unwrap(),
            "https://github.com/me/f.git"
        );
        assert_eq!(committed_locator("git@github.com:me/f.git").unwrap(), "git@github.com:me/f.git");
    }

    /// pushed to the whole team from the one file agit tells you to commit.
    #[test]
    fn a_published_remote_carries_no_credential() {
        // http(s): the entire userinfo goes — git needs none of it, and a bare username is still an
        // account name in a shared file.
        assert_eq!(
            locator("https://x:ghp_SECRET123@github.com/me/f.git"),
            "https://github.com/me/f.git"
        );
        assert_eq!(locator("http://user@10.0.0.2:8080/f.git"), "http://10.0.0.2:8080/f.git");
        // ssh/scp: the user is part of the ADDRESS (`git@github.com` stops resolving without it), so it
        // stays; only a password would be stripped.
        assert_eq!(locator("git@github.com:me/f.git"), "git@github.com:me/f.git");
        assert_eq!(locator("ssh://git@github.com/me/f.git"), "ssh://git@github.com/me/f.git");
        // scp-like with a (non-standard) password: strip the password, keep the user as the address.
        assert_eq!(locator("x:ghp_SECRET123@github.com:me/f.git"), "x@github.com:me/f.git");
        // a local path containing `@` is NOT scp userinfo — left alone.
        assert_eq!(locator("/srv/a@b/f.git"), "/srv/a@b/f.git");
        // no credential to strip — unchanged.
        assert_eq!(locator("https://github.com/me/f.git"), "https://github.com/me/f.git");
        assert_eq!(locator("/srv/agents/f.git"), "/srv/agents/f.git");
        // a `@` in the PATH is not userinfo and must not be mistaken for it.
        assert_eq!(locator("https://github.com/me/f@v2.git"), "https://github.com/me/f@v2.git");
        // a password CONTAINING `@` — the userinfo boundary is the LAST `@` in the authority, not the
        // first. Splitting on the first `@` leaked the password tail into the committed binding.
        assert_eq!(locator("https://user:p@ss@github.com/me/f.git"), "https://github.com/me/f.git");
        // the property that actually matters: no output of `locator` contains a known secret token.
        for u in [
            "https://x:ghp_SECRET123@github.com/me/f.git",
            "https://ghp_SECRET123@github.com/me/f.git",
            "http://user:ghp_SECRET123@host/f.git",
            "https://u:p@ghp_SECRET123@host/f.git",
            "https://x:ghp_SECRET123@host/path/with@at.git",
            "x:ghp_SECRET123@host:me/f.git",
        ] {
            assert!(!locator(u).contains("ghp_SECRET123"), "credential survived in {}", locator(u));
        }
    }
}
