//! Query submitted user and assistant messages without requiring a local clone.

use super::CmdResult;
use crate::{
    ExitCode,
    domain::{
        activity::{
            self, Alias, Assistant, Attribution, AttributionBody, AttributionRequest, Facts,
            FactsRequest, Page, Query, Reason, SentBasis, TimeBasis,
        },
        repo::Repo,
    },
    hub::{self, client::Client},
    infra::config,
    ui,
};
use anyhow::{Result, bail};
use clap::{Args as ClapArgs, ValueEnum};

/// Coverage requests stay below common request body limits.
const COVERAGE_CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Json,
    Markdown,
}

#[derive(ClapArgs)]
pub struct Args {
    /// Agent repository (owner/name).
    pub repo: String,
    /// Hub username/account ID, or exact legacy Git author name/email.
    #[arg(long)]
    pub author: String,
    /// Inclusive send time: the time the harness recorded each message, or its save time.
    #[arg(long)]
    pub since: String,
    /// Exclusive send time, as YYYY-MM-DD (UTC) or RFC3339 with a timezone.
    #[arg(long)]
    pub before: String,
    /// Branch name or exact AgentGit session ID.
    #[arg(long)]
    pub session: Option<String>,
    /// Continue the same query using the returned cursor.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Maximum message chunks in this page (1-200).
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Assistant messages to include: every one, or only the last before each user message.
    #[arg(long, value_enum, default_value = "all")]
    pub assistant: Assistant,
    /// Output format (default: JSON in pipes, Markdown on a terminal).
    #[arg(long, value_enum)]
    pub format: Option<Format>,
}

pub fn run(args: Args) -> CmdResult {
    match run_query(args) {
        Ok(code) => Ok(code),
        Err(error) if error.is::<activity::RequestError>() => crate::input_argument(Err(error)),
        Err(error) => {
            super::fix::register_terminal_api_error(&error);
            ui::error(&format!("activity failed: {error:#}"));
            let fallback = match error
                .downcast_ref::<hub::client::ApiError>()
                .map(|api| api.status)
            {
                Some(400) => ExitCode::Usage,
                Some(404) => ExitCode::Ref,
                Some(409 | 412) => ExitCode::Precondition,
                _ => ExitCode::Network,
            };
            Ok(super::terminal_error_code(&error, fallback))
        }
    }
}

fn run_query(args: Args) -> CmdResult {
    let (owner, name) = crate::input_argument(super::parse_slug(&args.repo))?;
    crate::input_argument(crate::domain::repo::valid_name(&owner))?;
    crate::input_argument(crate::domain::repo::valid_name(&name))?;
    let query = Query {
        author: args.author,
        since: args.since,
        before: args.before,
        session: args.session,
        cursor: args.cursor,
        limit: Some(args.limit),
        assistant: args.assistant,
    };
    crate::input_argument(query.selection())?;
    let client = super::require_login()?;
    let slug = format!("{owner}/{name}");
    let dir = config::repo_dir(&owner, &name)?;
    let (source, page) = if let Some(repo) = Repo::open(&dir) {
        super::migration::check_readonly_repo_startup(&repo)?;
        let identity = hub::identity::resolve_transport_target(&repo, &client, &owner, &name)?;
        if let Some(pinned) = hub::identity::read(&repo)? {
            anyhow::ensure!(
                pinned == identity,
                "the local checkout belongs to another Hub or repository"
            );
        }
        // A continuation reads the snapshot that its first page fetched.
        if query.cursor.is_none() {
            let outcome = hub::git::run_for_remote(
                &repo,
                &[
                    "fetch",
                    "--prune",
                    "origin",
                    "+refs/heads/*:refs/remotes/origin/*",
                ],
                &identity,
            )?;
            if !outcome.ok() {
                ui::error(&format!("activity sync failed: {}", outcome.stderr.trim()));
                return Ok(ExitCode::Network);
            }
        }
        let mut attribution = HubAttribution {
            client: &client,
            owner: &owner,
            name: &name,
            agent_id: &identity.agent_id,
        };
        let page = activity::page(
            &mut activity::local::LocalStore::new(&repo),
            &mut attribution,
            &slug,
            &format!("local:{}:{}", identity.hub, identity.agent_id),
            &query,
        )?;
        ("local", page)
    } else {
        anyhow::ensure!(
            !dir.exists(),
            "local repository exists but cannot be opened: {}",
            dir.display()
        );
        ("hub", client.activity(&owner, &name, &query)?)
    };
    if matches!(args.format, Some(Format::Markdown)) || (args.format.is_none() && ui::is_tty()) {
        markdown(source, &page);
    } else {
        let mut value = serde_json::to_value(page)?;
        value["source"] = source.into();
        println!("{value}");
    }
    Ok(ExitCode::Ok)
}

