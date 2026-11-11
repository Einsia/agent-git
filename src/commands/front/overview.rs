//! Bare `agit`: what is saved here, what is not, and the next step. Local state only, so it
//! answers at once and offline.

use super::{FolderSession, Upload};
use crate::{ExitCode, commands::CmdResult, infra::config, ui};

/// How many of the folder's sessions the overview names.
const SHOWN: usize = 5;

/// The overview's last line when no step is next: what to do with a session, and the command for
/// each. Every one must exist as written (a test parses them).
const FOOTER: [(&str, &str); 3] = [
    ("Send one", "agit share"),
    ("Pick one up", "agit resume"),
    ("Everything", "agit help"),
];

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
    let failure_said = root
        .as_deref()
        .and_then(crate::commands::project::last_failure_described);
    // An on folder whose uploads are held is said to be on and held, never to upload.
    let held = match (state, root.as_deref().map(crate::commands::project::saving)) {
        (FolderState::On, Some(crate::commands::project::Saving::Held { held, .. })) => Some(held),
        _ => None,
    };
    let codex_untrusted = state == FolderState::On && codex_hooks_silent(&sessions);
    let opencode = root
        .as_deref()
        .and_then(crate::commands::project::opencode_not_captured);
    // An old agit section in a folder agit set up still tells agents to run agit at every session
    // start; the person removes it, as the file may be committed.
    let legacy = crate::commands::setup::legacy_folder_sections();
    // After the person said no to turning this folder on, nothing suggests it again.
    let declined = root
        .as_deref()
        .is_some_and(crate::commands::project::automatic_upload_declined);
    let next = next_step(account.is_some(), root.is_some(), state, total)
        .filter(|(command, _)| !(declined && *command == "agit on"));
    // With nothing else next, the sessions from before: the person's to upload, so an agent is
    // told to ask rather than handed the command.
    let earlier_next = (next.is_none() && waiting > 0).then_some(waiting);

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
                "held": held.as_ref().map(crate::commands::project::Held::json),
                "codex_hooks_not_running": codex_untrusted,
                "opencode_not_captured": opencode,
                "legacy_agents_sections": legacy.iter().map(|section| serde_json::json!({
                    "file": section.file,
                    "tracked": section.tracked,
                    "command": section.command,
                })).collect::<Vec<_>>(),
                "next": next
                    .map(|(command, why)| serde_json::json!({"command": command, "why": why}))
                    .or_else(|| earlier_next.map(|count| serde_json::json!({
                        "ask_user": super::earlier_note(count, "here", super::Reader::Agent),
                    }))),
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
            match (state, held.as_ref()) {
                (FolderState::On, Some(held)) => {
                    let place = match &through {
                        Some(main) => format!(" through {} (git worktree)", ui::tilde(main)),
                        None => String::new(),
                    };
                    match held.code {
                        // Named as the start line and `agit status` name it, with what ends it.
                        "push_auto_off" => println!(
                            "{folder} is on{place}, but automatic upload to {} is off on this computer: sessions here are saved on this computer and not uploaded; {} publishes them{}",
                            ui::bold(repository),
                            ui::accent(&format!("agit push {repository}")),
                            if crate::commands::project::on_switches_back(repository) {
                                format!(
                                    ", and {} switches automatic upload back on",
                                    ui::accent("agit on")
                                )
                            } else {
                                String::new()
                            }
                        ),
                        "public_needs_consent" => println!(
                            "{folder} is on{place}, but {} is public, and automatic upload into it waits until you switch it on: {}",
                            ui::bold(repository),
                            ui::accent("agit on")
                        ),
                        _ => println!(
                            "{folder} is on{place}, but its uploads to {} are held: {}",
                            ui::bold(repository),
                            held.text
                        ),
                    }
                }
                (FolderState::On, None) => match &through {
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
                (FolderState::LocalOnly, _) => println!(
                    "{folder} is set up for {repository}, but its sessions are not saved or uploaded by themselves"
                ),
                (FolderState::Off, _) => println!("{folder} is off"),
                (FolderState::NotSaved, _) => println!("{folder} is not saved"),
            }
            if let Some(said) = opencode {
                println!("  {said}");
            }
            if codex_untrusted {
                println!(
                    "  {} in Codex, open /hooks and trust both AgentGit hooks; until then nothing from Codex is saved here; to upload one session: `agit save SESSION-ID`",
                    ui::warn_text("Codex has not run AgentGit's save hook:")
                );
            }
            if let Some(failure) = failure_said {
                println!("  {} {failure}", ui::warn_text("the last upload failed:"));
            }
            for (worktree, count) in earlier.iter().filter(|(_, count)| *count > 0) {
                let place = if through.is_some() {
                    "here".to_owned()
                } else {
                    format!("in {} (git worktree)", ui::tilde(worktree))
                };
                println!(
                    "  {}",
                    super::earlier_note(*count, &place, super::Reader::of_this_process())
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
    for section in &legacy {
        println!();
        println!(
            "{}",
            ui::warn_text(&crate::commands::setup::legacy_folder_section_line(section))
        );
    }
    println!();
    match (next, earlier_next) {
        (Some((command, why)), _) => println!("Next: {} — {why}", ui::accent(command)),
        (None, Some(count)) => match super::Reader::of_this_process() {
            super::Reader::Person => println!(
                "Next: {} — upload the sessions that ran before this folder covered them",
                ui::accent("agit save --all")
            ),
            super::Reader::Agent => println!(
                "Next: {}",
                super::earlier_note(count, "here", super::Reader::Agent)
            ),
        },
        (None, None) => println!(
            "{}",
            FOOTER
                .map(|(what, command)| format!("{what}: {}", ui::accent(command)))
                .join(" · ")
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// Every command the overview suggests runs as printed; a hint naming a command the CLI does
    /// not define (such as `agit continue`) fails here.
    #[test]
    fn every_command_the_overview_suggests_exists() {
        let mut commands: Vec<&str> = FOOTER.iter().map(|(_, command)| *command).collect();
        commands.push("agit save --all");
        for signed_in in [false, true] {
            for in_folder in [false, true] {
                for state in [
                    FolderState::On,
                    FolderState::LocalOnly,
                    FolderState::Off,
                    FolderState::NotSaved,
                ] {
                    for sessions in [0, 3] {
                        commands.extend(
                            next_step(signed_in, in_folder, state, sessions)
                                .map(|(command, _)| command),
                        );
                    }
                }
            }
        }
        for command in commands {
            match crate::commands::Cli::try_parse_from(command.split_whitespace()) {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        clap::error::ErrorKind::DisplayHelp
                            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    ) => {}
                Err(error) => panic!("`{command}` is not a command: {error}"),
            }
        }
    }
}
