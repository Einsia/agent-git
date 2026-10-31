//! `agit save`: upload now. Inside an agent it saves that conversation; in a terminal it saves
//! what the folder still owes the Hub; `--all` adds the sessions that ran there before the folder
//! was on, and `--today` saves the day's sessions from every folder.

use super::{FolderSession, Refusal};
use crate::{ExitCode, commands::CmdResult, commands::project::Saved, ui};
use anyhow::Result;
use clap::Args as ClapArgs;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct SaveArgs {
    /// Which session: its ID or the start of it, or words from its title (default: this
    /// conversation inside an agent, otherwise every unsaved session in this folder).
    pub session: Option<String>,
    /// Every session that ran in this folder, including those from before it was on.
    #[arg(long, conflicts_with_all = ["session", "today"])]
    pub all: bool,
    /// Every session active today, in every folder.
    #[arg(long, conflicts_with = "session")]
    pub today: bool,
    /// Save to this repository when the folder is not on yet.
    #[arg(long, value_name = "OWNER/NAME", conflicts_with = "today")]
    pub repo: Option<String>,
    /// Allow the repository named with --repo to be public.
    #[arg(long, requires = "repo")]
    pub public: bool,
}

pub fn run(args: SaveArgs) -> CmdResult {
    super::finish(save(args))
}

/// The sessions to save, grouped by folder, and whether sessions the folder's binding left out
/// may be let in.
struct Plan {
    folders: BTreeMap<PathBuf, Vec<FolderSession>>,
    earlier: bool,
    /// The conversation this command runs inside, when that is what is saved.
    this_conversation: bool,
    /// Sessions from before the folder was on, which only `--all` uploads.
    left_out: usize,
    /// Sessions the person stopped uploading (`agit project drop`), which only naming uploads.
    dropped: usize,
}

fn again(args: &SaveArgs) -> String {
    let mut command = String::from("agit save");
    if let Some(session) = &args.session {
        command.push_str(&format!(" {}", ui::quote_posix_argument(session)));
    }
    if args.all {
        command.push_str(" --all");
    }
    if args.today {
        command.push_str(" --today");
    }
    if let Some(repo) = &args.repo {
        command.push_str(&format!(" --repo {repo}"));
    }
    if args.public {
        command.push_str(" --public");
    }
    command
}

