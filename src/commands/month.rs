//! A member's month in an organization, written on their own machine.
//!
//! `draft` reads the member's month from the Hub, then for each repository in it gathers what
//! they asked and what came back in the sessions the Hub lists for them, into a private bundle
//! their own agent writes from. `publish` puts the sections they wrote on the Hub as their own
//! text: each section must name the sessions it was written from, passes the secret scan with
//! the member's registered values, and goes only after they confirm at a terminal. The text
//! stays theirs alone until they share the month on the website. `clean` deletes the bundle.
//!
//! The bundle is read by the member's agent, often a hosted model, so every message in it passes
//! the device's protection first, as text does before it is committed: registered values and
//! detected credentials reach neither the bundle nor the model. It never lands in the working
//! directory or anywhere it could be committed or uploaded: it lives under the AgentGit home in a
//! directory only the member can open, and a successful publish deletes it. A bundle written with
//! `--out` stays where the member put it until they delete it.
//!
//! Everything in the bundle is read from the Hub, with the member's own credential: what they
//! never published, withdrew or may no longer show stays out, local history included. A session
//! is read only within the days the Hub keeps for it, so a day the member hid is never read.

use super::CmdResult;
use crate::{
    ExitCode,
    domain::{
        activity::{Assistant, Query},
        meta,
        privacy::{projector::Mode as PrivacyMode, service as privacy},
        repo, secrets,
    },
    hub::client::{ApiError, Client},
    infra::config,
    ui,
};
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use clap::{Args as ClapArgs, Subcommand};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// How long `draft` waits for the Hub to build a month it asked for.
const BUILD_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
const BUILD_POLL: std::time::Duration = std::time::Duration::from_secs(3);
/// Message chunks one activity page asks for.
const PAGE_LIMIT: usize = 200;
/// The file `draft` tells the agent to write and `publish` reads by default.
const SUMMARY: &str = "summary.md";

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Gather your month's conversations into a private bundle for your agent to write from
    Draft {
        /// Organization
        org: String,
        /// Month, as YYYY-MM
        month: String,
        /// Write the bundle to this new directory instead of the private one under the AgentGit
        /// home; it must not exist yet, nor lie in a Git work tree or an agit project folder.
        /// publish and clean leave it for you to delete
        #[arg(long)]
        out: Option<PathBuf>,
        /// Replace the earlier draft in the private directory without asking, deleting the files
        /// you added there, such as summary.md
        #[arg(long)]
        replace: bool,
    },
    /// Put the sections you wrote on the Hub as your own text, after a scan and your confirmation
    Publish {
        /// Organization
        org: String,
        /// Month, as YYYY-MM
        month: String,
        /// The summary to publish (default: summary.md in the month's bundle)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Delete the month's private bundle; one written with --out is yours to delete
    Clean {
        /// Organization
        org: String,
        /// Month, as YYYY-MM
        month: String,
    },
}

pub fn run(args: Args) -> CmdResult {
    let outcome = match args.command {
        Command::Draft {
            org,
            month,
            out,
            replace,
        } => draft(&org, &month, out, replace),
        Command::Publish { org, month, file } => publish(&org, &month, file),
        Command::Clean { org, month } => clean(&org, &month),
    };
    match outcome {
        Ok(code) => Ok(code),
        Err(error) => {
            super::fix::register_terminal_api_error(&error);
            ui::error(&format!("month failed: {error:#}"));
            let fallback = match error
                .downcast_ref::<crate::hub::client::ApiError>()
                .map(|api| api.status)
            {
                Some(400) => ExitCode::Usage,
                Some(404) => ExitCode::Ref,
                Some(409 | 412) => ExitCode::Precondition,
                Some(_) => ExitCode::Network,
                None => ExitCode::Failure,
            };
            Ok(super::terminal_error_code(&error, fallback))
        }
    }
}

/// A month as `YYYY-MM`, the current one or earlier.
fn month_of(raw: &str) -> Result<NaiveDate> {
    let first = NaiveDate::parse_from_str(&format!("{raw}-01"), "%Y-%m-%d")
        .ok()
        .filter(|_| raw.len() == 7)
        .with_context(|| format!("{raw:?} is not a month; use YYYY-MM"))?;
    Ok(first)
}

