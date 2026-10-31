//! Bare `agit`: what is saved here, what is not, and the next step. Local state only, so it
//! answers at once and offline.

use super::{FolderSession, Upload};
use crate::{ExitCode, commands::CmdResult, infra::config, ui};

/// How many of the folder's sessions the overview names.
const SHOWN: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum FolderState {
    /// Sessions save and upload by themselves.
    On,
    /// Bound to a repository, without automatic saving or uploading.
    LocalOnly,
    /// Bound to a project that was turned off.
    Off,
    /// Nothing here is saved.
    NotSaved,
}

pub fn run() -> CmdResult {
    let cwd = std::env::current_dir()?;
    let account = super::signed_in_account();
    let hub = config::hub_url();
    let root = super::folder(&cwd).ok();
    let (state, repository) = match &root {
        None => (FolderState::NotSaved, None),
        Some(root) => {
            let repository = super::bound(root);
            let state = match (
                crate::commands::project::uploads_automatically(root),
                &repository,
            ) {
                (Some(true), _) => FolderState::On,
                (Some(false), _) => FolderState::Off,
                (None, Some(_)) => FolderState::LocalOnly,
                (None, None) => FolderState::NotSaved,
            };
            (state, repository)
        }
    };
    let mut sessions = match &root {
        Some(root) => super::sessions_in(root).unwrap_or_default(),
        None => Vec::new(),
    };
    let total = sessions.len();
    // A git worktree a main checkout covers says which folder's setting it follows, and both say
    // how many sessions from before that coverage wait for `agit save --all`.
    let through = root
        .as_deref()
        .and_then(crate::commands::project::worktree_of);
    let earlier: Vec<(std::path::PathBuf, usize)> = match (&root, &through) {
        (Some(root), Some(_)) => vec![(root.clone(), super::left_out_in(root, &sessions))],
        (Some(root), None) => crate::commands::project::covered_worktrees(root)
            .into_iter()
            .map(|worktree| {
                let count = super::left_out_in(&worktree, &sessions);
                (worktree, count)
            })
            .collect(),
        (None, _) => Vec::new(),
    };
    let waiting: usize = earlier.iter().map(|(_, count)| count).sum();
    // The background uploader decides what an on folder still owes the Hub, including turns that
    // never reached a local branch; asking it costs this command nothing.
    if state == FolderState::On
        && let Some(root) = &root
    {
        let _ = crate::commands::project::catch_up(root);
    }
    sessions.truncate(SHOWN);
    let failure = root
        .as_deref()
        .and_then(crate::commands::project::last_failure);
    let codex_untrusted = state == FolderState::On && codex_hooks_silent(&sessions);
    let next = next_step(account.is_some(), root.is_some(), state, total).or_else(|| {
        (waiting > 0).then_some((
            "agit save --all",
            "upload the sessions that ran before this folder covered them",
        ))
    });

    if crate::commands::json::is_capturing() {
        println!(
            "{}",
            serde_json::json!({
                "hub": hub,
                "account": account,
                "folder": root,
                "state": state,
                "repository": repository,
                "through": through,
                "earlier_sessions_not_uploaded": through.as_ref().map(|_| waiting),
                "worktrees": through.is_none().then(|| earlier.iter().map(|(folder, count)| serde_json::json!({
                    "folder": folder,
                    "earlier_sessions_not_uploaded": count,
                })).collect::<Vec<_>>()),
                "sessions_here": total,
                "recent": sessions.iter_mut().map(row_json).collect::<Vec<_>>(),
                "last_upload_error": failure,
                "codex_hooks_not_running": codex_untrusted,
                "next": next.map(|(command, why)| serde_json::json!({"command": command, "why": why})),
            })
        );
        return Ok(ExitCode::Ok);
    }

    let host = hub
        .split("://")
        .nth(1)
        .unwrap_or(&hub)
        .trim_end_matches('/')
        .to_string();
    match &account {
        Some(account) => println!("Signed in as {} on {host}", ui::bold(account)),
        None => println!("Not signed in to {host}"),
    }
    println!();
    match &root {
        None => println!("This folder is not a project folder; run agit inside one."),
        Some(root) => {
            let folder = ui::tilde(root);
            let repository = repository.as_deref().unwrap_or_default();
            match state {
                FolderState::On => match &through {
                    Some(main) => println!(
                        "{folder} is on through {} (git worktree): sessions upload to {}",
                        ui::tilde(main),
                        ui::bold(repository)
                    ),
                    None => println!(
                        "{folder} is on: sessions upload to {}",
                        ui::bold(repository)
                    ),
                },
                FolderState::LocalOnly => println!(
                    "{folder} is set up for {repository}, but its sessions are not saved or uploaded by themselves"
                ),
                FolderState::Off => println!("{folder} is off"),
                FolderState::NotSaved => println!("{folder} is not saved"),
            }
            if codex_untrusted {
                println!(
                    "  {} in Codex, open /hooks and trust both AgentGit hooks; until then its turns upload when you run `agit` or `agit save`",
                    ui::warn_text("Codex has not run AgentGit's save hook:")
                );
            }
            if let Some(failure) = &failure {
                println!(
                    "  {} {}",
                    ui::warn_text("the last upload failed:"),
                    ui::truncate(failure, 160)
                );
            }
            for (worktree, count) in earlier.iter().filter(|(_, count)| *count > 0) {
                let place = if through.is_some() {
                    "here".to_owned()
                } else {
                    format!("in {} (git worktree)", ui::tilde(worktree))
                };
                println!(
                    "  {count} earlier session{} {place} {} not uploaded; to upload {} too: {}",
                    if *count == 1 { "" } else { "s" },
                    if *count == 1 { "is" } else { "are" },
                    if *count == 1 { "it" } else { "them" },
                    ui::accent("agit save --all")
                );
            }
            for session in &mut sessions {
                print_row(session, root);
            }
            if total > SHOWN {
                println!("  … and {} more", total - SHOWN);
            }
            if total == 0 {
                println!("  no Claude Code or Codex sessions have run here yet");
            }
        }
    }
    println!();
    match next {
        Some((command, why)) => println!("Next: {} — {why}", ui::accent(command)),
        None => println!(
            "Send one: {} · Pick one up: {} · Everything: {}",
            ui::accent("agit share"),
            ui::accent("agit continue"),
            ui::accent("agit help")
        ),
    }
    Ok(ExitCode::Ok)
}