fn plan(args: &SaveArgs) -> Result<Plan> {
    let mut folders: BTreeMap<PathBuf, Vec<FolderSession>> = BTreeMap::new();
    if args.today {
        let midnight = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .and_then(|t| t.and_local_timezone(chrono::Local).single())
            .map(std::time::SystemTime::from)
            .unwrap_or(std::time::UNIX_EPOCH);
        let mut dropped = 0;
        for session in super::sessions_since(midnight)? {
            if matches!(session.upload(), Some(super::Upload::Uploaded)) {
                continue;
            }
            // A session outside any project folder has no destination of its own.
            if let Ok(root) = super::folder(&session.cwd) {
                // The day's sessions are not a request for one the person dropped.
                if crate::commands::project::is_dropped(&root, session.runtime, &session.id) {
                    dropped += 1;
                    continue;
                }
                folders.entry(root).or_default().push(session);
            }
        }
        return Ok(Plan {
            folders,
            earlier: true,
            this_conversation: false,
            left_out: 0,
            dropped,
        });
    }
    let this = crate::infra::runtime_session::this_conversation();
    if args.session.is_none()
        && !args.all
        && this.is_none()
        && !crate::infra::runtime_session::named().is_empty()
    {
        return Err(Refusal::new(
            "this command runs inside more than one agent conversation",
            "name the one to save: `agit save SESSION-ID`",
            ExitCode::Interactive,
        )
        .into());
    }
    if args.session.is_none()
        && !args.all
        && let Some((runtime, id)) = this
    {
        let session = super::sessions_where_id(runtime, &id)?;
        let Some(session) = session else {
            return Err(Refusal::new(
                "cannot find this conversation's transcript",
                "name the conversation to save: `agit save SESSION-ID`",
                ExitCode::Ref,
            )
            .into());
        };
        let root = super::folder(&session.cwd)?;
        folders.insert(root, vec![session]);
        return Ok(Plan {
            folders,
            earlier: true,
            this_conversation: true,
            left_out: 0,
            dropped: 0,
        });
    }
    let root = super::folder(&std::env::current_dir()?)?;
    let on = crate::commands::project::uploads_automatically(&root) == Some(true);
    let here = super::sessions_in(&root)?;
    let mut left_out = 0;
    let mut dropped = 0;
    let chosen: Vec<FolderSession> = match &args.session {
        Some(query) => vec![super::pick(here, query, &root)?],
        None => here
            .into_iter()
            .filter(|s| !matches!(s.upload(), Some(super::Upload::Uploaded)))
            // `--all` asks for earlier sessions, not for one the person dropped.
            .filter(|s| {
                let gone = crate::commands::project::is_dropped(&root, s.runtime, &s.id);
                dropped += usize::from(gone);
                !gone
            })
            .filter(|s| {
                let earlier = on
                    && !args.all
                    && crate::commands::project::left_out(&root, s.runtime, &s.id, &s.cwd);
                left_out += usize::from(earlier);
                !earlier
            })
            .collect(),
    };
    // Saving a folder for the first time uploads the sessions already in it. Only a person at a
    // terminal, who is shown how many, or an explicit `--all` may decide that for all of them.
    if !on && args.session.is_none() && !args.all && !chosen.is_empty() && !ui::prompt::may_prompt()
    {
        let count = match chosen.len() {
            1 => "the 1 session".to_owned(),
            n => format!("all {n} sessions"),
        };
        return Err(Refusal::new(
            format!("this would upload {count} in {}", ui::tilde(&root)),
            "name one: `agit save SESSION-ID` (`agit` shows them), or upload them all: `agit save --all`",
            ExitCode::Interactive,
        )
        .into());
    }
    // A folder saved for the first time has no earlier sessions to leave out.
    let earlier = args.all || args.session.is_some() || !on;
    if !chosen.is_empty() {
        folders.insert(root, chosen);
    }
    Ok(Plan {
        folders,
        earlier,
        this_conversation: false,
        left_out,
        dropped,
    })
}

struct Row {
    title: String,
    runtime: &'static str,
    session_id: String,
    folder: PathBuf,
    repository: String,
    outcome: Saved,
    /// The person had stopped uploading it (`agit project drop`), and naming it let it back in.
    was_dropped: bool,
}