/// The private directory of one month's bundle under the AgentGit home.
fn bundle_dir(org: &str, month: &str) -> Result<PathBuf> {
    Ok(config::agit_home()?.join("month").join(org).join(month))
}

/// Where a redraft writes beside `dir` before taking its place. Whatever deletes the bundle
/// deletes this too, since a draft cut short could leave a copy there.
fn staging_dir(dir: &Path, month: &str) -> PathBuf {
    dir.with_file_name(format!(".{month}.drafting"))
}

/// The directory `--out` names. It must not exist yet, so making it private changes no
/// directory the member already has, and it must not lie in a Git work tree, an agit project
/// folder or a bound workspace, where the bundle could be committed or uploaded.
fn chosen_out(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    // A ".." is resolved only when the directory is made, after the checks below have looked at
    // the path as written, so it could carry the bundle into a work tree they never saw.
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        bail!(
            "{} has a \"..\"; name the directory without one",
            path.display()
        );
    }
    if path.exists() {
        bail!("{} exists; --out takes a new directory", path.display());
    }
    let existing = path
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .context("--out names no directory on this machine")?
        .canonicalize()?;
    if existing
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
    {
        bail!(
            "{} is inside a Git work tree, where the bundle could be committed; choose a directory outside it",
            path.display()
        );
    }
    if super::project::covers(&existing)? {
        bail!(
            "{} is inside an agit project folder, where the bundle could be uploaded; choose a directory outside it",
            path.display()
        );
    }
    Ok(path)
}

