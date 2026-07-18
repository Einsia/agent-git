//! `agit init` — make this code repo ready to work with an agent.
//!
//! Init no longer *places* a store, which was its whole job before identity. An agent is a memory that
//! outlives any one repo, so it lives at `$AGIT_HOME/agents/<aid>/` and this repo only records
//! **which** agents it works with, in the committed `.agit.toml`.
//!
//! What is left is genuinely init's, and nothing else does all of it in one place:
//!   * keep `.agit/` (this environment's local state) out of the code history;
//!   * make sure this repo resolves to an agent — minting one only when a human names it;
//!   * install (or **repair**) the store's secret gate — `.git/hooks` is carried by no clone, and a
//!     store cloned by an older agit has none;
//!   * say which agent this repo is now pointed at, and what to type next.

use crate::agent::{self, Binding};
use crate::scope;
use crate::{errln, outln};
use anyhow::{bail, Context, Result};
use std::io::{IsTerminal, Write};
use std::path::Path;

pub fn run() -> Result<i32> {
    run_named(None)
}

/// `agit init [--agent <name>]`.
pub fn run_named(name: Option<String>) -> Result<i32> {
    let env = scope::env_root().context("agit init must be run inside a git repository (your code repo)")?;

    // The store no longer lives here, but `.agit/` still holds this environment's local state (the
    // workspace log, the watcher's pidfile and log) — none of which belongs in the code history.
    ensure_gitignore(&env)?;

    let agent = match agent::resolve(None) {
        Ok(a) => a,
        // The fresh-clone takeover (§13.1): the committed binding names agents this machine lacks, so
        // init clones the ones that carry a remote and resolves again. This is the whole of PRD #1 —
        // `git clone` then `agit init` and the team's memory is here — so init doing it, rather than
        // making the user `track` each name by hand, is the point.
        Err(_) if declares_agents(&env) => match track_declared(&env)? {
            Some(a) => a,
            // Declared, but nothing could be cloned (no remote recorded, or every clone failed). The
            // resolver's own words name what to do; do not mint a namesake on top of a declared agent.
            None => {
                errln!("{:#}", agent::resolve(None).unwrap_err());
                return Ok(2);
            }
        },
        Err(_) => agent::init_agent(&pick_name(&env, name.as_deref())?)?,
    };

    // Idempotent, and re-run deliberately: a store cloned by an older agit, or one whose .git/hooks
    // was blown away, gets the gate back by running init.
    install_hooks(&agent.store)?;

    agent::bind_here(&agent, &env, false)?;
    agent::write_active(&env, &agent.aid)?;

    outln!("agit is ready.");
    outln!("  Environment : {}", env.display());
    outln!("  Agent       : {} ({})", agent.name, agent.aid);
    outln!("  Store       : {}", agent.store.display());
    outln!("  Binding     : {}   (commit it — your team gets this agent on clone)", agent::BINDING_FILE);
    outln!();
    outln!("  agit start              launch a session already carrying this agent's latest context");
    outln!("  agit snap               capture this project's sessions into the store");
    outln!("  agit a push / pull      sync the memory with your team");
    outln!("  agit a merge <agent>    reconcile this agent's memory with another agent's, by dialogue");
    Ok(0)
}

/// Whether the committed binding names agents — i.e. this is a teammate's clone, and the answer to
/// "no agent" is to clone what it declares, not to mint a stranger.
fn declares_agents(env: &Path) -> bool {
    Binding::load(env).ok().flatten().map(|b| !b.agents.is_empty()).unwrap_or(false)
}

/// Clone every declared agent this machine does not already have and that carries a remote, then return
/// the one the binding defaults to (so init can activate it). This is the fresh-clone takeover.
///
/// A declared agent with NO remote cannot be cloned — the owner minted it but never published — so it is
/// reported and skipped rather than failing the whole takeover: the agents that CAN be pulled still are.
fn track_declared(env: &Path) -> Result<Option<agent::Agent>> {
    let Some(binding) = Binding::load(env)? else { return Ok(None) };
    let mut got: Vec<agent::Agent> = Vec::new();
    for entry in &binding.agents {
        // Already here (a re-run, or an agent shared by two repos): track is idempotent, but skipping
        // avoids a needless network round-trip on every init.
        if agent::info(&entry.id).is_ok() {
            if let Ok(a) = agent::info(&entry.id) {
                got.push(a);
            }
            continue;
        }
        if entry.primary_url().is_none() {
            errln!("  · {} is declared but has no remote to clone — skipping (its owner has not published it)", entry.name);
            continue;
        }
        match agent::clone_agent(&entry.name, false) {
            Ok(a) => {
                outln!("  ✓ cloned {} ({})", a.name, a.aid);
                got.push(a);
            }
            // One agent that fails to clone must not sink the others: a private repo the teammate lacks
            // a token for is a real, recoverable situation, not a reason to abort the takeover.
            Err(e) => errln!("  · could not clone {}: {e:#}", entry.name),
        }
    }
    if got.is_empty() {
        return Ok(None);
    }
    // Prefer the binding's default; else the first one that came down.
    let default = binding.default.as_deref();
    Ok(got
        .iter()
        .find(|a| Some(a.name.as_str()) == default)
        .or_else(|| got.first())
        .cloned())
}