fn save(args: SaveArgs) -> CmdResult {
    let plan = plan(&args)?;
    if plan.folders.is_empty() {
        if crate::commands::json::is_capturing() {
            println!(
                "{}",
                serde_json::json!({"sessions": [], "folders_turned_on": [], "left_out": plan.left_out, "dropped": plan.dropped})
            );
        } else {
            if plan.left_out == 0 && plan.dropped == 0 {
                println!("Nothing to save: every session here is uploaded.");
            } else {
                println!("Nothing new to save.");
            }
            report_left_out(plan.left_out);
            report_dropped(plan.dropped);
        }
        return Ok(ExitCode::Ok);
    }
    let client = super::sign_in(&again(&args))?;
    if args.today && std::env::var_os("AGIT_YES").is_none() && ui::prompt::may_prompt() {
        let count: usize = plan.folders.values().map(Vec::len).sum();
        println!("Sessions from today:");
        for (root, sessions) in &plan.folders {
            println!("  {}  {} session(s)", ui::tilde(root), sessions.len());
        }
        if ui::prompt::confirm(
            &format!(
                "Upload these {count} sessions? Their folders also upload new sessions by themselves from then on."
            ),
            true,
        )? != Some(true)
        {
            return Err(Refusal::new(
                "nothing was uploaded",
                "run `agit save` inside one folder to save only its sessions",
                ExitCode::Ok,
            )
            .into());
        }
    }

    let mut rows = Vec::new();
    let mut turned_on = Vec::new();
    let mut skipped = Vec::new();
    for (root, mut sessions) in plan.folders {
        // A folder turned off stays off until the person turns it on again.
        if crate::commands::project::turned_off(&root) {
            // A git worktree is off when the folder whose setting covers it is.
            let why = match crate::commands::project::worktree_of(&root) {
                Some(main) => format!(
                    "{} is a git worktree of {}, which is off",
                    ui::tilde(&root),
                    ui::tilde(&main)
                ),
                None => format!("{} is off", ui::tilde(&root)),
            };
            if args.today {
                skipped.push((root, format!("{why}; `agit on` there turns it back on")));
                continue;
            }
            return Err(Refusal::new(
                why,
                "turn it back on first: `agit on`",
                ExitCode::Precondition,
            )
            .into());
        }
        let count = sessions.len();
        let enabled = match super::on::enable(
            &client,
            &root,
            args.repo.as_deref(),
            args.public,
            |slug| {
                // Saving names its destination; only a person at a terminal is asked once more.
                if args.today || std::env::var_os("AGIT_YES").is_some() || !ui::prompt::may_prompt()
                {
                    return Ok(true);
                }
                let these = match count {
                    1 => "this session".to_owned(),
                    n => format!("these {n} sessions"),
                };
                Ok(ui::prompt::confirm(
                    &format!(
                        "Upload {these} from {} to {slug}, and new ones there automatically from then on?",
                        ui::tilde(&root)
                    ),
                    true,
                )? == Some(true))
            },
        ) {
            Ok(enabled) => enabled,
            // With several folders, one that cannot be set up does not stop the others.
            Err(error) if args.today && error.is::<Refusal>() => {
                skipped.push((root, error.to_string()));
                continue;
            }
            Err(error) => return Err(error),
        };
        if !enabled.already {
            turned_on.push((root.clone(), enabled.repository.clone()));
        }
        let targets: Vec<(String, String, PathBuf)> = sessions
            .iter()
            .map(|s| (s.runtime.to_string(), s.id.clone(), s.cwd.clone()))
            .collect();
        // Only a session the person names, or this conversation they asked to save, may come back
        // after they dropped it.
        let named = args.session.is_some() || plan.this_conversation;
        let was_dropped: Vec<bool> = sessions
            .iter()
            .map(|s| named && crate::commands::project::is_dropped(&root, s.runtime, &s.id))
            .collect();
        let spinner = ui::spinner(&format!("uploading {} session(s)…", targets.len()));
        let outcomes = crate::commands::project::save_now(&root, &targets, plan.earlier, named);
        spinner.finish_and_clear();
        for ((session, outcome), was_dropped) in sessions.iter_mut().zip(outcomes?).zip(was_dropped)
        {
            rows.push(Row {
                title: session.title(),
                runtime: session.runtime,
                session_id: session.id.clone(),
                folder: root.clone(),
                repository: enabled.repository.clone(),
                outcome,
                was_dropped,
            });
        }
    }
    let code = report(
        &rows,
        &turned_on,
        &skipped,
        plan.this_conversation,
        plan.dropped,
    )?;
    if !crate::commands::json::is_capturing() {
        report_left_out(plan.left_out);
    }
    Ok(code)
}

/// What `--all`, `--today` or a folder's save left out because the person dropped it.
fn report_dropped(dropped: usize) {
    if dropped > 0 {
        println!(
            "{dropped} session{} you stopped uploading (`agit project drop`) {} left out; to upload one anyway, name it: {}",
            if dropped == 1 { "" } else { "s" },
            if dropped == 1 { "is" } else { "are" },
            ui::accent("agit save SESSION-ID")
        );
    }
}

/// The command that turns `folder` off again, quoted for this platform's shell.
fn off_command(folder: &Path) -> String {
    let folder = folder.to_string_lossy();
    #[cfg(windows)]
    let folder = ui::quote_powershell_argument(&folder);
    #[cfg(not(windows))]
    let folder = ui::quote_posix_argument(&folder);
    format!("agit off --json {folder}")
}

fn report_left_out(left_out: usize) {
    if left_out > 0 {
        println!(
            "{left_out} session{} from before this folder was on {} left out; to upload {} too: {}",
            if left_out == 1 { "" } else { "s" },
            if left_out == 1 { "is" } else { "are" },
            if left_out == 1 { "it" } else { "them" },
            ui::accent("agit save --all")
        );
    }
}

fn link(repository: &str, branch: &str) -> String {
    super::session_link(repository, branch).unwrap_or_else(|| super::repository_link(repository))
}