/// Joins the chunks a page split a long message into, in contributions read in order, so each
/// message is protected whole: a credential cut across two chunks matches no rule in either.
fn join_chunks(contributions: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut joined: Vec<serde_json::Value> = Vec::new();
    // Where the message still waiting for its next chunk is: contribution, then message.
    let mut open: Option<(String, u64, usize, usize)> = None;
    for mut contribution in contributions {
        let commit = contribution["commit"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let messages = contribution["messages"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut kept = Vec::new();
        for message in messages {
            let line = message["line"].as_u64().unwrap_or_default();
            let continues = message["text_offset"].as_u64().unwrap_or_default() > 0;
            let complete = message["text_complete"].as_bool().unwrap_or(true);
            match &open {
                Some((open_commit, open_line, at, index))
                    if continues && *open_commit == commit && *open_line == line =>
                {
                    let (at, index) = (*at, *index);
                    let target = &mut joined[at]["messages"][index];
                    let text = format!(
                        "{}{}",
                        target["text"].as_str().unwrap_or_default(),
                        message["text"].as_str().unwrap_or_default()
                    );
                    target["text"] = text.into();
                    target["text_complete"] = complete.into();
                    if complete {
                        open = None;
                    }
                }
                _ => {
                    if !complete {
                        open = Some((commit.clone(), line, joined.len(), kept.len()));
                    }
                    kept.push(message);
                }
            }
        }
        if !kept.is_empty() {
            contribution["messages"] = kept.into();
            joined.push(contribution);
        }
    }
    joined
}

/// `text` after the device's protection, with the rules of the repository at `repo` when it is
/// checked out here. Text that could not be protected in full stops the draft: nothing
/// unprotected is written.
fn protected(text: &str, repo: Option<&Path>) -> Result<String> {
    privacy::protect_for_commit(repo, text, PrivacyMode::ProtectText).map_err(|pending| {
        anyhow!(
            "could not protect the conversation text ({}); nothing was written, run the command again",
            privacy::reason_name(pending.reason)
        )
    })
}

/// Every message of `contributions` after the device's protection.
fn protect(contributions: &mut [serde_json::Value], repo: Option<&Path>) -> Result<()> {
    for contribution in contributions {
        let Some(messages) = contribution["messages"].as_array_mut() else {
            continue;
        };
        for message in messages {
            let Some(text) = message["text"].as_str() else {
                continue;
            };
            message["text"] = protected(text, repo)?.into();
        }
    }
    Ok(())
}

/// The Hub's session list after the device's protection: a title or milestone can hold a value
/// the member registered after it was uploaded, and it reaches the agent as the messages do.
fn protect_sessions(sessions: &[SessionView], repo: Option<&Path>) -> Result<Vec<SessionView>> {
    let mut sessions = sessions.to_vec();
    for session in &mut sessions {
        if let Some(title) = &session.title {
            session.title = Some(protected(title, repo)?);
        }
        for milestone in &mut session.milestones {
            *milestone = protected(milestone, repo)?;
        }
    }
    Ok(sessions)
}

/// Create `dir` and its missing parents so only the member can open them.
fn private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Write `bytes` to `path` so only the member can read it.
fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct MonthView {
    state: String,
    #[serde(default)]
    repositories: Vec<RepositoryView>,
}

#[derive(Debug, Deserialize)]
struct RepositoryView {
    repository: String,
    #[serde(default)]
    repository_id: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    state: Option<String>,
    /// The section as the member's agent reads it; their text must name it when it is put.
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    sessions: Vec<SessionView>,
}

/// One organization day, `[start, end)`.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct DayWindow {
    start: String,
    end: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
struct SessionView {
    session: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: String,
    #[serde(default)]
    turns: u32,
    #[serde(default)]
    dates: Vec<String>,
    #[serde(default)]
    milestones: Vec<String>,
    /// The organization days the Hub keeps of the session; nothing outside them is read.
    #[serde(default)]
    days: Option<Vec<DayWindow>>,
}

impl SessionView {
    /// The kept days as instants, in order. Empty when the Hub sent none or one is unreadable,
    /// so nothing of the session is read.
    fn windows(&self) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
        let mut windows = Vec::new();
        for day in self.days.iter().flatten() {
            match (instant(&day.start), instant(&day.end)) {
                (Some(start), Some(end)) if start < end => windows.push((start, end)),
                _ => return Vec::new(),
            }
        }
        windows.sort();
        windows
    }
}

fn instant(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Whether a contribution was saved inside one of `windows`, judged by its commit time, the
/// time the Hub keys a month's days by; `None` when it carries no such time to judge.
fn placed(item: &serde_json::Value, windows: &[(DateTime<Utc>, DateTime<Utc>)]) -> Option<bool> {
    if item["time_basis"] != "committed_at" {
        return None;
    }
    let at = instant(item["saved_at"].as_str()?)?;
    Some(windows.iter().any(|(start, end)| *start <= at && at < *end))
}

fn parse_view(value: serde_json::Value) -> Result<MonthView> {
    serde_json::from_value(value).context("the Hub answered a month this agit cannot read")
}

/// The member's month, built first when it was never built, once it is ready.
fn ready_month(client: &Client, org: &str, month: &str) -> Result<MonthView> {
    let mut view = match client.month(org, month)? {
        Some(view) => parse_view(view)?,
        None => parse_view(client.month_build(org, month)?)?,
    };
    let started = std::time::Instant::now();
    while view.state != "ready" {
        if started.elapsed() > BUILD_WAIT {
            bail!("the Hub is still building {org} {month}; run the command again later");
        }
        ui::info(format_args!("the Hub is building your month in {org}…"));
        std::thread::sleep(BUILD_POLL);
        view = parse_view(
            client
                .month(org, month)?
                .context("the month disappeared while it was being built")?,
        )?;
    }
    Ok(view)
}

/// The files in an earlier draft's directory that agit did not write: the member's summary,
/// notes or anything else they saved there, which a new draft must not delete without their
/// word. agit's own are the README and the repository files that README lists; without a
/// README nothing there is known to be agit's.
fn members_files(dir: &Path) -> Result<Vec<String>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut ours = HashSet::new();
    if let Ok(readme) = std::fs::read_to_string(dir.join("README.md")) {
        ours.insert("README.md".to_owned());
        for line in readme.lines() {
            if let Some((repository, _)) = line.strip_prefix("- ").and_then(|r| r.split_once(": "))
                && let Some((owner, name)) = repository.split_once('/')
            {
                ours.insert(format!("{owner}__{name}.json"));
            }
        }
    }
    let mut names = Vec::new();
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !ours.contains(&name) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn draft(org: &str, month: &str, out: Option<PathBuf>, replace: bool) -> CmdResult {
    crate::input_argument(repo::valid_name(org))?;
    crate::input_argument(month_of(month))?;
    let dir = match &out {
        Some(dir) => crate::input_argument(chosen_out(dir))?,
        None => bundle_dir(org, month)?,
    };
    // A new draft replaces the earlier one in the private directory, since a repository's file
    // or a summary from it could hold what the member has hidden since. A file the member wrote
    // there goes only with their word, asked before anything is read.
    if out.is_none() && !replace {
        let theirs = members_files(&dir)?;
        if !theirs.is_empty() {
            let names = theirs.join(", ");
            if !ui::prompt::can_ask() {
                ui::error(&format!(
                    "{} holds {names}, which a new draft would delete; nothing was changed. Run it again with --replace to replace the earlier draft",
                    ui::tilde(&dir)
                ));
                return Ok(ExitCode::Precondition);
            }
            let question = format!(
                "A new draft deletes {names} in {}. Replace the earlier draft?",
                ui::tilde(&dir)
            );
            if ui::prompt::confirm(&question, false)? != Some(true) {
                println!("Nothing was changed.");
                return Ok(ExitCode::Ok);
            }
        }
    }
    let client = super::require_login()?;
    let author = client
        .credential_snapshot()
        .map(|credential| credential.account_id.unwrap_or(credential.username))
        .context("sign in with agit login first")?;
    let view = ready_month(&client, org, month)?;
    // A Hub that names no session's days predates them. The draft never reads a session over
    // the whole month in their place, so it stops before anything is read or written.
    let sessions = view.repositories.iter().flat_map(|r| &r.sessions);
    if sessions.clone().next().is_some() && sessions.clone().all(|s| s.days.is_none()) {
        ui::error(&format!(
            "the Hub does not say which days of {org} {month} it keeps; drafting needs a newer \
             Hub, and nothing was read"
        ));
        return Ok(ExitCode::Precondition);
    }
    // The whole bundle is built before the earlier draft is touched, so a Hub refusal or a
    // protection failure leaves it as it was.
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut written = Vec::new();
    for repository in &view.repositories {
        if repository.state.as_deref() != Some("ready") || repository.sessions.is_empty() {
            continue;
        }
        let (owner, name) = super::parse_slug(&repository.repository)?;
        let checkout = config::repo_dir(&owner, &name)?;
        let rules = crate::domain::repo::Repo::open(&checkout).map(|_| checkout.as_path());
        let mut contributions = Vec::new();
        let mut incomplete = BTreeMap::<String, serde_json::Value>::new();
        // Only the sessions the Hub lists for the member are read, through the Hub, and only
        // within the days it keeps of each: unpublished work, other people's sessions and the
        // days the member hid stay out of the bundle.
        for session in &repository.sessions {
            let windows = session.windows();
            let mut reasons = serde_json::Map::new();
            let (Some((since, _)), Some((_, before))) = (windows.first(), windows.last()) else {
                reasons.insert("no_kept_days".into(), serde_json::json!({ "count": 1 }));
                incomplete.insert(session.session.clone(), reasons.into());
                continue;
            };
            // The Hub bounds a page by when messages were sent, and a turn belongs to the day it
            // was saved on, after its messages: the page opens a day early, and each
            // contribution is then kept by its save time alone.
            let mut query = Query {
                author: author.clone(),
                since: (*since - chrono::Duration::days(1))
                    .to_rfc3339_opts(SecondsFormat::Millis, true),
                before: before.to_rfc3339_opts(SecondsFormat::Millis, true),
                session: Some(session.session.clone()),
                cursor: None,
                limit: Some(PAGE_LIMIT),
                assistant: Assistant::Last,
                select_by: Default::default(),
            };
            let mut unplaced = 0u64;
            loop {
                let page = serde_json::to_value(client.activity(&owner, &name, &query)?)?;
                for item in page["contributions"].as_array().into_iter().flatten() {
                    match placed(item, &windows) {
                        Some(true) => contributions.push(item.clone()),
                        Some(false) => {}
                        None => unplaced += 1,
                    }
                }
                // A page the Hub could not read in full is said so in the bundle, with a
                // reason even when the Hub names none.
                if page["incomplete"].as_bool() == Some(true) {
                    match page["reasons"].as_object() {
                        Some(more) if !more.is_empty() => reasons.extend(more.clone()),
                        _ => {
                            reasons.insert("hub".into(), serde_json::json!({ "count": 1 }));
                        }
                    }
                }
                match page["next_cursor"].as_str() {
                    Some(cursor) if page["has_more"].as_bool() == Some(true) => {
                        query.cursor = Some(cursor.to_owned());
                    }
                    _ => break,
                }
            }
            if unplaced > 0 {
                reasons.insert("unplaced".into(), serde_json::json!({ "count": unplaced }));
            }
            if !reasons.is_empty() {
                incomplete.insert(session.session.clone(), reasons.into());
            }
        }
        let mut contributions = join_chunks(contributions);
        protect(&mut contributions, rules)?;
        let sessions = protect_sessions(&repository.sessions, rules)?;
        // The agent writes about work, not people, and the bundle often goes to a hosted model.
        for item in &mut contributions {
            if let Some(fields) = item.as_object_mut() {
                fields.remove("author_name");
                fields.remove("author_email");
            }
        }
        let bundle = serde_json::json!({
            "repository": repository.repository,
            "repository_id": repository.repository_id,
            "version": repository.version,
            "private": repository.private,
            "month": month,
            "read_from": "hub",
            "sessions": sessions,
            "contributions": contributions,
            "incomplete": incomplete,
        });
        files.push((
            PathBuf::from(format!("{owner}__{name}.json")),
            serde_json::to_string_pretty(&bundle)?,
        ));
        written.push((repository.repository.clone(), repository.sessions.len()));
    }
    files.push((PathBuf::from("README.md"), readme(org, month, &written)));
    let replaced = out.is_none() && dir.exists();
    if replaced {
        // Written beside the earlier draft and moved over it once complete, so a failure while
        // writing leaves the earlier draft as it was.
        let staging = staging_dir(&dir, month);
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        private_dir(&staging)?;
        let wrote = files
            .iter()
            .try_for_each(|(name, text)| private_file(&staging.join(name), text.as_bytes()));
        if let Err(error) = wrote {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        let moved = std::fs::remove_dir_all(&dir).and_then(|()| std::fs::rename(&staging, &dir));
        if let Err(error) = moved {
            ui::error(&format!(
                "the earlier draft at {} was removed and the new one could not be put in its place",
                ui::tilde(&dir)
            ));
            return Err(error.into());
        }
    } else {
        private_dir(&dir)?;
        for (name, text) in &files {
            private_file(&dir.join(name), text.as_bytes())?;
        }
    }
    if written.is_empty() {
        ui::warning(&format!(
            "no repository in {org} holds turns recorded under your account in {month}"
        ));
    }
    println!("{}", dir.display());
    if replaced {
        ui::hint(&format!(
            "replaced the earlier draft at {}",
            ui::tilde(&dir)
        ));
    }
    ui::hint(&format!(
        "ask your agent to read {} and write {SUMMARY} there; then run `agit month publish {org} {month}` yourself",
        ui::tilde(&dir.join("README.md"))
    ));
    Ok(ExitCode::Ok)
}

fn readme(org: &str, month: &str, written: &[(String, usize)]) -> String {
    let mut text = format!(
        "# Month summary: {org}, {month}\n\n\
         Each JSON file here holds one repository's month: the sessions the Hub lists for you, \
         and what was asked and what came back in them (`contributions`). A non-empty \
         `incomplete` means part of a session could not be read; say so instead of treating it \
         as absent.\n\n\
         Write `{SUMMARY}` in this directory with one section per repository you want to \
         publish, in your own voice: what was worked on and what came of it. Start each section \
         with a `## owner/repo` heading. Name the sessions a section draws on as links whose \
         target is the session id, such as `[Lease pool rework](agit-…)`; a section names only \
         sessions listed in its file, and a section that names none is refused. Describe the \
         work, never evaluate people. Write each section only from its own repository's file: \
         readers of that repository see it, so nothing from another file, a private \
         repository's above all, goes into it.\n\n\
         Nothing leaves this machine until you run `agit month publish {org} {month}` at a \
         terminal and confirm. What you publish is yours alone until you share the month on the \
         website; then members who can read a repository see its section. A successful \
         publish deletes this bundle when it is the private one under the AgentGit home; one \
         written with `--out` stays until you delete it.\n\n\
         Repositories:\n\n"
    );
    for (repository, sessions) in written {
        text.push_str(&format!("- {repository}: {sessions} session(s)\n"));
    }
    text
}

/// One `## owner/repo` section of a summary.
#[derive(Debug, PartialEq, Eq)]
struct Section {
    repository: String,
    text: String,
    sources: Vec<String>,
}

/// The `## owner/repo` sections of `summary`, each with the session ids its text names. Text
/// before the first section heading is not published.
fn sections(summary: &str) -> Vec<Section> {
    let mut found: Vec<Section> = Vec::new();
    for line in summary.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            found.push(Section {
                repository: heading.trim().trim_matches('`').to_owned(),
                text: String::new(),
                sources: Vec::new(),
            });
        } else if let Some(section) = found.last_mut() {
            section.text.push_str(line);
            section.text.push('\n');
        }
    }
    for section in &mut found {
        section.text = section.text.trim().to_owned();
        let named: BTreeSet<String> = section
            .text
            .match_indices(meta::ID_PREFIX)
            .filter_map(|(at, _)| {
                section
                    .text
                    .get(at..at + meta::ID_PREFIX.len() + 40)
                    .filter(|id| meta::is_bare_id(id))
                    .map(str::to_owned)
            })
            .collect();
        section.sources = named.into_iter().collect();
    }
    found
}

/// What the secret scan found in `text`, by rule: the strict rules, which no inline annotation
/// turns off, and the values the member registered with `agit secrets`.
/// What a repository's draft recorded about its section: where to put the text, and the version
/// of the section the draft read.
struct Drafted {
    repository_id: String,
    version: Option<String>,
    /// The directory the draft is in.
    dir: PathBuf,
}

/// The draft of `repository` for `month` in the first of `dirs` that has one. A file whose own
/// repository or month differs is not its draft: file names can collide, and the confirmation
/// must name the repository the text is put on.
fn drafted(dirs: &[PathBuf], repository: &str, month: &str) -> Result<Option<Drafted>> {
    let Ok((owner, name)) = super::parse_slug(repository) else {
        return Ok(None);
    };
    for dir in dirs {
        let file = dir.join(format!("{owner}__{name}.json"));
        let raw = match std::fs::read(&file) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("could not read {}", file.display()));
            }
        };
        let draft: serde_json::Value = serde_json::from_slice(&raw)
            .with_context(|| format!("could not read {}", file.display()))?;
        if draft["repository"] != repository || draft["month"] != month {
            continue;
        }
        let Some(repository_id) = draft["repository_id"].as_str().filter(|id| !id.is_empty())
        else {
            return Ok(Some(Drafted {
                repository_id: String::new(),
                version: None,
                dir: dir.clone(),
            }));
        };
        return Ok(Some(Drafted {
            repository_id: repository_id.to_owned(),
            version: draft["version"].as_str().map(str::to_owned),
            dir: dir.clone(),
        }));
    }
    Ok(None)
}