/// Whether an idle Codex session here changed well after the last time Codex ran AgentGit's Stop
/// hook on this computer: the hook is installed, so Codex is not running it, which it does only
/// for hooks the person trusted. A session still being written may be in the middle of a turn
/// and says nothing yet.
fn codex_hooks_silent(sessions: &[FolderSession]) -> bool {
    const SETTLING: std::time::Duration = std::time::Duration::from_secs(120);
    let seen = crate::commands::hooks::stop_seen_path("codex")
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| meta.modified().ok());
    sessions.iter().any(|session| {
        session.runtime == "codex"
            && session.modified.elapsed().is_ok_and(|idle| idle > SETTLING)
            && seen.is_none_or(|seen| {
                session
                    .modified
                    .duration_since(seen)
                    .is_ok_and(|after| after > SETTLING)
            })
    })
}

fn next_step(
    signed_in: bool,
    in_folder: bool,
    state: FolderState,
    sessions: usize,
) -> Option<(&'static str, &'static str)> {
    if !signed_in {
        return Some(("agit login", "sign in so sessions can upload"));
    }
    if !in_folder {
        return None;
    }
    match state {
        FolderState::On => None,
        FolderState::LocalOnly | FolderState::Off | FolderState::NotSaved if sessions > 0 => {
            Some((
                "agit on",
                "save and upload this folder's sessions from now on",
            ))
        }
        _ => Some(("agit on", "save and upload sessions you start here")),
    }
}

fn status_of(session: &FolderSession) -> &'static str {
    match session.upload() {
        None => "not saved",
        Some(Upload::NotUploaded) => "not uploaded yet",
        Some(Upload::Waiting) => "uploading",
        Some(Upload::Uploaded) => "uploaded",
    }
}

/// The machine-readable form of [`status_of`], for `--json` readers.
fn status_key(session: &FolderSession) -> &'static str {
    match session.upload() {
        None => "not_saved",
        Some(Upload::NotUploaded) => "not_uploaded",
        Some(Upload::Waiting) => "waiting",
        Some(Upload::Uploaded) => "uploaded",
    }
}

/// One session of the folder; one that ran in a git worktree the folder covers names it.
fn print_row(session: &mut FolderSession, root: &std::path::Path) {
    let title = ui::truncate(&session.title(), 44);
    let place = if session.cwd.starts_with(root) {
        String::new()
    } else {
        format!("  {}", ui::dim(&ui::tilde(&session.cwd)))
    };
    println!(
        "  {:<46} {:<9} {:<10} {}{place}",
        title,
        super::runtime_name(session.runtime),
        ui::ago(session.modified),
        ui::dim(status_of(session))
    );
}

fn row_json(session: &mut FolderSession) -> serde_json::Value {
    serde_json::json!({
        "title": session.title(),
        "runtime": session.runtime,
        "session_id": session.id,
        "status": status_key(session),
        "repository": session.saved.as_ref().map(|(r, _)| r),
        "folder": session.cwd,
    })
}