fn report(
    rows: &[Row],
    turned_on: &[(PathBuf, String)],
    skipped: &[(PathBuf, String)],
    this_conversation: bool,
    dropped: usize,
) -> CmdResult {
    let failed = rows
        .iter()
        .filter(|r| matches!(r.outcome, Saved::Failed(_)))
        .count();
    if crate::commands::json::is_capturing() {
        let sessions: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                let (status, branch, error) = match &r.outcome {
                    Saved::Uploaded(b) => ("uploaded", Some(b), None),
                    Saved::Pending(b) => ("pending", Some(b), None),
                    Saved::Failed(e) => ("failed", None, Some(e)),
                };
                serde_json::json!({
                    "title": r.title,
                    "runtime": r.runtime,
                    "session_id": r.session_id,
                    "folder": r.folder,
                    "repository": r.repository,
                    "status": status,
                    "link": match &r.outcome {
                        Saved::Uploaded(b) => Some(link(&r.repository, b)),
                        Saved::Pending(_) => Some(super::repository_link(&r.repository)),
                        Saved::Failed(_) => None,
                    },
                    "branch": branch,
                    "error": error,
                    "was_dropped": r.was_dropped,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "sessions": sessions,
                "folders_turned_on": turned_on.iter().map(|(f, r)| serde_json::json!({"folder": f, "repository": r, "if_no": off_command(f)})).collect::<Vec<_>>(),
                "folders_skipped": skipped.iter().map(|(f, why)| serde_json::json!({"folder": f, "why": why})).collect::<Vec<_>>(),
                "dropped": dropped,
                // A save that turned a folder on did what the person asked and more: whether new
                // sessions there keep uploading by themselves is theirs to answer, before anything
                // else (ledger 121, choice A).
                "next": if let [(folder, repository)] = turned_on {
                    Some(serde_json::json!({
                        "ask_user": format!("New sessions in {} now upload to {repository} by themselves. Keep that on?", ui::tilde(folder)),
                        "if_no": off_command(folder),
                    }))
                } else if !turned_on.is_empty() {
                    Some(serde_json::json!({
                        "ask_user": format!(
                            "New sessions in {} now upload by themselves. Keep that on for each?",
                            turned_on.iter().map(|(f, _)| ui::tilde(f)).collect::<Vec<_>>().join(", ")
                        ),
                        "if_no": "for each folder the user says no to, run its `if_no` in folders_turned_on",
                    }))
                } else if failed > 0 {
                    Some(serde_json::json!({
                        "command": "agit save",
                        "why": "retry what did not upload; what is already on the Hub is not sent again",
                    }))
                } else {
                    this_conversation.then(|| serde_json::json!({
                        "command": "agit share",
                        "why": "get a link to send to someone",
                    }))
                },
            })
        );
    } else {
        for row in rows {
            match &row.outcome {
                Saved::Uploaded(branch) => {
                    ui::success(&format!("uploaded \"{}\"", ui::truncate(&row.title, 60)));
                    println!("  {}", link(&row.repository, branch));
                    if row.was_dropped {
                        println!("  you had stopped uploading it; its new turns upload again");
                    }
                }
                Saved::Pending(_) if this_conversation => {
                    ui::success(&format!("saving \"{}\"", ui::truncate(&row.title, 60)));
                    println!(
                        "  it appears in {} when this reply finishes",
                        super::repository_link(&row.repository)
                    );
                }
                Saved::Pending(_) => {
                    println!(
                        "waiting: \"{}\" has no finished reply yet; it uploads to {} once one finishes",
                        ui::truncate(&row.title, 60),
                        row.repository
                    );
                }
                Saved::Failed(error) => {
                    ui::error(&format!(
                        "could not upload \"{}\": {}",
                        ui::truncate(&row.title, 60),
                        ui::truncate(error, 200)
                    ));
                }
            }
        }
        for (folder, repository) in turned_on {
            println!(
                "{} is on: new sessions there upload to {repository} by themselves (to stop: `agit off`)",
                ui::tilde(folder)
            );
        }
        for (folder, why) in skipped {
            ui::warning(&format!("skipped {}: {why}", ui::tilde(folder)));
        }
        report_dropped(dropped);
        if failed > 0 {
            ui::hint("run `agit save` again to retry; `agit` shows what is still waiting");
        } else if this_conversation {
            println!("To send it to someone: {}", ui::accent("agit share"));
        }
    }
    Ok(if failed > 0 {
        ExitCode::Failure
    } else {
        ExitCode::Ok
    })
}