fn findings(text: &str) -> Result<Vec<String>> {
    let empty = HashSet::new();
    let mut kinds: BTreeSet<String> =
        secrets::scan_text_with(text, &empty, secrets::Policy::STRICT)
            .into_iter()
            .map(|hit| hit.rule)
            .collect();
    kinds.extend(
        secrets::scan_text_registered(text, &empty)?
            .into_iter()
            .map(|hit| hit.rule),
    );
    Ok(kinds.into_iter().collect())
}

fn publish(org: &str, month: &str, file: Option<PathBuf>) -> CmdResult {
    crate::input_argument(repo::valid_name(org))?;
    crate::input_argument(month_of(month))?;
    let default_dir = bundle_dir(org, month)?;
    let path = file.unwrap_or_else(|| default_dir.join(SUMMARY));
    let summary = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let sections = sections(&summary);
    if sections.is_empty() {
        bail!(
            "{} has no `## owner/repo` section to publish",
            path.display()
        );
    }
    // Each section goes out with the version its draft read, never one read now: a section
    // that changed after the draft, by a hide or a rebuild, is then refused by the Hub instead
    // of being checked against what is left of it.
    let dirs: Vec<PathBuf> = path
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .chain([default_dir.clone()])
        .collect();
    let mut drafts = BTreeMap::new();
    for section in &sections {
        if let Some(draft) = drafted(&dirs, &section.repository, month)? {
            drafts.insert(section.repository.as_str(), draft);
        }
    }
    let mut problems = Vec::new();
    for section in &sections {
        match drafts.get(section.repository.as_str()) {
            None => problems.push(format!(
                "{}: no draft of it next to the summary; run agit month draft {org} {month}",
                section.repository
            )),
            Some(draft) if draft.version.is_none() => problems.push(format!(
                "{}: drafted before the Hub versioned month sections; run agit month draft {org} {month} again",
                section.repository
            )),
            Some(_) => {}
        }
        if section.text.is_empty() {
            problems.push(format!("{}: the section is empty", section.repository));
        } else if section.sources.is_empty() {
            problems.push(format!(
                "{}: name the sessions it was written from, as [title](agit-…) links",
                section.repository
            ));
        }
        let kinds = findings(&section.text)?;
        if !kinds.is_empty() {
            problems.push(format!(
                "{}: looks like it holds a credential ({}); remove it",
                section.repository,
                kinds.join(", ")
            ));
        }
    }
    if !problems.is_empty() {
        for problem in &problems {
            ui::error(problem);
        }
        return Ok(ExitCode::Policy);
    }
    // A person at a terminal, not an agent's tool shell under a pseudo-terminal.
    if !ui::prompt::can_ask() {
        ui::error("publishing asks you to confirm at a terminal; run it yourself");
        return Ok(ExitCode::Interactive);
    }
    let client = super::require_login()?;
    ui::section(&format!("Your text for {org}, {month}"));
    for section in &sections {
        println!(
            "  {}  {} characters, {} session(s); only you, then readers of {} once you share the month on the website",
            section.repository,
            section.text.chars().count(),
            section.sources.len(),
            section.repository
        );
    }
    let question = format!("Put these {} section(s) on the Hub?", sections.len());
    if ui::prompt::confirm(&question, false)? != Some(true) {
        println!("Nothing was sent.");
        return Ok(ExitCode::Ok);
    }
    for (sent, section) in sections.iter().enumerate() {
        let draft = &drafts[section.repository.as_str()];
        let version = draft.version.as_deref().unwrap_or_default();
        if let Err(error) = client.month_put_authored(
            org,
            month,
            &draft.repository_id,
            version,
            &section.text,
            &section.sources,
        ) {
            ui::error(&format!(
                "{} of {} section(s) were put on the Hub before {} failed",
                sent,
                sections.len(),
                section.repository
            ));
            if error
                .chain()
                .find_map(|cause| cause.downcast_ref::<ApiError>())
                .is_some_and(|api| api.status == 409)
            {
                ui::error(&format!(
                    "{} changed on the Hub after you drafted it; draft the month again with agit month draft {org} {month}",
                    section.repository
                ));
                return Ok(ExitCode::Precondition);
            }
            return Err(error);
        }
    }
    ui::success(&format!(
        "{} section(s) are on the Hub, yours alone until you share the month on the website",
        sections.len()
    ));
    let _ = std::fs::remove_dir_all(staging_dir(&default_dir, month));
    if default_dir.exists() {
        std::fs::remove_dir_all(&default_dir)
            .with_context(|| format!("could not delete {}", default_dir.display()))?;
        ui::hint(&format!("deleted the bundle {}", ui::tilde(&default_dir)));
    }
    // A bundle written with --out is where the member chose to put it; it is theirs to delete.
    let kept: BTreeSet<&Path> = drafts
        .values()
        .map(|draft| draft.dir.as_path())
        .filter(|dir| *dir != default_dir.as_path())
        .collect();
    for dir in kept {
        ui::hint(&format!(
            "the bundle at {} is still there; delete it when you no longer need it",
            ui::tilde(dir)
        ));
    }
    Ok(ExitCode::Ok)
}