fn markdown(source: &str, page: &Page) {
    println!(
        "# Conversation activity: {}\n\nSource: {source}. Times are in UTC.\n",
        page.repo
    );
    for contribution in &page.contributions {
        let author = [
            contribution.author_name.as_str(),
            contribution.author_email.as_str(),
            contribution
                .author_account_id
                .as_deref()
                .unwrap_or_default(),
        ]
        .into_iter()
        .find(|value| !value.is_empty())
        .unwrap_or("unknown author");
        let basis = match contribution.time_basis {
            TimeBasis::CommittedAt => "committed",
            TimeBasis::PublishedAt => "published",
        };
        let turn = contribution
            .turn
            .map(|turn| format!(", turn {turn}"))
            .unwrap_or_default();
        println!(
            "## {} ({basis}) · {author}\n\nSession {}{turn}; commit {}\n",
            contribution.saved_at, contribution.session, contribution.commit
        );
        for message in &contribution.messages {
            let role = if message.role == "user" {
                "User"
            } else {
                "Assistant"
            };
            let part = match (message.text_offset, message.text_complete) {
                (0, true) => String::new(),
                (offset, complete) => format!(
                    ", from byte {offset}{}",
                    if complete { "" } else { ", continues" }
                ),
            };
            let saved = if message.sent_basis == SentBasis::Saved {
                ", time saved"
            } else {
                ""
            };
            println!(
                "**{role}** {}{saved} (line {}{part})\n\n{}\n",
                message.sent_at, message.line, message.text
            );
        }
    }
    if page.incomplete {
        let reasons: Vec<_> = page
            .reasons
            .iter()
            .map(|(name, reason)| format!("{name} ({})", reason.count))
            .collect();
        println!("Incomplete: {}", reasons.join(", "));
    }
    if !page.warnings.is_empty() {
        let warnings: Vec<_> = page.warnings.iter().map(String::as_str).collect();
        println!("Warnings: {}", warnings.join(", "));
    }
    if let Some(cursor) = &page.next_cursor {
        println!("More results: repeat with --cursor {cursor}");
    }
}

/// Projected commits in a clone carry no identity; the Hub's accepted records supply it.
struct HubAttribution<'a> {
    client: &'a Client,
    owner: &'a str,
    name: &'a str,
    agent_id: &'a str,
}

impl HubAttribution<'_> {
    fn call(&self, request: AttributionRequest) -> Result<AttributionBody> {
        self.client
            .activity_attribution(self.owner, self.name, self.agent_id, &request)
    }
}

impl Attribution for HubAttribution<'_> {
    fn aliases(&mut self, sessions: &[String]) -> Result<Vec<Alias>> {
        match self.call(AttributionRequest::Aliases(sessions.to_vec()))? {
            AttributionBody::Aliases(aliases) => Ok(aliases),
            _ => bail!("the Hub answered another activity attribution request"),
        }
    }

    fn facts(&mut self, request: &FactsRequest) -> Result<Facts> {
        match self.call(AttributionRequest::Facts(request.clone()))? {
            AttributionBody::Facts(facts) => Ok(facts),
            _ => bail!("the Hub answered another activity attribution request"),
        }
    }

    fn missing(&mut self, commits: &[String]) -> Result<Reason> {
        let mut missing = Reason::default();
        for chunk in commits.chunks(COVERAGE_CHUNK) {
            match self.call(AttributionRequest::Missing(chunk.to_vec()))? {
                AttributionBody::Missing(part) => missing.absorb(part),
                _ => bail!("the Hub answered another activity attribution request"),
            }
        }
        Ok(missing)
    }
}