/// An agent is named for what it KNOWS (`frontend`, `payments-api`) — not for the directory it
/// happened to be initialised in. It works across many repos, so the cwd is the worst available label,
/// and naming from it is a guess dressed as an answer.
///
/// So the name is always a human's decision: `--agent <name>`, or one prompt. A script that gives
/// neither gets an actionable error, never a name agit invented — the same rule as resolution, where
/// agit will not guess which memory you meant.
fn pick_name(env: &Path, given: Option<&str>) -> Result<String> {
    if let Some(n) = given.map(str::trim).filter(|n| !n.is_empty()) {
        return Ok(n.to_string());
    }
    if !std::io::stdin().is_terminal() {
        bail!(
            "no agent here yet, and agit will not name one for you.\n\
             \x20      An agent is a memory, named for what it knows — `frontend`, `payments-api` — and it outlives\n\
             \x20      this repo, so `{}` would be the wrong label even when it is a legal one.\n\
             \x20        agit init --agent <name>   mint one here\n\
             \x20        agit a clone <name|url>    use one you (or your team) already have",
            env.file_name().and_then(|s| s.to_str()).unwrap_or("this directory")
        );
    }
    // Interactive: the directory is offered as a *suggestion* a human can see and reject — which is
    // the point. It is only a default when someone looked at it and pressed Enter.
    let dir = env.file_name().and_then(|s| s.to_str()).unwrap_or_default().to_string();
    let suggest = crate::agent::is_usable_name(&dir).then_some(dir);
    loop {
        match &suggest {
            Some(d) => print!("Agent name — what will this agent know? [{d}]: "),
            None => print!("Agent name — what will this agent know?: "),
        }
        std::io::stdout().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("no agent name given (stdin closed) — agit init --agent <name>");
        }
        match (line.trim(), &suggest) {
            ("", Some(d)) => return Ok(d.clone()),
            ("", None) => outln!("  a name is needed, and this directory does not suggest a usable one."),
            (n, _) => match crate::agent::is_usable_name(n) {
                true => return Ok(n.to_string()),
                // Re-ask rather than abort: they are already at the prompt, and losing the whole
                // command over a typo is the kind of thing that teaches people to script around it.
                false => outln!("  `{n}` is not a usable agent name (letters, digits, `-`, `_`, `.`; max 64)."),
            },
        }
    }
}

/// Keep `.agit/` — this environment's local state (the workspace log, the watcher's pidfile and log)
/// — out of the code history.
///
/// Called from `bind_here` as well as `init`, because init is no longer the only door: `a new`,
/// `a track` and `a import` all tie an agent to a repo, and `workspace::record` starts writing
/// `.agit/` the moment either repo commits. Whichever way you arrived, agit's local scratch must not
/// show up as untracked noise in your project.
pub(crate) fn ensure_gitignore(env: &Path) -> Result<()> {
    let gi = env.join(".gitignore");
    let existing = std::fs::read_to_string(&gi).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".agit/" || l.trim() == ".agit") {
        return Ok(());
    }
    let mut s = existing;
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(".agit/\n");
    std::fs::write(&gi, s)?;
    outln!("  appended to the code repo .gitignore: .agit/");
    Ok(())
}

/// The secret gate, installed into a store's `.git/hooks`.
///
/// Every store gets this **at creation** — `agit a init`, `a track`, `a import` — not only at `agit
/// init`. Under the old model init was what built the store, so init was the only door; now identity
/// mints stores, and a store that skipped this scans nothing, silently. That matters here more than
/// anywhere: a store holds whole transcripts, so it holds whatever the agent saw — the `.env` it read,
/// the connection string it printed — and pushing one publishes them to the team.
pub(crate) fn install_hooks(store: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("could not locate agit's own path")?;
    install_hook(store, "pre-commit", &exe, "hook-scan --staged")?;
    install_hook(store, "pre-push", &exe, "hook-scan")
}

/// POSIX sh single-quote escaping: the only dangerous character is `'` itself; break out with `'\''` and rejoin.
/// Double quotes wouldn't stop `$` / backticks / `"` in a path; inside single quotes these are all literal.
pub(crate) fn sh_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn install_hook(agent: &Path, name: &str, exe: &Path, args: &str) -> Result<()> {
    let hooks = agent.join(".git/hooks");
    std::fs::create_dir_all(&hooks)?;
    let p = hooks.join(name);
    std::fs::write(
        &p,
        format!(
            "#!/bin/sh\n# installed by agit\nexec {} {}\n",
            sh_single_quote(&exe.to_string_lossy()),
            args
        ),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
