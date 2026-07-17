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

/// Record an agent in the committed binding. `agit a clone` and `agit a init` both go through here, so
/// a teammate's fresh clone can find the same agents.
pub fn bind_here(agent: &Agent, env_root: &Path, set_default: bool) -> Result<()> {
    crate::init::ensure_gitignore(env_root)?;
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
pub fn clone_agent(target: &str, activate: bool) -> Result<Agent> {
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
                             \x20      agit a clone <url>   clone it from somewhere"
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
    // Hooks live in .git/hooks, which no clone carries: a tracked agent would otherwise push a secret
    // its owner's machine would have caught.
    crate::init::install_hooks(&dest)?;
    registry_put(home, &id.name, &id.aid)?;
    Ok(agent_at(dest, id))
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

/// The result of reconciling the committed binding with the store's own `origin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingSync {
    /// No `origin` on the store — nothing a clone could be pointed at.
    NoOrigin,
    /// `origin` is set but is not a transport agit will write into a committed file (an `ext::`
    /// helper, a `-`-leading URL). The binding is left untouched; the offending URL is carried back
    /// so the caller can say why. A local push still works — this only refuses to *record* it.
    NotShareable(String),
    /// The binding already records this locator; nothing was written.
    Unchanged(String),
    /// The binding was updated to this credential-stripped locator. `stripped` is true when a
    /// credential was removed from the store's raw origin on the way in.
    Recorded { locator: String, stripped: bool },
}

/// Reconcile the committed binding with the store's current `origin`: if `origin` is a shareable
/// transport, record its credential-stripped locator next to the aid, so a teammate's fresh clone can
/// find this agent. Idempotent — an unchanged origin writes nothing.
///
/// This is what makes `agit a push` the agent-context push rather than a bare git push: the locator is
/// only useful to anyone else once it is committed next to the aid, and a push is exactly the moment a
/// store first gains (or changes) the remote worth recording.
pub fn sync_origin_to_binding(aid: &str, env_root: &Path) -> Result<BindingSync> {
    sync_origin_to_binding_in(&scope::agit_home()?, aid, env_root)
}

fn sync_origin_to_binding_in(home: &Path, aid: &str, env_root: &Path) -> Result<BindingSync> {
    // Re-read from the store's own origin, so this is the same answer every other reader gets.
    let a = find_in(home, aid)?;
    let Some(raw) = a.remote.clone() else { return Ok(BindingSync::NoOrigin) };
    // The binding is COMMITTED and cloned by other machines: an `ext::`/`-` origin must never land in
    // it (that is the RCE-on-clone vector). Refuse to record it — the local push is git's own business.
    if check_remote(&raw).is_err() {
        return Ok(BindingSync::NotShareable(raw));
    }
    let locator = committed_locator(&raw)?;
    let current = Binding::load(env_root)?
        .and_then(|b| b.agents.into_iter().find(|e| e.id == a.aid).and_then(|e| e.remote));
    if current.as_deref() == Some(locator.as_str()) {
        return Ok(BindingSync::Unchanged(locator));
    }
    let stripped = locator != raw;
    bind_here(&Agent { remote: Some(locator.clone()), ..a }, env_root, false)?;
    Ok(BindingSync::Recorded { locator, stripped })
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
    b.upsert(BoundAgent { id: agent.aid.clone(), name: agent.name.clone(), remote: committed.clone() });
    b.save(&env)?;
    write_active(&env, &agent.aid)?;
    // Return the LOCATOR, not the store's raw origin: a caller printing `remote` must not echo a token
    // to the terminal or a CI log (the same hygiene publish already keeps).
    Ok(Agent { remote: committed, ..agent })
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
        b.upsert(BoundAgent { id: "agt_old".into(), name: "frontend".into(), remote: None });
        // rebind: same name, new aid → replaces, does not append.
        b.upsert(BoundAgent { id: "agt_new".into(), name: "frontend".into(), remote: None });
        assert_eq!(b.agents.len(), 1, "a rebind left a duplicate name: {:?}", b.agents);
        assert_eq!(b.agents[0].id, "agt_new");
        // rename: same aid, new name → replaces the same slot.
        b.upsert(BoundAgent { id: "agt_new".into(), name: "web".into(), remote: None });
        assert_eq!(b.agents.len(), 1, "a rename left a duplicate aid: {:?}", b.agents);
        assert_eq!(b.agents[0].name, "web");
        // a genuinely different agent is a new entry.
        b.upsert(BoundAgent { id: "agt_other".into(), name: "api".into(), remote: None });
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
        assert_eq!(sync_origin_to_binding_in(h.path(), &a.aid, &env).unwrap(), BindingSync::NoOrigin);

        // `git remote add` sets an origin that carries a credential, as a person typing a token URL would.
        scope::git_in(
            &a.store,
            &["remote", "add", "origin", "https://alice:ghp_secrettoken00000000000000000000@hub.example.com/frontend.git"],
        )
        .unwrap();

        // The first push records the locator with the credential stripped out.
        match sync_origin_to_binding_in(h.path(), &a.aid, &env).unwrap() {
            BindingSync::Recorded { locator, stripped } => {
                assert!(stripped, "a credential in the origin must be stripped on the way into the binding");
                assert_eq!(locator, "https://hub.example.com/frontend.git");
                assert!(!locator.contains("ghp_"), "no token may reach the committed binding: {locator}");
            }
            other => panic!("expected Recorded, got {other:?}"),
        }
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents[0].remote.as_deref(), Some("https://hub.example.com/frontend.git"));

        // Idempotent: pushing again with an unchanged origin writes nothing.
        assert!(matches!(
            sync_origin_to_binding_in(h.path(), &a.aid, &env).unwrap(),
            BindingSync::Unchanged(_)
        ));

        // An `ext::` origin (the RCE-on-clone transport) is refused for the committed file — the local
        // push is git's business, but agit will not hand a teammate a shell command to clone.
        scope::git_in(&a.store, &["remote", "set-url", "origin", "ext::sh -c evil"]).unwrap();
        match sync_origin_to_binding_in(h.path(), &a.aid, &env).unwrap() {
            BindingSync::NotShareable(url) => assert!(url.contains("ext::"), "got: {url}"),
            other => panic!("expected NotShareable, got {other:?}"),
        }
        // ...and the last good locator still stands, untouched.
        let b = Binding::load(&env).unwrap().unwrap();
        assert_eq!(b.agents[0].remote.as_deref(), Some("https://hub.example.com/frontend.git"));
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
