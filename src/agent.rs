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

/// Mint a new aid. Minted **once**, at `agit a new`, before any remote exists.
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

/// Read a store's identity. Refuses the legacy placeholder: `crate::init::scaffold` writes
/// `id = "unnamed-agent"` into **every** store on earth, so accepting it would give every store one
/// shared "identity". The hub already rejects it (`hub::identity`); this stays consistent by reusing
/// that parser rather than growing a second opinion.
pub fn read_identity(store: &Path) -> Result<StoreIdentity> {
    let p = store.join("agent.toml");
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("{} has no agent.toml — it is not an agent store", store.display()))?;
    let aid = match parse_agent_toml(&text) {
        Identity::Aid(a) => a,
        Identity::Unidentified => bail!(
            "{} carries no agent identity (no `agt_…` id in agent.toml).\n\
             \x20      A store minted before identity existed writes the placeholder `unnamed-agent`, which every\n\
             \x20      store shares and so can never be an identity. Adopt it with: agit a import",
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
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        bail!("`{name}` is not a usable agent name (use letters, digits, `-`, `_`, `.`; max 64)");
    }
    if is_aid(name) {
        bail!("`{name}` looks like an aid; a name must be a label, or `agit a use {name}` becomes ambiguous");
    }
    Ok(())
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

/// `$AGIT_HOME/agents/<aid>/`.
pub fn store_path(aid: &str) -> Result<PathBuf> {
    if !is_aid(aid) {
        bail!("`{aid}` is not an aid (expected `agt_…`)");
    }
    Ok(agents_dir_in(&scope::agit_home()?).join(aid))
}

fn store_remote(store: &Path) -> Option<String> {
    match scope::git_in_status(store, &["remote", "get-url", "origin"]) {
        (0, url) if !url.trim().is_empty() => Some(url.trim().to_string()),
        _ => None,
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

pub fn registry_path() -> Result<PathBuf> {
    Ok(registry_path_in(&scope::agit_home()?))
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

/// One `[[agent]]` entry. `id` is the **integrity check**: if the store behind `remote` carries a
/// different aid, agit refuses rather than binding you to a different agent wearing the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAgent {
    pub id: String,
    pub name: String,
    pub remote: Option<String>,
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
            if let Some(r) = &a.remote {
                check_toml_value("remote", r)?;
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
                        b.agents.push(BoundAgent { id: String::new(), name: String::new(), remote: None });
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
                        "remote" => cur.remote = Some(v).filter(|s| !s.is_empty()),
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
            if let Some(r) = &a.remote {
                s.push_str(&format!("remote = \"{r}\"\n"));
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
    pub fn upsert(&mut self, e: BoundAgent) {
        match self.agents.iter_mut().find(|a| a.id == e.id) {
            Some(cur) => *cur = e,
            None => self.agents.push(e),
        }
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
        bound.remote.as_deref().unwrap_or("the local store"),
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

/// Record an agent in the committed binding. `agit a track` and `agit a new` both go through here, so
/// a teammate's fresh clone can find the same agents.
pub fn bind_here(agent: &Agent, env_root: &Path, set_default: bool) -> Result<()> {
    let mut b = Binding::load(env_root)?.unwrap_or_default();
    b.upsert(BoundAgent { id: agent.aid.clone(), name: agent.name.clone(), remote: agent.remote.clone() });
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
            Source::Active => "the active agent (agit a use)",
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
    s.push_str("  agit a use <name>           set this worktree's agent\n");
    s.push_str("  agit a new <name>           mint one\n");
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
         \x20      agit a new {sel}       mint it\n\
         \x20      agit a track <url>     clone it from its remote"
    );
}

/// `agit a use <name|aid>` — sets MY default in THIS worktree. A default, not a lock: two agents can
/// run in one repo at once, each selected per-invocation with `--agent`.
pub fn use_agent(sel: &str) -> Result<Agent> {
    let home = scope::agit_home()?;
    let env = scope::env_root()?;
    let agent = find_in(&home, sel)?;
    if let Some(b) = Binding::load(&env)? {
        check_resolved(&b, &agent)?;
    }
    write_active(&env, &agent.aid)?;
    Ok(agent)
}

/// `agit a new <name>` — mint an agent. Works with no remote: identity exists before any URL does.
pub fn new_agent(name: &str) -> Result<Agent> {
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
fn scaffold_store(store: &Path, id: &StoreIdentity) -> Result<()> {
    scope::git_in(store, &["init", "-q", "-b", "main"])?;
    scope::git_in(store, &["config", "user.name", "agit"])?;
    scope::git_in(store, &["config", "user.email", "agit@local"])?;
    write_agent_toml(store, id)?;
    std::fs::create_dir_all(store.join("sessions"))?;
    std::fs::write(store.join("sessions/.gitkeep"), "")?;
    scope::git_in(store, &["add", "-A"])?;
    scope::git_in(store, &["commit", "-q", "-m", &format!("agit: mint agent {} ({})", id.name, id.aid)])?;
    Ok(())
}

/// `agit a track <name|url>` — adopt an agent this machine does not have yet. A bare name is looked
/// up in the committed binding, which is how a fresh clone gets its team's agents.
///
/// Tracking **activates** by default (`--no-use` opts out): `track` then `use` was two commands for
/// one intent.
pub fn track(target: &str, activate: bool) -> Result<Agent> {
    let home = scope::agit_home()?;
    let env = scope::env_root().ok();
    let binding = match env.as_deref() {
        Some(e) => Binding::load(e)?,
        None => None,
    };

    let agent = match find_in(&home, target) {
        // Already on disk: `track` is idempotent, and re-cloning would be a second copy of one memory.
        Ok(a) if !looks_like_url(target) => a,
        _ => {
            let url = if looks_like_url(target) {
                target.to_string()
            } else {
                binding
                    .as_ref()
                    .and_then(|b| b.find(target))
                    .and_then(|e| e.remote.clone())
                    .with_context(|| {
                        format!(
                            "no agent `{target}` on this machine, and {BINDING_FILE} declares no remote for it.\n\
                             \x20      agit a track <url>   clone it from somewhere"
                        )
                    })?
            };
            clone_in(&home, &url)?
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

/// Transports agit will hand to `git clone`. An allowlist, because the danger is a REMOTE THIS MACHINE
/// DID NOT CHOOSE: `.agit.toml` is committed, so `agit a track frontend` clones a URL whoever wrote the
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
fn clone_in(home: &Path, url: &str) -> Result<Agent> {
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
        bail!("git clone {url} failed");
    }
    let id = match read_identity(&tmp) {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e).with_context(|| format!("{url} is not an agent store"));
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
    registry_put(home, &id.name, &id.aid)?;
    Ok(agent_at(dest, id))
}

/// `agit a rename <old> <new>` — metadata only. The store is keyed by the aid, so nothing moves and
/// a running watcher keeps working.
pub fn rename(old: &str, new: &str) -> Result<Agent> {
    let home = scope::agit_home()?;
    let agent = rename_in(&home, old, new)?;
    // The binding names agents by label, so a rename that skipped it would leave `[defaults] api`
    // pointing at nothing.
    if let Ok(env) = scope::env_root() {
        if let Some(mut b) = Binding::load(&env)? {
            if b.find(&agent.aid).is_some() {
                let was_default = b.default.as_deref() == Some(old);
                b.upsert(BoundAgent {
                    id: agent.aid.clone(),
                    name: agent.name.clone(),
                    remote: agent.remote.clone(),
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
        &["commit", "-q", "-m", &format!("agit: rename agent {old} -> {new}")],
    )?;
    let mut m = registry_load(home);
    m.remove(old);
    m.insert(new.to_string(), id.aid.clone());
    registry_save(home, &m)?;
    Ok(agent_at(agent.store, id))
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
        // Byte-for-byte what crate::init::scaffold writes into every store on earth.
        std::fs::write(store.join("agent.toml"), "# Agent identity\nid = \"unnamed-agent\"\n").unwrap();
        let e = read_identity(&store).unwrap_err().to_string();
        assert!(e.contains("no agent identity"), "got: {e}");
        // And it must not surface as an agent anywhere else.
        assert!(list_in(h.path()).unwrap().is_empty());
        assert!(find_in(h.path(), "legacy").is_err());
    }

    #[test]
    fn binding_round_trips() {
        let b = Binding {
            version: 1,
            agents: vec![
                BoundAgent {
                    id: "agt_0190f3a1-4c2b-7f1e-8a3d-9b2c1d4e5f60".into(),
                    name: "frontend".into(),
                    remote: Some("https://hub.acme.com/frontend.git".into()),
                },
                // No remote: an agent exists before it is published.
                BoundAgent { id: "agt_0190f4b7-9d81-7c02-b6aa-2f5e8c7d3a11".into(), name: "api".into(), remote: None },
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
        assert_eq!(p.agents[0].remote.as_deref(), Some("https://hub.acme.com/frontend.git"));
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
            remote: Some("https://hub/frontend.git".into()),
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
        assert!(e.contains("agit a use <name>"), "got: {e}");
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
    }

    #[test]
    fn bind_here_records_what_a_fresh_clone_needs() {
        let h = tmp();
        let d = tmp();
        let env = env_repo(d.path());
        let a = new_agent_in(h.path(), "frontend").unwrap();
        bind_here(&a, &env, false).unwrap();
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents, vec![BoundAgent { id: a.aid.clone(), name: "frontend".into(), remote: None }]);
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
                remote: Some("https://hub/frontend.git".into()),
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
            agents: vec![BoundAgent { id: a.aid.clone(), name: "old-label".into(), remote: None }],
            ..Binding::default()
        };
        assert_eq!(resolve_in(h.path(), Some(&a.aid), None, None, Some(&ok)).unwrap().aid, a.aid);
        // An agent the binding says nothing about is not this check's business.
        let unrelated = new_agent_in(h.path(), "infra").unwrap();
        assert_eq!(resolve_in(h.path(), Some("infra"), None, None, Some(&binding)).unwrap().aid, unrelated.aid);
    }

    #[test]
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

    fn url_shapes_are_not_mistaken_for_names() {
        for u in ["https://hub/f.git", "git@github.com:me/f.git", "/srv/agents/f.git", "./f", "~/f"] {
            assert!(looks_like_url(u), "{u}");
        }
        for n in ["frontend", "payments-api", "agt_abc"] {
            assert!(!looks_like_url(n), "{n}");
        }
    }
}
