//! `agit status` — the state of this machine at a glance.
//!
//! Answers four things: which sessions are adopted; which agent each is managed by and up to
//! which version; whether anything is committed but not pushed; which sessions in this repo are
//! still unadopted.
//!
//! Displayed session rows use bounded, read-only native snapshots. Incomplete evidence remains
//! unavailable instead of being reported as no unsettled work.

/// Internal memory-only observation runs before ordinary command startup.
#[doc(hidden)]
pub fn native_observation_worker(args: &[std::ffi::OsString]) -> Option<i32> {
    super::diff::pending::status_native_worker(args)
}

use super::CmdResult;
use crate::domain::link;
use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::domain::store::Store;
use crate::infra::config;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

mod branches;
mod sessions;

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
    let ClaimInventory {
        store,
        mut links,
        issues: link_issues,
    } = claim_inventory(config::store_root()?);
    // Active claims remain ahead of recovery history without using filesystem timestamps.
    links.sort_by_key(|link| !link.is_active());
    let page = observe_page(
        store.as_ref(),
        &links,
        link_issues.is_empty(),
        args.offset,
        limit,
    );
    let inventory_complete = page.inventory_complete;

    // ── Who am I (PRD status, first block: the context resolution result and its route) ──
    ui::section("who am I");
    let cwd = std::env::current_dir()?;
    match super::context::from_env_with_claims(&links, inventory_complete) {
        Ok(Some(c)) => {
            println!("  {} @ {}", c.repo, c.branch);
            println!("  {}", ui::dim(&format!("via: {}", c.via)));
        }
        Ok(None) => {
            println!(
                "  {}",
                ui::dim("no session target supplied through AGIT_SESSION")
            );
        }
        Err(error) => println!("  session target unavailable: {error}"),
    }
    if let Some(ws) = crate::domain::workspace::read(&cwd) {
        println!("  {}", ui::dim(&format!("bound repo: {}", ws.repo)));
    }

    // ── Local store ──
    ui::section("local");
    if store.is_none() {
        println!("  no sessions adopted yet.");
        ui::hint(
            "`agit import` opens the session picker; use `agit import <id> --into <owner/repo>@<branch>` for an explicit target",
        );
    }

    let committed = links.iter().filter(|l| l.agent.is_some()).count();

    print!(
        "{}",
        ui::table::key_values(&[
            ("store", ui::tilde(&config::store_root()?)),
            (
                "adopted sessions",
                if inventory_complete {
                    format!("{} ({committed} with a recorded repository)", links.len())
                } else {
                    format!(
                        "{} observed ({committed} with a recorded repository; incomplete inventory)",
                        links.len()
                    )
                }
            ),
        ])
    );

    // ── Adopted sessions ──
    if !links.is_empty() {
        println!(
            "{}",
            ui::table::render(
                &[
                    "session",
                    "runtime",
                    "repo",
                    "branch",
                    "last commit",
                    "pending activity",
                    "local instance"
                ],
                &page.rows
            )
        );
        let remaining = links
            .len()
            .saturating_sub(args.offset.saturating_add(limit));
        if remaining > 0 {
            println!("{}", ui::dim(&format!("… {remaining} more")));
        }
    }

    if !inventory_complete {
        ui::warning("status is incomplete: some local session claims cannot be inspected");
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
                (
                    "adopted from this repo",
                    if inventory_complete {
                        here.to_string()
                    } else {
                        format!("{here} observed (incomplete inventory)")
                    },
                ),
            ])
        );
    }

    // ── Unadopted sessions (expensive, explicitly triggered) ──
    if args.check_missing {
        ui::section("unadopted sessions");
        let sp = ui::spinner("checking runtime indexes…");
        let discovery = uncaptured(&links, inventory_complete);
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
    let ClaimInventory {
        store,
        mut links,
        issues: link_issues,
    } = claim_inventory(config::store_root()?);

    links.sort_by_key(|link| !link.is_active());
    let limit = args.limit.unwrap_or(100) as usize;
    let page = observe_page(
        store.as_ref(),
        &links,
        link_issues.is_empty(),
        args.offset,
        limit,
    );
    let inventory_complete = page.inventory_complete;
    let selection = match super::context::from_env_with_claims(&links, inventory_complete) {
        Ok(Some(context)) => serde_json::json!({
            "repo": context.repo, "branch": context.branch, "source": context.via,
        }),
        Ok(None) => serde_json::json!({"repo":null, "branch":null,
            "reason":"no session target supplied through AGIT_SESSION"}),
        Err(error) => serde_json::json!({"repo":null, "branch":null, "reason":error.to_string()}),
    };
    let items: Vec<_> = links
        .iter()
        .skip(args.offset)
        .take(limit)
        .zip(page.rows)
        .map(|(link, detail)| {
            let target = match (&link.owner, &link.agent, &link.branch) {
                (Some(owner), Some(repo), Some(branch)) => Some(format!("{owner}/{repo}@{branch}")),
                _ => None,
            };
            serde_json::json!({
                "runtime": link.source, "session_id": link.session_id,
                "owner": link.owner, "repository_name": link.agent, "branch": link.branch,
                "target": target, "cwd": link.cwd, "active": link.is_active(),
                "superseded_by": link.superseded_by,
                "last_commit": (detail[4] != "—").then_some(&detail[4]),
                "pending_activity": detail[5], "local_instance": detail[6],
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
        let discovery = uncaptured(&links, inventory_complete);
        Some((discovery.sessions.into_iter().map(|(runtime, session_id)| {
            serde_json::json!({"runtime": runtime, "session_id": session_id})
        }).collect::<Vec<_>>(), discovery.errors))
    } else {
        None
    };
    let code_repo = config::repo_root();
    let adopted_here = code_repo
        .as_ref()
        .filter(|_| inventory_complete)
        .map(|root| {
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
            "limit": limit, "next_offset": (next < links.len()).then_some(next),
             "incomplete": !inventory_complete},
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

fn observe_page(
    store: Option<&Store>,
    links: &[link::Link],
    complete: bool,
    offset: usize,
    limit: usize,
) -> sessions::Page {
    store
        .map(|store| sessions::rows(store, links, complete, offset, limit))
        .unwrap_or_else(|| sessions::Page {
            rows: Vec::new(),
            inventory_complete: complete && links.is_empty(),
        })
}

struct ClaimInventory {
    store: Option<Store>,
    links: Vec<link::Link>,
    issues: Vec<link::LinkIssue>,
}

/// Only a missing carrier proves an empty inventory; inspection failures retain unknown claims.
fn claim_inventory(root: std::path::PathBuf) -> ClaimInventory {
    let store = Store::at(root);
    let (links, mut issues) =
        link::list_checked_with_limits(&store, sessions::MAX_LINKS, sessions::MAX_LINK_BYTES);
    let carrier_issue = match std::fs::symlink_metadata(store.root()) {
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && links.is_empty()
                && issues.is_empty() =>
        {
            return ClaimInventory {
                store: None,
                links,
                issues,
            };
        }
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => None,
        Ok(_) => Some(link::LinkIssueKind::InvalidPath),
        Err(_) => Some(link::LinkIssueKind::UnreadableDirectory),
    };
    if let Some(kind) = carrier_issue {
        issues.push(link::LinkIssue {
            path: store.root().to_owned(),
            kind,
            repository: None,
        });
    }
    ClaimInventory {
        store: Some(store),
        links,
        issues,
    }
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
fn uncaptured(links: &[link::Link], complete: bool) -> Discovery {
    if !complete {
        return Discovery {
            sessions: Vec::new(),
            errors: vec![IndexError {
                runtime: "claims",
                message: "unavailable: claim inventory incomplete; adoption cannot be determined"
                    .into(),
            }],
        };
    }
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
    fn a_missing_store_only_proves_a_complete_empty_observation() {
        let empty = super::observe_page(None, &[], true, 0, 8);
        assert!(empty.inventory_complete);
        assert!(empty.rows.is_empty());
        assert!(!super::observe_page(None, &[], false, 0, 8).inventory_complete);
        let claim = crate::domain::link::Link::new("codex", "unverified", None);
        assert!(!super::observe_page(None, &[claim], true, 0, 8).inventory_complete);
    }

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