fn clean(org: &str, month: &str) -> CmdResult {
    crate::input_argument(repo::valid_name(org))?;
    crate::input_argument(month_of(month))?;
    let dir = bundle_dir(org, month)?;
    let _ = std::fs::remove_dir_all(staging_dir(&dir, month));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("could not delete {}", dir.display()))?;
        println!("Deleted {}", ui::tilde(&dir));
    } else {
        println!("No bundle for {org} {month}.");
    }
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary publishes one section per `## owner/repo` heading, each naming the session ids
    /// its links point at, once each; text before the first heading and ids of the wrong shape
    /// are not part of any section. A parser that cut sections at any heading level, or took
    /// every `agit-` prefix as an id, would publish text the member did not mark or cite
    /// sessions that do not exist.
    #[test]
    fn a_summary_splits_into_repository_sections_with_the_sessions_they_name() {
        let one = format!("agit-{}", "a".repeat(40));
        let two = format!("agit-{}", "b".repeat(40));
        let summary = format!(
            "# October\n\nNot published.\n\n## acme/hub\n\nReworked the pool ([Lease pool]({one})).\n### Detail\nAgain [here]({one}) and agit-short.\n\n## `acme/cli`\n\n[Login]({two})\n"
        );
        assert_eq!(
            sections(&summary),
            vec![
                Section {
                    repository: "acme/hub".into(),
                    text: format!(
                        "Reworked the pool ([Lease pool]({one})).\n### Detail\nAgain [here]({one}) and agit-short."
                    ),
                    sources: vec![one.clone()],
                },
                Section {
                    repository: "acme/cli".into(),
                    text: format!("[Login]({two})"),
                    sources: vec![two],
                },
            ]
        );
    }

    /// `--out` never takes a directory the member already has, nor one inside a Git work tree,
    /// written plainly or through "..": making an existing directory private would change its
    /// mode, and a bundle inside a work tree could be committed with it.
    #[test]
    fn out_takes_only_a_new_directory_outside_a_work_tree() {
        let root = tempfile::tempdir().unwrap();
        let plain = root.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(chosen_out(&plain).is_err());
        assert!(chosen_out(&plain.join("month")).is_ok());
        let work = root.path().join("work");
        std::fs::create_dir_all(work.join(".git")).unwrap();
        assert!(chosen_out(&work.join("src").join("month")).is_err());
        // Through a directory that doesn't exist yet and back out, into the work tree.
        assert!(
            chosen_out(
                &root
                    .path()
                    .join("new")
                    .join("..")
                    .join("work")
                    .join("month")
            )
            .is_err()
        );
    }

    /// A message that pages split is protected as one text: the chunks of one commit and line
    /// are joined in order, and a contribution left with nothing of its own is dropped. Protecting
    /// chunk by chunk would miss a value cut at the boundary.
    #[test]
    fn chunks_of_one_message_are_joined_before_protection() {
        let joined = join_chunks(vec![
            serde_json::json!({"commit": "c1", "messages": [
                {"line": 1, "text": "first", "text_offset": 0, "text_complete": true},
                {"line": 2, "text": "split ", "text_offset": 0, "text_complete": false},
            ]}),
            serde_json::json!({"commit": "c1", "messages": [
                {"line": 2, "text": "value", "text_offset": 6, "text_complete": true},
            ]}),
            serde_json::json!({"commit": "c2", "messages": [
                {"line": 1, "text": "next", "text_offset": 0, "text_complete": true},
            ]}),
        ]);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0]["messages"][1]["text"], "split value");
        assert_eq!(joined[0]["messages"][1]["text_complete"], true);
        assert_eq!(joined[1]["commit"], "c2");
    }

    /// The bundle can hold private originals, so its directory and files are the member's alone
    /// even under a permissive umask. Files created with the process default would be readable
    /// by every account on a shared machine.
    #[cfg(unix)]
    #[test]
    fn the_bundle_is_readable_only_by_its_member() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("month").join("acme").join("2026-10");
        private_dir(&dir).unwrap();
        private_file(&dir.join("README.md"), b"x").unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("README.md")), 0o600);
    }
}
