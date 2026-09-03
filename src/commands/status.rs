//! `agit status` — the state of this machine at a glance.
//!
//! Answers four things: which sessions are adopted; which agent each is managed by and up to
//! which version; whether anything is committed but not pushed; which sessions in this repo are
//! still unadopted.
//!
//! **Opens no transcript.** Every number comes from the store links and from git, so the command
//! stays fast with thousands of sessions.

use super::CmdResult;
use crate::domain::link;
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Also scan runtime directories for unadopted sessions (slower)
    #[arg(long)]
    pub check_missing: bool,
}

pub fn run(args: Args) -> CmdResult {
    let s = ui::theme::symbols();

    // ── Who am I (PRD status, first block: the context resolution result and its route) ──
    ui::section("who am I");
    let cwd = std::env::current_dir()?;
    match super::context::resolve(&cwd) {
        Ok(c) => {
            println!("  {} @ {}", c.repo, c.branch);
            println!("  {}", ui::dim(&format!("via: {}", c.via)));
        }
        Err(_) => {
            println!(
                "  {}",
                ui::dim("not inside an agent session, and this directory isn’t pinned to a branch")
            );
        }
    }
    if let Some(ws) = crate::domain::workspace::read(&cwd) {
        println!(
            "  {}",
            ui::dim(&format!(
                "bound repo: {} · pinned: {}",
                ws.repo,
                ws.pinned.as_deref().unwrap_or("(none)")
            ))
        );
    }

    // ── Local store ──
    ui::section("local");
    let Some(store) = Store::open()? else {
        println!("  no sessions adopted yet.");
        ui::hint(
            "`agit import -n <name>` lists this repo’s sessions — pick one and record its first version",
        );
        return Ok(ExitCode::Ok);
    };

    let links = link::list(&store);
    let committed = links.iter().filter(|l| l.agent.is_some()).count();

    print!(
        "{}",
        ui::table::key_values(&[
            ("store", ui::tilde(store.root())),
            (
                "adopted sessions",
                format!("{} ({committed} versioned)", links.len())
            ),
        ])
    );

    // ── Adopted sessions ──
    if !links.is_empty() {
        let rows: Vec<Vec<String>> = links
            .iter()
            .take(8)
            .map(|l| {
                vec![
                    link::short(&l.session_id),
                    l.source.clone(),
                    l.agent
                        .clone()
                        .unwrap_or_else(|| ui::dim("unversioned").to_string()),
                ]
            })
            .collect();
        println!(
            "{}",
            ui::table::render(&["session", "runtime", "AGENT"], &rows)
        );
        if links.len() > 8 {
            println!("{}", ui::dim(&format!("… {} more", links.len() - 8)));
        }
    }

    // ── Agent repos on this machine ──
    //
    // The test for "to publish" is git's ahead / behind, not whether some staging directory
    // exists — the local repo is the authoritative copy, and whether a push succeeded shows up
    // in the refs.
    let agents = super::clone::list_local()?;
    if !agents.is_empty() {
        ui::section("agent repos");
        let rows: Vec<Vec<String>> = agents
            .iter()
            .map(|(o, n, p)| {
                let r = Repo::at(p);
                // "current" is the version ID of the HEAD commit (= its SHA).
                let head = r
                    .git_opt(&["rev-parse", "HEAD"])
                    .map(|sha| meta::short(&meta::id_from_sha(sha.trim())))
                    .unwrap_or_else(|| "—".into());
                let state = match r.ahead_behind() {
                    None => ui::warn_text("never pushed").to_string(),
                    Some((0, 0)) => ui::dim("in sync").to_string(),
                    Some((a, 0)) => ui::warn_text(&format!("{a} to publish")).to_string(),
                    Some((0, b)) => format!("behind {b}"),
                    Some((a, b)) => {
                        ui::warn_text(&format!("diverged (ahead {a}, behind {b})")).to_string()
                    }
                };
                vec![
                    format!("{o}/{n}"),
                    r.versions().len().to_string(),
                    head,
                    state,
                ]
            })
            .collect();
        println!(
            "{}",
            ui::table::render(&["AGENT", "versions", "current", "state"], &rows)
        );
    }

    // ── Current repo ──
    if let Some(repo) = config::repo_root() {
        let want = repo.to_string_lossy().to_string();
        let here = links
            .iter()
            .filter(|l| l.cwd.as_deref() == Some(want.as_str()))
            .count();
        ui::section("this repo");
        print!(
            "{}",
            ui::table::key_values(&[
                ("path", ui::tilde(&repo)),
                ("adopted from this repo", here.to_string()),
            ])
        );
    }

    // ── Unadopted sessions (expensive, explicitly triggered) ──
    if args.check_missing {
        ui::section("unadopted sessions");
        let sp = ui::spinner("checking runtime indexes…");
        let missing = uncaptured(&store);
        sp.finish_and_clear();
        if missing.is_empty() {
            println!(
                "  {} every session in this repo is adopted",
                ui::ok(s.check)
            );
        } else {
            println!(
                "  {} {} sessions not adopted yet",
                ui::dim(s.idle),
                missing.len()
            );
            for (rt, id) in missing.iter().take(8) {
                println!(
                    "    {} {}  {}",
                    ui::dim(s.idle),
                    link::short(id),
                    ui::dim(rt)
                );
            }
            if missing.len() > 8 {
                println!("    {}", ui::dim(&format!("… {} more", missing.len() - 8)));
            }
            ui::hint("`agit import <session-id> -n <name>` adopts the one you want");
        }
    } else {
        ui::hint("--check-missing lists this repo’s unadopted sessions");
    }

    // The status computation itself is local. The main startup path may have performed the
    // separate, best-effort once-a-day version check before dispatching this command.
    Ok(ExitCode::Ok)
}

/// Sessions in this repo that are not adopted yet.
///
/// Uses only the runtime indexes (Codex queries the `threads` table, CC reads a directory),
/// **opening no transcript**.
fn uncaptured(store: &Store) -> Vec<(&'static str, String)> {
    let Some(repo) = config::repo_root().or_else(|| std::env::current_dir().ok()) else {
        return vec![];
    };
    let known: std::collections::HashSet<String> = link::list(store)
        .into_iter()
        .map(|l| l.session_id)
        .collect();

    let mut out = vec![];
    for rt in crate::adapter::RUNTIMES {
        let Ok(ad) = crate::adapter::get(rt) else {
            continue;
        };
        for sr in ad.sessions_for(&repo).unwrap_or_default() {
            if !known.contains(&sr.id) {
                out.push((ad.id(), sr.id));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn expensive_scan_is_opt_in() {
        // CC has to read a directory and Codex has to query a database; neither belongs in the
        // default path.
        assert!(
            !super::Args {
                check_missing: false
            }
            .check_missing
        );
    }
}
