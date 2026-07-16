//! WorkspaceRevision — joint version control.
//!
//! PRD: "agit commit pins an EnvironmentRevision, agit -a commit pins an AgentRevision.
//! After either ref moves, agit automatically generates a WorkspaceRevision recording the current Agent, the current Environment, and their edges."
//!
//! Stored as an append-only log under .agit/workspace/, **deliberately kept outside both git worktrees** —
//! otherwise the act of "writing the pairing" would itself move the agent ref, triggering another write, and recurse forever.

use crate::environment::{self, EnvironmentRevision};
use crate::scope::{self, Scope};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRevision {
    /// Which operation triggered this ("env:commit" / "agent:merge" …).
    pub trigger: String,
    /// The current AgentRevision (the Agent Store's HEAD). May be empty (no agent commit yet).
    pub agent_rev: String,
    /// The current EnvironmentRevision.
    pub env: EnvironmentRevision,
    /// Agent↔Environment and Agent↔Agent edges. Placeholder for now in the MVP.
    pub relations: Vec<String>,
}

fn now_iso() -> String {
    // Don't drag system time into the test path: getting a stable, reproducible timestamp via git costs more,
    // so we use chrono directly here. A WorkspaceRevision is a runtime artifact and never enters a golden test.
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn agent_head() -> String {
    match scope::agent_root() {
        Ok(root) if root.join(".git").exists() => {
            scope::git_in_status(&root, &["rev-parse", "HEAD"]).1
        }
        _ => String::new(),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(9).collect()
}

/// Edges for this pairing: Agent↔Environment (the pairing itself) + Agent↔Agent (if the agent HEAD is a merge,
/// record the merged-in parent commits). Computed live from the git topology — no longer a perpetually empty placeholder; `workspace` can see the real graph.
fn relations_for(agent_rev: &str, env: &EnvironmentRevision) -> Vec<String> {
    let mut rels = vec![format!(
        "agent~env:{}@{}",
        if agent_rev.is_empty() { "∅".into() } else { short(agent_rev) },
        short(&env.head_commit)
    )];
    if let Ok(root) = scope::agent_root() {
        if root.join(".git").exists() {
            // rev-list --parents -n1 HEAD → "HEAD p1 p2 …"; more than 2 tokens means a merge commit.
            let line = scope::git_in_status(&root, &["rev-list", "--parents", "-n", "1", "HEAD"]).1;
            let toks: Vec<&str> = line.split_whitespace().collect();
            if toks.len() > 2 {
                let parents: Vec<String> = toks[1..].iter().map(|p| short(p)).collect();
                rels.push(format!("agent-merge:{}", parents.join("+")));
            }
        }
    }
    rels
}

/// Generate and append one WorkspaceRevision. Called automatically by agit after either store's ref moves.
pub fn record(trigger: &str) -> Result<WorkspaceRevision> {
    let env = environment::capture_current()?;
    let agent_rev = agent_head();
    let relations = relations_for(&agent_rev, &env);
    let rev = WorkspaceRevision {
        trigger: trigger.to_string(),
        agent_rev,
        env,
        relations,
    };

    let dir = scope::workspace_dir()?;
    std::fs::create_dir_all(&dir).context("failed to create .agit/workspace")?;

    let mut line = serde_json::to_string(&serde_json::json!({
        "ts": now_iso(),
        "trigger": rev.trigger,
        "agent_rev": rev.agent_rev,
        "env": rev.env,
        "relations": rev.relations,
    }))?;
    line.push('\n');

    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log.jsonl"))
        .context("failed to write workspace log")?;
    f.write_all(line.as_bytes())?;

    std::fs::write(
        dir.join("HEAD.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "ts": now_iso(),
            "trigger": rev.trigger,
            "agent_rev": rev.agent_rev,
            "env": rev.env,
            "relations": rev.relations,
        }))?,
    )?;

    Ok(rev)
}

/// Whether a given git subcommand moves a ref (used to decide whether to generate a WorkspaceRevision).
pub fn moves_ref(subcommand: &str) -> bool {
    matches!(
        subcommand,
        "commit" | "merge" | "reset" | "checkout" | "switch" | "cherry-pick" | "pull" | "rebase" | "revert" | "am"
    )
}

/// For the routing layer to call: scope + subcommand → trigger string.
pub fn trigger_label(scope: Scope, subcommand: &str) -> String {
    let s = match scope {
        Scope::Environment => "env",
        Scope::Agent => "agent",
    };
    format!("{s}:{subcommand}")
}
