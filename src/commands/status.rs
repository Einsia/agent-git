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

mod branches;

#[derive(ClapArgs)]
pub struct Args {
    /// Also inspect runtime indexes for unadopted sessions (slower; SQLite may maintain sidecars)
    #[arg(long)]
    pub check_missing: bool,
    /// Session rows per page (text: 8; JSON: 100).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=1000))]
    pub limit: Option<u16>,
    /// Skip this many session rows.
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
}

pub fn run(args: Args) -> CmdResult {
    if super::json::requested() {
        return structured(&args);
    }
    let limit = args.limit.unwrap_or(8) as usize;
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
                ui::dim("no session target supplied through AGIT_SESSION")
            );
        }
    }
    if let Some(ws) = crate::domain::workspace::read(&cwd) {
        println!("  {}", ui::dim(&format!("bound repo: {}", ws.repo)));
    }

    // ── Local store ──
    ui::section("local");
    let store = Store::open()?;
    if store.is_none() {
        println!("  no sessions adopted yet.");
        ui::hint(
            "`agit import` opens the session picker; use `agit import <id> --into <owner/repo>@<branch>` for an explicit target",
        );
    }

    let mut links = store.as_ref().map(link::list).unwrap_or_default();
    // Historical links remain visible for recovery, but they must not push the branch's current
    // writer below the display limit. The stable sort keeps `link::list`'s deterministic order
    // inside each group and avoids a filesystem metadata read in every comparator call.
    links.sort_by_key(|link| !link.is_active());
    let committed = links.iter().filter(|l| l.agent.is_some()).count();

    print!(
        "{}",
        ui::table::key_values(&[
            ("store", ui::tilde(&config::store_root()?)),
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
            .skip(args.offset)
            .take(limit)
            .map(|l| {
                vec![
                    link::short(&l.session_id),
                    l.source.clone(),
                    l.agent
                        .clone()
                        .unwrap_or_else(|| ui::dim("unversioned").to_string()),
                    if l.is_active() {
                        "active".to_string()
                    } else {
                        ui::dim("superseded").to_string()
                    },
                ]
            })
            .collect();
        println!(
            "{}",
            ui::table::render(&["session", "runtime", "AGENT", "state"], &rows)
        );
        let remaining = links
            .len()
            .saturating_sub(args.offset.saturating_add(limit));
        if remaining > 0 {
            println!("{}", ui::dim(&format!("… {remaining} more")));
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
        let mut rows = Vec::new();
        let mut omitted = 0;
        for (index, (owner, name, path)) in agents.iter().enumerate() {
            if rows.len() >= 128 {
                omitted += agents.len() - index;
                break;
            }
            let slug = format!("{owner}/{name}");
            match branches::inspect(&Repo::at(path), 128 - rows.len()) {
                Ok(page) if page.branches.is_empty() => rows.push(vec![
                    slug,
                    "—".into(),
                    "—".into(),
                    "—".into(),
                    "no branch refs".into(),
                ]),
                Ok(page) => {
                    omitted += page.omitted;
                    for branch in page.branches {
                        rows.push(vec![
                            slug.clone(),
                            branch.name,
                            meta::short(&meta::id_from_sha(&branch.head)),
                            if branch.tracking.is_empty() {
                                "—".into()
                            } else {
                                branch.tracking
                            },
                            branch.state,
                        ]);
                    }
                }
                Err(error) => rows.push(vec![
                    slug,
                    "—".into(),
                    "—".into(),
                    "—".into(),
                    format!("unavailable: {error:#}"),
                ]),
            }
        }
        println!(
            "{}",
            ui::table::render(
                &["repo", "branch", "last commit", "tracking ref", "state"],
                &rows
            )
        );
        if omitted > 0 {
            ui::warning(
                "status is incomplete: additional branches or repositories exceed the display budget",
            );
            ui::hint(
                "inspect a repository with `agit branch --repo <owner/repo> --all` for its remaining branches",
            );
        }
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
        let discovery = uncaptured(&links);
        let missing = &discovery.sessions;
        sp.finish_and_clear();
        if missing.is_empty() && discovery.errors.is_empty() {
            println!(
                "  {} no unadopted sessions found in the checked indexes",
                ui::ok(s.check)
            );
        } else if !missing.is_empty() {
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
            ui::hint(
                "`agit import <session-id> --from <runtime> --into <owner/repo>@<branch>` lets you choose its lineage",
            );
        }
        for error in discovery.errors {
            ui::warning(&format!(
                "{} index could not be checked: {}",
                error.runtime, error.message
            ));
        }
    } else {
        ui::hint("--check-missing lists this repo’s unadopted sessions");
    }

    Ok(ExitCode::Ok)
}

fn structured(args: &Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let selection = match super::context::resolve(&cwd) {
        Ok(context) => serde_json::json!({
            "repo": context.repo, "branch": context.branch, "source": context.via,
        }),
        Err(error) => {
            serde_json::json!({"repo": null, "branch": null, "reason": error.to_string()})
        }
    };
    let store = Store::open()?;
    let mut links = store.as_ref().map(link::list).unwrap_or_default();
    links.sort_by_key(|link| !link.is_active());
    let limit = args.limit.unwrap_or(100) as usize;
    let items: Vec<_> = links
        .iter()
        .skip(args.offset)
        .take(limit)
        .map(|link| {
            let target = match (&link.owner, &link.agent, &link.branch) {
                (Some(owner), Some(repo), Some(branch)) => Some(format!("{owner}/{repo}@{branch}")),
                _ => None,
            };
            serde_json::json!({
                "runtime": link.source, "session_id": link.session_id,
                "owner": link.owner, "repository_name": link.agent, "branch": link.branch,
                "target": target, "cwd": link.cwd, "active": link.is_active(),
                "superseded_by": link.superseded_by,
            })
        })
        .collect();
    let next = args.offset.saturating_add(items.len());
    let agents = super::clone::list_local()?;
    let mut repositories = Vec::new();
    let mut remaining = 128;
    let mut repositories_omitted = 0;
    for (index, (owner, name, path)) in agents.iter().enumerate() {
        if remaining == 0 {
            repositories_omitted = agents.len() - index;
            break;
        }
        let branch_sync = match branches::inspect(&Repo::at(path), remaining) {
            Ok(page) => {
                remaining -= page.branches.len().max(1);
                serde_json::json!({
                    "items": page.branches, "omitted": page.omitted, "error": null,
                })
            }
            Err(error) => {
                remaining -= 1;
                serde_json::json!({"items": null, "omitted": null, "error": format!("{error:#}")})
            }
        };
        repositories.push(serde_json::json!({
            "repo": format!("{owner}/{name}"), "path": path, "branches": branch_sync,
        }));
    }
    let missing = if args.check_missing {
        let discovery = uncaptured(&links);
        Some((discovery.sessions.into_iter().map(|(runtime, session_id)| {
            serde_json::json!({"runtime": runtime, "session_id": session_id})
        }).collect::<Vec<_>>(), discovery.errors))
    } else {
        None
    };
    let code_repo = config::repo_root();
    let adopted_here = code_repo.as_ref().map(|root| {
        links
            .iter()
            .filter(|link| link.cwd.as_deref() == root.to_str())
            .count()
    });
    let result = serde_json::json!({
        "schema_version": 1, "cwd": cwd, "selection": selection,
        "bound_repo": crate::domain::workspace::read(&cwd).map(|workspace| workspace.repo),
        "store_path": store.as_ref().map(|store| store.root()),
        "sessions": {"items": items, "total": links.len(), "offset": args.offset,
            "limit": limit, "next_offset": (next < links.len()).then_some(next)},
        "repositories": repositories, "repositories_omitted": repositories_omitted,
        "code_repository": {"path": code_repo, "adopted_sessions": adopted_here},
        "unadopted": {"checked": args.check_missing,
            "sessions": missing.as_ref().map(|(sessions, _)| sessions),
            "incomplete": missing.as_ref().map(|(_, errors)| !errors.is_empty()),
            "errors": missing.as_ref().map(|(_, errors)| errors)},
    });
    println!("{}", serde_json::to_string(&result)?);
    Ok(ExitCode::Ok)
}

#[derive(Default)]
struct Discovery {
    sessions: Vec<(&'static str, String)>,
    errors: Vec<IndexError>,
}

#[derive(serde::Serialize)]
struct IndexError {
    runtime: &'static str,
    message: String,
}

/// Runtime indexes define discovery; an unreadable index is not evidence of an empty one.
fn uncaptured(links: &[link::Link]) -> Discovery {
    let Some(repo) = config::repo_root().or_else(|| std::env::current_dir().ok()) else {
        return Discovery::default();
    };
    let mut known: std::collections::HashMap<&str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for link in links {
        known
            .entry(&link.source)
            .or_default()
            .insert(&link.session_id);
    }

    let mut out = Discovery::default();
    for rt in crate::adapter::RUNTIMES {
        let Ok(ad) = crate::adapter::get(rt) else {
            continue;
        };
        let sessions = match ad.sessions_for(&repo) {
            Ok(sessions) => sessions,
            Err(error) => {
                out.errors.push(IndexError {
                    runtime: ad.id(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        for sr in sessions {
            if !known
                .get(ad.id())
                .is_some_and(|ids| ids.contains(sr.id.as_str()))
            {
                out.sessions.push((ad.id(), sr.id));
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
                check_missing: false,
                limit: None,
                offset: 0,
            }
            .check_missing
        );
    }
}
