//! Search readable conversation history with evidence and pagination metadata intact.
//!
//! Machine output preserves uncertainty and unknown qualifiers because an incomplete scan
//! or an unsupported filter cannot establish that no relevant prior work exists.

use super::{CmdResult, require_login};
use crate::domain::query::Query;
use crate::hub::{AgentHit, PersonHit, PrHit, SearchFilters, SearchHit, SearchPage};
use crate::hub::{CodeOriginNotConfirmed, valid_code_origin};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

mod local_git;

/// The allowed types. Matches the hub's `SearchType`.
const KINDS: &[&str] = &["sessions", "agents", "prs", "people"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorpusScope {
    Mine,
    Org,
    Public,
    Repository(String),
}

impl std::str::FromStr for CorpusScope {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "mine" => Ok(Self::Mine),
            "org" => Ok(Self::Org),
            "public" => Ok(Self::Public),
            _ => {
                let (owner, name) = super::parse_slug(value).map_err(|_| {
                    "scope must be mine, org, public, or an explicit owner/repo".to_owned()
                })?;
                if value.trim() != value {
                    return Err("scope must not contain surrounding whitespace".into());
                }
                for component in [&owner, &name] {
                    if component.trim() != component {
                        return Err("scope components must not contain whitespace".into());
                    }
                    crate::domain::repo::valid_name(component).map_err(|e| e.to_string())?;
                }
                Ok(Self::Repository(value.to_ascii_lowercase()))
            }
        }
    }
}

impl CorpusScope {
    fn apply(&self, query: &str, account: Option<&str>) -> anyhow::Result<String> {
        if *self == Self::Org {
            return Ok(query.to_owned());
        }
        let mut expected = Query::parse(query);
        let (key, value, existing) = match self {
            Self::Mine => {
                let account = account.ok_or_else(|| anyhow::anyhow!("missing Hub identity"))?;
                if account.trim() != account {
                    anyhow::bail!("Hub returned an invalid account name");
                }
                crate::domain::repo::valid_name(account)?;
                let value = account.to_ascii_lowercase();
                ("owner", value.clone(), expected.owner.replace(value))
            }
            Self::Org => unreachable!("organization scope is a server-side corpus predicate"),
            Self::Public => (
                "is",
                "public".to_owned(),
                expected.visibility.replace("public".into()),
            ),
            Self::Repository(slug) => ("agent", slug.clone(), expected.agent.replace(slug.clone())),
        };
        if existing.is_some_and(|existing| existing != value) {
            anyhow::bail!(
                "--scope conflicts with the query's {key}: qualifier; remove that qualifier or choose a matching scope"
            );
        }
        // A prefix stays outside an unfinished quoted phrase in the original query. The parsed
        // query must retain every original condition and the requested corpus restriction.
        let scoped = if query.is_empty() {
            format!("{key}:{value}")
        } else {
            format!("{key}:{value} {query}")
        };
        if Query::parse(&scoped) != expected {
            anyhow::bail!("query cannot preserve the requested scope; check its qualifiers");
        }
        Ok(scoped)
    }
}

#[derive(ClapArgs)]
pub struct Args {
    /// Query. Supports qualifiers: in:prompt|reply|tool|output|edit|summary, owner:,
    /// agent:, runtime:, tool:, path:, turns:>20, "quoted phrases", -exclude
    #[arg(value_name = "query", default_value = "")]
    pub query: String,

    /// Additional query; repeat to search a batch with shared options
    #[arg(
        short = 'Q',
        long = "query",
        value_name = "query",
        allow_hyphen_values = true
    )]
    pub queries: Vec<String>,

    /// Restrict to one Agent repo (owner/name)
    #[arg(long, value_name = "owner/name")]
    pub repo: Option<String>,

    /// Restrict to a repo owner (person or organization)
    #[arg(long, value_name = "name")]
    pub owner: Option<String>,

    /// Match the saved version's Git author name or email (exact, case insensitive)
    #[arg(long, value_name = "name/email")]
    pub author: Option<String>,

    /// Saved at or after this UTC date or RFC3339 timestamp
    #[arg(long, value_name = "date/time")]
    pub since: Option<String>,

    /// Saved before this UTC date or RFC3339 timestamp (exclusive)
    #[arg(long, value_name = "date/time")]
    pub before: Option<String>,

    /// Restrict to a runtime, such as claude-code or codex
    #[arg(long, value_name = "runtime")]
    pub runtime: Option<String>,

    /// Match event scope; repeat to include multiple scopes
    #[arg(long = "in", value_parser = ["prompt", "reply", "tool", "output", "edit", "summary"])]
    pub scopes: Vec<String>,

    /// Restrict to calls of this tool
    #[arg(long, value_name = "name")]
    pub tool: Option<String>,

    /// Restrict to file-edit paths containing this fragment
    #[arg(long, value_name = "fragment")]
    pub path: Option<String>,

    /// What to search: sessions, agents, prs, people
    #[arg(
        short = 't',
        long = "type",
        default_value = "sessions",
        value_name = "kind",
        value_parser = clap::builder::PossibleValuesParser::new(KINDS.iter().copied())
    )]
    pub kind: String,

    /// Restrict sessions or agents to your repos, membership organizations, public, or one repo
    #[arg(long, value_name = "mine|org|public|owner/repo")]
    pub scope: Option<CorpusScope>,

    /// Restrict sessions to this code Git repository exact origin (requires Hub support)
    #[arg(long)]
    pub here: bool,

    #[arg(skip)]
    code_origin: Option<String>,

    /// Max hits to return
    #[arg(short = 'n', long, default_value = "10", value_name = "count")]
    pub limit: usize,

    /// Page of results (1-based)
    #[arg(long, default_value = "1", value_name = "n")]
    pub page: usize,

    /// Order: best, recent, turns
    #[arg(long, value_name = "order", value_parser = ["best", "recent", "turns"])]
    pub sort: Option<String>,

    /// Show how many hits each type has, then exit
    #[arg(long)]
    pub counts: bool,

    /// Emit raw structured JSON (also the default when stdout is not a terminal)
    #[arg(long)]
    pub mcp: bool,
}

impl Args {
    fn saved_filters(&self) -> Result<SearchFilters, String> {
        let filters = SearchFilters {
            code_origin: self.code_origin.clone(),
            author: self.author.clone(),
            since: self.since.clone(),
            before: self.before.clone(),
        }
        .normalized()
        .map_err(|e| e.to_string())?;
        if filters.active() && (self.kind != "sessions" || self.counts) {
            return Err("--author, --since and --before require session search; use its total for the filtered count".into());
        }
        Ok(filters)
    }
}

const MAX_BATCH_QUERIES: usize = 16;
const BATCH_CONCURRENCY: usize = 4;
const MAX_QUERY_CHARS: usize = 256;

pub fn run(mut args: Args) -> CmdResult {
    let mut queries = match effective_queries(&args) {
        Ok(queries) => queries,
        Err(message) => {
            ui::error(&message);
            return Ok(ExitCode::Usage);
        }
    };
    let client = require_login()?;
    if args.here {
        args.code_origin = match current_code_origin() {
            Ok(origin) => Some(origin),
            Err(()) => {
                ui::error("--here requires a code Git repository with one credential-free origin.");
                ui::hint(
                    "set one explicit origin; remove passwords, HTTP userinfo, query and fragment data from the origin and URL rewrites; SSH usernames are allowed",
                );
                return Ok(ExitCode::Precondition);
            }
        };
    }
    if let Some(scope) = &args.scope {
        let account = if *scope == CorpusScope::Mine {
            match client.me() {
                Ok(me) => Some(me.username),
                Err(error) => return failed(error),
            }
        } else {
            None
        };
        queries = match scoped_queries(scope, &queries, account.as_deref()) {
            Ok(queries) => queries,
            Err(error) => {
                ui::error(&error.to_string());
                return Ok(ExitCode::Usage);
            }
        };
    }
    if queries.len() > 1 {
        let (value, failed) = batch(&client, &args, &queries);
        println!("{}", serde_json::to_string(&value)?);
        return Ok(if failed {
            ExitCode::Failure
        } else {
            ExitCode::Ok
        });
    }
    args.query = queries
        .into_iter()
        .next()
        .expect("a validated query exists");
    if args.mcp || !ui::is_tty() {
        return match structured(&client, &args, &args.query) {
            Ok(value) => {
                println!("{}", serde_json::to_string(&value)?);
                Ok(ExitCode::Ok)
            }
            Err(error) => failed(error),
        };
    }
    if args.counts {
        return counts(&client, &args);
    }
    match args.kind.as_str() {
        "sessions" => sessions(&client, &args),
        "agents" => agents(&client, &args),
        "prs" => prs(&client, &args),
        _ => people(&client, &args),
    }
}

fn scoped_queries(
    scope: &CorpusScope,
    queries: &[String],
    account: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    queries
        .iter()
        .map(|query| {
            let query = scope.apply(query, account)?;
            anyhow::ensure!(
                query.chars().count() <= MAX_QUERY_CHARS,
                "each scoped query must be at most {MAX_QUERY_CHARS} characters"
            );
            Ok(query)
        })
        .collect()
}

fn effective_queries(args: &Args) -> Result<Vec<String>, String> {
    if args.here && (args.counts || args.kind != "sessions") {
        return Err(
            "--here supports only --type sessions and cannot be used with --counts.".into(),
        );
    }
    if args.scope.is_some() && (args.counts || !matches!(args.kind.as_str(), "sessions" | "agents"))
    {
        return Err(
            "--scope supports --type sessions or agents and cannot be used with --counts.".into(),
        );
    }
    let saved_filters = args.saved_filters()?;
    if !(1..=100).contains(&args.limit) {
        return Err("--limit must be between 1 and 100".into());
    }
    if args.page == 0 {
        return Err("--page must be at least 1".into());
    }
    let mut filters = Vec::new();
    for (name, value) in [
        ("repo", &args.repo),
        ("owner", &args.owner),
        ("runtime", &args.runtime),
        ("tool", &args.tool),
        ("path", &args.path),
    ] {
        if let Some(value) = value {
            if value.trim().is_empty() || value.contains('"') || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "--{name} must be nonempty and contain no quotes or control characters"
                ));
            }
            filters.push(format!("{name}:\"{value}\""));
        }
    }
    filters.extend(args.scopes.iter().map(|scope| format!("in:{scope}")));
    let mut queries: Vec<String> = if args.query.is_empty() {
        Vec::new()
    } else {
        vec![args.query.clone()]
    };
    queries.extend(args.queries.clone());
    let has_filters =
        args.scope.is_some() || args.here || !filters.is_empty() || saved_filters.active();
    if queries.is_empty() && has_filters {
        queries.push(String::new());
    }
    if queries.is_empty() {
        return Err("provide a query or a filter; use --query repeatedly for a batch".into());
    }
    if queries.len() > MAX_BATCH_QUERIES {
        return Err(format!(
            "a batch supports at most {MAX_BATCH_QUERIES} queries"
        ));
    }
    let explicit_queries = !args.query.is_empty() || !args.queries.is_empty();
    for query in &mut queries {
        if query.trim().is_empty() && (!has_filters || explicit_queries) {
            return Err("queries must not be empty".into());
        }
        if !filters.is_empty() {
            if !query.matches('"').count().is_multiple_of(2) {
                return Err(
                    "queries with explicit filters must have balanced double quotes".into(),
                );
            }
            if !query.is_empty() {
                query.push(' ');
            }
            query.push_str(&filters.join(" "));
        }
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(format!(
                "each query must be at most {MAX_QUERY_CHARS} characters"
            ));
        }
    }
    Ok(queries)
}

#[derive(Debug, thiserror::Error)]
#[error(
    "the Hub did not confirm --scope org; search results were withheld. Upgrade the Hub before relying on organization-scoped search"
)]
struct ScopeNotConfirmed;

fn scoped_page<T: serde::de::DeserializeOwned>(
    client: &crate::hub::Client,
    args: &Args,
    query: &str,
) -> anyhow::Result<SearchPage<T>> {
    let filters = args.saved_filters().map_err(anyhow::Error::msg)?;
    let org = args.scope == Some(CorpusScope::Org);
    if !org && !args.here {
        return client.search_page_filtered_with_scope(
            &args.kind,
            query,
            args.sort.as_deref(),
            args.page,
            args.limit,
            &filters,
            None,
        );
    }
    let page: SearchPage<serde_json::Value> = client.search_page_filtered_with_scope(
        &args.kind,
        query,
        args.sort.as_deref(),
        args.page,
        args.limit,
        &filters,
        org.then_some("org"),
    )?;
    if args.here && page.kind != args.kind {
        return Err(CodeOriginNotConfirmed.into());
    }
    if org && (page.applied_scope.as_deref() != Some("org") || page.kind != args.kind) {
        return Err(ScopeNotConfirmed.into());
    }
    Ok(SearchPage {
        applied_scope: page.applied_scope,
        kind: page.kind,
        applied_filters: page.applied_filters,
        total: page.total,
        page: page.page,
        per: page.per,
        items: page
            .items
            .into_iter()
            .map(serde_json::from_value)
            .collect::<Result<_, _>>()?,
        incomplete: page.incomplete,
        unknown: page.unknown,
        terms: page.terms,
    })
}

fn scope_label<T>(page: &SearchPage<T>) {
    if page.applied_filters.code_origin.is_some() {
        println!("scope: exact code Git origin");
    }
    if page.applied_scope.as_deref() == Some("org") {
        println!("scope: readable repositories in your membership organizations");
    }
}

fn structured(
    client: &crate::hub::Client,
    args: &Args,
    query: &str,
) -> anyhow::Result<serde_json::Value> {
    if args.counts {
        let counts = client.search_counts(query)?;
        return Ok(serde_json::json!({
            "query": query,
            "counts": {
                "sessions": counts.sessions, "agents": counts.agents,
                "prs": counts.prs, "people": counts.people,
                "sessions_incomplete": counts.sessions_incomplete,
            },
        }));
    }
    let page: SearchPage<serde_json::Value> = scoped_page(client, args, query)?;
    let mut value = serde_json::json!({
        "query": query, "type": args.kind, "total": page.total,
        "page": page.page, "per": page.per,
        "has_more": page.per > 0 && page.page.saturating_mul(page.per) < page.total,
        "incomplete": page.incomplete, "unknown": page.unknown,
        "terms": page.terms, "hits": page.items, "applied_filters": page.applied_filters,
    });
    if let Some(scope) = page.applied_scope {
        value["applied_scope"] = serde_json::json!(scope);
    }
    Ok(value)
}

fn batch(
    client: &crate::hub::Client,
    args: &Args,
    queries: &[String],
) -> (serde_json::Value, bool) {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(queries.len()));
    std::thread::scope(|scope| {
        for _ in 0..BATCH_CONCURRENCY.min(queries.len()) {
            let client = client.clone();
            let next = &next;
            let results = &results;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(query) = queries.get(index) else {
                        break;
                    };
                    let result = structured(&client, args, query);
                    results
                        .lock()
                        .expect("batch result lock is available")
                        .push((index, result));
                }
            });
        }
    });
    let mut results = results.into_inner().expect("batch results are available");
    results.sort_by_key(|(index, _)| *index);
    let failed = results.iter().any(|(_, result)| result.is_err());
    let results: Vec<_> = results.into_iter().map(|(index, result)| match result {
        Ok(value) => serde_json::json!({"query": queries[index], "ok": true, "result": value}),
        Err(error) => serde_json::json!({"query": queries[index], "ok": false, "error": format!("{error:#}")}),
    }).collect();
    (
        serde_json::json!({"batch": true, "results": results}),
        failed,
    )
}

/// The one exit for a failed request.
///
/// Say "this hub may not have the feature yet" rather than a flat failure: a self-hosted hub can
/// lag the CLI, and what the user does then (upgrade the hub) has nothing to do with a mistyped
/// query.
fn failed(e: anyhow::Error) -> CmdResult {
    super::fix::register_terminal_api_error(&e);
    ui::error(&format!("search failed: {e:#}"));
    ui::hint("if this hub is self-hosted, it may be older than this CLI");
    Ok(
        if e.is::<ScopeNotConfirmed>() || e.is::<CodeOriginNotConfirmed>() {
            ExitCode::Precondition
        } else {
            super::terminal_error_code(&e, ExitCode::Network)
        },
    )
}

fn counts(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let c = match client.search_counts(&args.query) {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    // The session count can be a lower bound, hence `≥` — a number that looks exact is taken for
    // a conclusion more readily than one marked uncertain.
    let sess = if c.sessions_incomplete {
        format!("≥{}", c.sessions)
    } else {
        c.sessions.to_string()
    };
    println!("{}  {}", ui::bold(&sess), ui::dim("sessions"));
    println!("{}  {}", ui::bold(&c.agents.to_string()), ui::dim("agents"));
    println!(
        "{}  {}",
        ui::bold(&c.prs.to_string()),
        ui::dim("pull requests")
    );
    println!("{}  {}", ui::bold(&c.people.to_string()), ui::dim("people"));
    if c.sessions_incomplete {
        ui::hint("session count is a lower bound — the search is incomplete");
    }
    Ok(ExitCode::Ok)
}

/// The line carrying the query string and the total, plus the notices that qualify it. Shared by
/// every type.
fn header<T>(p: &SearchPage<T>, query: &str) {
    scope_label(p);
    for (key, value) in [
        ("author", &p.applied_filters.author),
        ("since", &p.applied_filters.since),
        ("before", &p.applied_filters.before),
    ] {
        if let Some(value) = value {
            println!("{key}: {value}");
        }
    }
    let total = if p.incomplete {
        format!("≥{}", p.total)
    } else {
        p.total.to_string()
    };
    println!(
        "{} hits  {}\n",
        ui::bold(&total),
        ui::dim(&format!("\"{query}\""))
    );
    if !p.unknown.is_empty() {
        ui::warning(&format!(
            "unsupported {}: {} — inspect the query before relying on these results",
            if p.unknown.len() == 1 {
                "a qualifier"
            } else {
                "qualifiers"
            },
            p.unknown.join(", ")
        ));
    }
}

fn footer<T>(p: &SearchPage<T>) {
    if p.incomplete {
        ui::warning("search results are incomplete — some readable content may not be covered");
        ui::hint("try narrowing with owner: or agent:, or retry later");
    }
    if p.per > 0 && p.total > p.page.max(1) * p.per {
        ui::hint(&format!(
            "more on page {} (--page {})",
            p.page + 1,
            p.page + 1
        ));
    }
}

fn current_code_origin() -> Result<String, ()> {
    let cwd = std::env::current_dir().map_err(|_| ())?;
    let repo = crate::domain::repo::Repo::at(cwd).local_objects_only();
    let git = local_git::Git::new().map_err(|_| ())?;
    let inside = git
        .output(&repo, &["rev-parse", "--is-inside-work-tree"], 32)
        .map_err(|_| ())?;
    if !inside.status.success() || !inside.stderr.is_empty() || inside.stdout != b"true\n" {
        return Err(());
    }
    let config = git
        .output(
            &repo,
            &[
                "config",
                "--local",
                "--no-includes",
                "--null",
                "--get-all",
                "remote.origin.url",
            ],
            4097,
        )
        .map_err(|_| ())?;
    if !config.status.success() || !config.stderr.is_empty() {
        return Err(());
    }
    let raw = config.stdout.strip_suffix(b"\0").ok_or(())?;
    let origin = std::str::from_utf8(raw).map_err(|_| ())?;
    if !valid_code_origin(origin) {
        return Err(());
    }
    // Saved code provenance uses Git's effective URL. Validate both spellings so a rewrite
    // cannot introduce credentials or make an unsafe configured origin eligible for search.
    let effective = git.effective_origin(&repo).map_err(|_| ())?;
    if !effective.status.success() || !effective.stderr.is_empty() {
        return Err(());
    }
    let raw = effective.stdout.strip_suffix(b"\n").ok_or(())?;
    let origin = std::str::from_utf8(raw).map_err(|_| ())?;
    if !valid_code_origin(origin) {
        return Err(());
    }
    Ok(origin.to_owned())
}

fn sessions(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let p: SearchPage<SearchHit> = match scoped_page(client, args, &args.query) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };

    if p.items.is_empty() {
        return nothing(&args.query, &p);
    }

    header(&p, &args.query);
    let s = ui::theme::symbols();
    for h in &p.items {
        // `scope` renders as a verb: this column answers "who did what", and that is the line
        // between this search and grep. Secondhand is marked — that sentence came out of a compact
        // summary, so a summariser wrote it and nobody said it.
        let mut tags = Vec::new();
        // The verdict tag comes first: scanning a screen of results, the first thing wanted is
        // "can this one be used". `unknown` is not shown — it is the normal case, and rendering it
        // as an "unknown" tag only hangs a lump of noise off every row.
        if let Some(verdict) = verdict_label(h.outcome.as_deref()) {
            tags.push(verdict);
        }
        if let Some(sc) = &h.scope {
            tags.push(ui::accent(scope_verb(sc)).to_string());
        }
        if h.secondhand {
            tags.push(ui::warn_text("secondhand").to_string());
        }
        if h.turns > 0 {
            tags.push(ui::dim(&format!("{} turns", h.turns)).to_string());
        }
        // The collapse count: left unsaid, the user reads the row as "only one person has done
        // this" — which is exactly what they are trying to judge.
        if h.group_size > 1 {
            tags.push(ui::dim(&format!("{} more like this", h.group_size - 1)).to_string());
        }
        println!(
            "{} {}  {}",
            ui::accent(s.node),
            ui::bold(&h.agent),
            tags.join(ui::dim(" · ").to_string().as_str())
        );

        let mut id = format!(
            "session {}",
            h.session_id.chars().take(12).collect::<String>()
        );
        if let Some(rt) = &h.runtime {
            id.push_str(&format!("  {rt}"));
        }
        if let Some(t) = &h.tool {
            id.push_str(&format!("  {t}"));
        }
        println!("  {}", ui::dim(&id));

        for line in ui::truncate(&h.excerpt, 300).lines() {
            println!("  {line}");
        }
        if !h.paths.is_empty() {
            println!("  {}", ui::dim(&h.paths.join("  ")));
        }
        // The reason and the tag must appear together: "looks unsolved" with no reason asks the
        // user to act on a judgement they cannot check — and that judgement is only a heuristic.
        if let Some(reason) = &h.outcome_reason {
            let strength = h.confidence.as_deref().unwrap_or("low");
            println!("  {}", ui::dim(&format!("↳ {reason} ({strength})")));
        }
        if h.other_hits > 0 {
            println!(
                "  {}",
                ui::dim(&format!("+{} more in this session", h.other_hits))
            );
        }
        if let Some(u) = &h.url {
            println!("  {}", ui::dim(u));
        }
        println!();
    }
    footer(&p);
    ui::hint("use --json for structured results; --query adds another search to a batch");
    Ok(ExitCode::Ok)
}

/// `in:` values → verbs. The wording matches the web interface.
fn scope_verb(scope: &str) -> &str {
    match scope {
        "prompt" => "asked",
        "reply" => "answered",
        "tool" => "ran",
        // "printed", not "ran": this column answers "who did what", and the subject of tool
        // output is the machine. The wording has to stay apart from `ran` — the two show up side
        // by side in the same column.
        "output" => "printed",
        "edit" => "edited",
        "summary" => "summarised",
        other => other,
    }
}

/// The verdict tag. The wording matches the web interface.
///
/// `unknown` and a missing value (a hub that does not send one) both return `None`: **showing
/// nothing** beats showing an "unknown" — the latter hangs something carrying no information off
/// every row, and having no signal is the normal case.
///
/// The wording is `looks`: this is a heuristic read off the shape of the transcript, not a verified
/// result. Saying `solved` / `failed` makes it sound harder than the evidence is.
fn verdict_label(outcome: Option<&str>) -> Option<String> {
    match outcome? {
        // "it got through" takes the accent color.
        "worked" => Some(ui::accent("looks solved").to_string()),
        // "it did not get through" is **not bad news** — it saves as much time as a success does.
        // Hence the neutral dim rather than the error color: that would read as something being
        // wrong with the result itself.
        "failed" => Some(ui::dim("looks unsolved").to_string()),
        _ => None,
    }
}

fn agents(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let p: SearchPage<AgentHit> = match scoped_page(client, args, &args.query) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if p.items.is_empty() {
        return nothing(&args.query, &p);
    }
    header(&p, &args.query);
    let s = ui::theme::symbols();
    for h in &p.items {
        let mut tags = vec![ui::dim(&h.visibility).to_string()];
        if h.fork {
            tags.push(ui::dim("fork").to_string());
        }
        if !h.category.is_empty() && h.category != "general" {
            tags.push(ui::dim(&h.category).to_string());
        }
        println!(
            "{} {}  {}",
            ui::accent(s.node),
            ui::bold(&h.slug),
            tags.join(ui::dim(" · ").to_string().as_str())
        );
        if let Some(u) = &h.url {
            println!("  {}", ui::dim(u));
        }
    }
    println!();
    footer(&p);
    Ok(ExitCode::Ok)
}

fn prs(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let p: SearchPage<PrHit> = match client.search_page_filtered(
        "prs",
        &args.query,
        args.sort.as_deref(),
        args.page,
        args.limit,
        &args.saved_filters().map_err(anyhow::Error::msg)?,
    ) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if p.items.is_empty() {
        return nothing(&args.query, &p);
    }
    header(&p, &args.query);
    let s = ui::theme::symbols();
    for h in &p.items {
        println!(
            "{} {}  {}  {}",
            ui::accent(s.node),
            ui::bold(&format!("#{}", h.number)),
            h.title.as_deref().unwrap_or("(no title)"),
            ui::dim(&h.state)
        );
        println!(
            "  {}",
            ui::dim(&format!(
                "{} → {}  by {}",
                h.source, h.target_branch, h.created_by
            ))
        );
        // `summary` is written by the agent, not by the author — worth knowing when the match
        // landed there.
        if !h.matched_in.is_empty() {
            println!(
                "  {}",
                ui::dim(&format!("matched in {}", h.matched_in.join(", ")))
            );
        }
    }
    println!();
    footer(&p);
    Ok(ExitCode::Ok)
}

fn people(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let p: SearchPage<PersonHit> = match client.search_page_filtered(
        "people",
        &args.query,
        args.sort.as_deref(),
        args.page,
        args.limit,
        &args.saved_filters().map_err(anyhow::Error::msg)?,
    ) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if p.items.is_empty() {
        return nothing(&args.query, &p);
    }
    header(&p, &args.query);
    let s = ui::theme::symbols();
    for h in &p.items {
        println!(
            "{} {}  {}",
            ui::accent(s.node),
            ui::bold(&format!("@{}", h.name)),
            ui::dim(&format!("{} · {} agents you can see", h.kind, h.agents))
        );
    }
    println!();
    footer(&p);
    Ok(ExitCode::Ok)
}

/// The empty result.
///
/// The hint "the corpus only covers what you can read" is necessary: private content appears
/// neither in the results nor in the counts, so what "nothing" means depends on who is asking.
fn nothing<T>(query: &str, p: &SearchPage<T>) -> CmdResult {
    scope_label(p);
    println!("nothing found for “{query}”.");
    if !p.unknown.is_empty() {
        ui::warning(&format!(
            "{} was searched as plain text, not as a filter — check the spelling",
            p.unknown.join(", ")
        ));
    }
    if p.incomplete {
        ui::warning("the search is incomplete — matching readable content may still exist");
    }
    ui::hint("the corpus only covers what you can read");
    Ok(ExitCode::Ok)
}

/// The MCP shape of a session.
///
#[cfg(test)]
mod tests {
    use super::CorpusScope;
    use crate::domain::query::Query;
    use clap::Parser;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        a: super::Args,
    }

    #[test]
    fn exact_origin_preserves_ssh_identity_and_rejects_credentials_and_ambiguous_forms() {
        for origin in [
            "https://Example.test:114/team/Repo.git",
            "ssh://git@example.test:2222/team/repo.git",
            "git@example.test:team/repo.git",
            "example.test:/team/repo",
            "ssh://git@[::1]:2222/repo",
        ] {
            assert!(super::valid_code_origin(origin), "{origin}");
        }
        for origin in [
            "https://token@example.test/repo",
            "ssh://git:password@example.test/repo",
            "https://example.test/repo?token=secret",
            "https://example.test/repo#secret",
            "ext::anything",
            "file:///tmp/repo",
            "/tmp/repo",
            "C:/repo",
            "https://example.test",
            "https://example.test/a/../repo",
            "https://example.test/%40secret",
            "git@example.test:repo\n",
            "https://example.test:0/repo",
            "https://example.test:65536/repo",
        ] {
            assert!(
                !super::valid_code_origin(origin),
                "unsafe identity was accepted"
            );
        }
    }

    #[test]
    fn raw_json_flag_is_optional() {
        let w = W::parse_from(["x", "query term"]);
        assert!(
            !w.a.mcp,
            "output mode can follow the terminal without a flag"
        );
        assert_eq!(w.a.query, "query term");
    }

    /// The default type is `sessions`.
    ///
    /// This pins a product judgement rather than an implementation detail: the question this
    /// feature answers is "has anyone done this before", and the answer sits in the **body of a
    /// session**. Defaulting to agents (the way GitHub defaults to repositories) turns it into a
    /// repo-name searcher — the least useful thing this can be.
    #[test]
    fn default_type_is_sessions() {
        let w = W::parse_from(["x", "cache"]);
        assert_eq!(w.a.kind, "sessions");
        assert_eq!(w.a.page, 1);
        assert!(
            w.a.sort.is_none(),
            "the default sort is the hub's to pick, not the client's"
        );
    }

    #[test]
    fn shared_filters_preserve_values_and_batch_order() {
        let w = W::parse_from([
            "x",
            "cache",
            "--query",
            "-stale",
            "--repo",
            "alice/demo",
            "--path",
            "my folder",
            "--in",
            "tool",
            "--in",
            "output",
        ]);
        let queries = super::effective_queries(&w.a).unwrap();
        assert_eq!(queries.len(), 2);
        assert!(queries[0].starts_with("cache "));
        assert!(queries[1].starts_with("-stale "));
        let parsed = crate::domain::query::Query::parse(&queries[0]);
        assert_eq!(parsed.agent.as_deref(), Some("alice/demo"));
        assert_eq!(parsed.path.as_deref(), Some("my folder"));
        assert_eq!(parsed.scopes.len(), 2);
    }

    #[test]
    fn explicit_filters_cannot_be_swallowed_by_an_unclosed_query_phrase() {
        for query in ["\"cache", "-\"cache", "cache \"miss"] {
            let args = W::parse_from(["x", "--query", query, "--repo", "alice/service"]);
            assert!(
                super::effective_queries(&args.a)
                    .unwrap_err()
                    .contains("balanced double quotes")
            );
        }
        let args = W::parse_from(["x", "--query", "-\"cache miss\"", "--repo", "alice/service"]);
        let queries = super::effective_queries(&args.a).unwrap();
        let parsed = crate::domain::query::Query::parse(&queries[0]);
        assert_eq!(parsed.agent.as_deref(), Some("alice/service"));
        assert!(parsed.unknown.is_empty());
    }

    #[test]
    fn malformed_or_unbounded_requests_fail_before_login() {
        for argv in [
            vec!["x"],
            vec!["x", "term", "--limit", "0"],
            vec!["x", "term", "--limit", "101"],
            vec!["x", "term", "--page", "0"],
            vec!["x", "term", "--repo", r#""owner:other"#],
            vec!["x", "--query", " ", "--owner", "alice"],
        ] {
            let w = W::parse_from(&argv);
            assert!(super::effective_queries(&w.a).is_err(), "{argv:?}");
        }
        let mut argv = vec!["x"];
        for _ in 0..=super::MAX_BATCH_QUERIES {
            argv.extend(["--query", "cache"]);
        }
        assert!(super::effective_queries(&W::parse_from(argv).a).is_err());
        let long = "x".repeat(super::MAX_QUERY_CHARS + 1);
        assert!(super::effective_queries(&W::parse_from(["x", &long]).a).is_err());
        assert!(W::try_parse_from(["x", "cache", "--sort", "fast"]).is_err());
        assert!(W::try_parse_from(["x", "cache", "--type", "whatever"]).is_err());
        assert_eq!(
            super::effective_queries(&W::parse_from(["x", "--owner", "alice"]).a).unwrap(),
            vec!["owner:\"alice\""]
        );
    }

    #[test]
    fn saved_filters_validate_before_login_and_allow_filter_only_queries() {
        let args = W::parse_from([
            "x",
            "--author",
            " Bob ",
            "--since",
            "2026-09-01T08:00:00+08:00",
            "--before",
            "2026-09-02",
        ])
        .a;
        assert_eq!(super::effective_queries(&args).unwrap(), vec![""]);
        let filters = args.saved_filters().unwrap();
        assert_eq!(filters.author.as_deref(), Some("bob"));
        assert_eq!(filters.since.as_deref(), Some("2026-09-01T00:00:00Z"));
        for argv in [
            vec!["x", "--author", " "],
            vec!["x", "--since", "2026-02-30"],
            vec!["x", "--since", "2026-09-01T08:00:00"],
            vec!["x", "--since", "2026-09-02", "--before", "2026-09-02"],
            vec!["x", "--author", "Bob", "--counts"],
            vec!["x", "--author", "Bob", "--type", "people"],
        ] {
            assert!(
                super::effective_queries(&W::parse_from(&argv).a).is_err(),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn saved_filters_require_matching_hub_acknowledgement() {
        use std::io::{Read, Write};
        for acknowledgement in [None, Some("other@example.org"), Some("bob@example.org")] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut connection, _) = listener.accept().unwrap();
                connection
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut buffer = [0; 8192];
                let n = connection.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..n]);
                assert!(request.contains("author=bob%40example.org"));
                assert!(request.contains("since=2026-09-01T00%3A00%3A00Z"));
                let mut body = serde_json::json!({"type":"sessions", "total":0, "page":1, "per":10, "items":[]});
                if let Some(author) = acknowledgement {
                    body["applied_filters"] =
                        serde_json::json!({"author":author, "since":"2026-09-01T00:00:00Z"});
                }
                let body = body.to_string();
                write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            });
            let args = W::parse_from([
                "x",
                "cache",
                "--author",
                "BOB@example.org",
                "--since",
                "2026-09-01",
            ])
            .a;
            let result = super::structured(&crate::hub::Client::for_hub(&base), &args, "cache");
            server.join().unwrap();
            assert_eq!(result.is_ok(), acknowledgement == Some("bob@example.org"));
            if let Err(error) = result {
                assert!(error.to_string().contains("did not acknowledge"));
            }
        }
    }

    const HERE_ORIGIN: &str = "https://example.test/team/repo.git";

    fn scoped_response<T: serde::de::DeserializeOwned>(
        args: &super::Args,
        body: serde_json::Value,
    ) -> anyhow::Result<crate::hub::SearchPage<T>> {
        use std::io::{Read, Write};
        use std::time::{Duration, Instant};

        args.saved_filters().expect("the fixture query is valid");
        if args.here {
            assert_eq!(args.code_origin.as_deref(), Some(HERE_ORIGIN));
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let org = args.scope == Some(CorpusScope::Org);
        let here = args.here;
        let kind = args.kind.clone();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut connection = loop {
                match listener.accept() {
                    Ok((connection, _)) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "search request did not arrive");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("search connection failed: {error}"),
                }
            };
            connection.set_nonblocking(false).unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            connection
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let count = connection.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "search request was truncated");
                bytes.extend_from_slice(&buffer[..count]);
                assert!(bytes.len() <= 16384, "search request exceeded its bound");
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.starts_with(&format!("GET /api/search/{kind}?q=cache&")));
            assert_eq!(request.contains("&scope=org&"), org);
            assert_eq!(request.contains("&code_origin="), here);
            if here {
                assert!(
                    request.contains("&code_origin=https%3A%2F%2Fexample.test%2Fteam%2Frepo.git&")
                );
            }
            let body = body.to_string();
            write!(connection, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let result = super::scoped_page(&crate::hub::Client::for_hub(&base), args, "cache");
        server.join().unwrap();
        result
    }

    #[test]
    fn org_confirmation_precedes_typed_items_for_nonempty_pages() {
        use crate::hub::{AgentHit, SearchHit};
        use serde_json::{Value, json};

        let args = W::parse_from(["x", "cache", "--type", "agents", "--scope", "org"]).a;
        let session = json!({
            "agent":"acme/demo", "session_id":"selected-session", "excerpt":"withheld hit"
        });
        assert!(serde_json::from_value::<SearchHit>(session.clone()).is_ok());
        assert!(serde_json::from_value::<AgentHit>(session.clone()).is_err());
        for body in [
            json!({"type":"sessions", "applied_scope":"org", "total":1, "items":[session]}),
            json!({"type":"agents", "total":1, "items":[{"invalid":"withheld hit"}]}),
        ] {
            for result in [
                scoped_response::<AgentHit>(&args, body.clone()).map(|_| ()),
                scoped_response::<Value>(&args, body).map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert!(error.is::<super::ScopeNotConfirmed>(), "{error:#}");
                assert!(!format!("{error:#}").contains("withheld hit"));
            }
        }
    }

    #[test]
    fn confirmed_org_page_decodes_items_and_retains_metadata() {
        use crate::hub::{AgentHit, SearchHit};
        use serde_json::json;

        for here in [false, true] {
            let mut args = W::parse_from([
                "x", "cache", "--type", "sessions", "--scope", "org", "--author", "Bob",
            ])
            .a;
            args.here = here;
            args.code_origin = here.then(|| HERE_ORIGIN.to_owned());
            let body = json!({
                "type":"sessions", "applied_scope":"org",
                "applied_filters":{"author":"bob", "code_origin":args.code_origin},
                "total":19, "page":3, "per":4, "incomplete":true,
                "unknown":["runtim:codex"], "terms":["cache"],
                "items":[{"agent":"acme/demo", "session_id":"selected-session", "excerpt":"selected hit"}]
            });
            let page = scoped_response::<SearchHit>(&args, body).unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].session_id, "selected-session");
            assert_eq!(page.applied_scope.as_deref(), Some("org"));
            assert_eq!(page.kind, "sessions");
            assert_eq!(page.applied_filters, args.saved_filters().unwrap());
            assert_eq!((page.total, page.page, page.per), (19, 3, 4));
            assert!(page.incomplete);
            assert_eq!(page.unknown, ["runtim:codex"]);
            assert_eq!(page.terms, ["cache"]);
        }

        for scope in [Some("org"), None] {
            let mut argv = vec!["x", "cache", "--type", "agents"];
            if let Some(scope) = scope {
                argv.extend(["--scope", scope]);
            }
            let args = W::parse_from(argv).a;
            let body = json!({"type":"agents", "applied_scope":"org", "total":1, "items":[{}]});
            let error = scoped_response::<AgentHit>(&args, body).unwrap_err();
            assert!(!error.is::<super::ScopeNotConfirmed>(), "{error:#}");
            assert!(format!("{error:#}").contains("missing field"));
        }
    }

    #[test]
    fn here_confirmation_precedes_typed_items_and_org_confirmation() {
        use crate::hub::{AgentHit, CodeOriginNotConfirmed, SearchHit};
        use serde_json::{Value, json};

        let agent =
            json!({"owner":"acme", "name":"demo", "slug":"acme/demo", "visibility":"public"});
        assert!(serde_json::from_value::<AgentHit>(agent.clone()).is_ok());
        assert!(serde_json::from_value::<SearchHit>(agent.clone()).is_err());
        for (org, origin, kind, org_error) in [
            (false, Some(HERE_ORIGIN), "agents", false),
            (false, None, "sessions", false),
            (
                false,
                Some("https://example.test/other/repo.git"),
                "sessions",
                false,
            ),
            (true, None, "agents", false),
            (true, Some(HERE_ORIGIN), "agents", false),
            (true, Some(HERE_ORIGIN), "sessions", true),
        ] {
            let mut args = W::parse_from(["x", "cache", "--here"]).a;
            args.code_origin = Some(HERE_ORIGIN.to_owned());
            args.scope = org.then_some(CorpusScope::Org);
            let body = json!({
                "type":kind, "total":1, "applied_filters":{"code_origin":origin},
                "items":[agent.clone()]
            });
            for result in [
                scoped_response::<SearchHit>(&args, body.clone()).map(|_| ()),
                scoped_response::<Value>(&args, body).map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert_eq!(
                    error.is::<super::ScopeNotConfirmed>(),
                    org_error,
                    "{error:#}"
                );
                assert_eq!(
                    error.is::<CodeOriginNotConfirmed>(),
                    !org_error,
                    "{error:#}"
                );
                assert!(!format!("{error:#}").contains("acme/demo"));
            }
        }
    }

    #[test]
    fn here_saved_filters_keep_priority_and_here_only_needs_no_org_acknowledgement() {
        use crate::hub::{CodeOriginNotConfirmed, SearchHit};
        use serde_json::{Value, json};

        let mut args =
            W::parse_from(["x", "cache", "--here", "--scope", "org", "--author", "Bob"]).a;
        args.code_origin = Some(HERE_ORIGIN.to_owned());
        let body = json!({
            "type":"agents", "total":1,
            "applied_filters":{"code_origin":HERE_ORIGIN, "author":"alice"}, "items":[{}]
        });
        for result in [
            scoped_response::<SearchHit>(&args, body.clone()).map(|_| ()),
            scoped_response::<Value>(&args, body).map(|_| ()),
        ] {
            let error = result.unwrap_err();
            assert!(!error.is::<CodeOriginNotConfirmed>());
            assert!(!error.is::<super::ScopeNotConfirmed>());
            assert!(format!("{error:#}").contains("did not acknowledge"));
        }

        args.scope = None;
        let mut body = json!({
            "type":"sessions", "total":1,
            "applied_filters":{"code_origin":HERE_ORIGIN, "author":"bob"},
            "items":[{"agent":"acme/demo", "session_id":"selected-session", "excerpt":"selected hit"}]
        });
        let page = scoped_response::<SearchHit>(&args, body.clone()).unwrap();
        assert_eq!(page.items[0].session_id, "selected-session");
        assert!(page.applied_scope.is_none());
        assert_eq!(page.applied_filters, args.saved_filters().unwrap());
        body["items"] = json!([{}]);
        let error = scoped_response::<SearchHit>(&args, body).unwrap_err();
        assert!(!error.is::<CodeOriginNotConfirmed>());
        assert!(!error.is::<super::ScopeNotConfirmed>());
        assert!(format!("{error:#}").contains("missing field"));
    }

    #[test]
    fn batch_preserves_wire_metadata_order_and_partial_failure() {
        use std::io::{Read, Write};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let server_peak = Arc::clone(&peak);
        let server = std::thread::spawn(move || {
            std::thread::scope(|scope| {
                for connection in listener.incoming().take(9) {
                    let mut connection = connection.unwrap();
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&server_peak);
                    scope.spawn(move || {
                        connection.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
                        let mut buffer = [0; 8192];
                        let n = connection.read(&mut buffer).unwrap();
                        let request = String::from_utf8_lossy(&buffer[..n]);
                        assert!(request.contains("author=bob"));
                        let index = request.split("q=q").nth(1).unwrap().chars().next().unwrap().to_digit(10).unwrap();
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(count, Ordering::SeqCst);
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        let (status, body) = if index == 3 {
                            ("500 Internal Server Error", serde_json::json!({"error":"search unavailable"}))
                        } else {
                            ("200 OK", serde_json::json!({"type":"sessions", "total":11, "page":2,
                                "per":5, "applied_filters":{"author":"bob"}, "incomplete":true, "unknown":["runtim:codex"], "terms":["cache"],
                                "items":[{"session_id":format!("s{index}"), "timestamp":"2026-09-08T00:00:00Z",
                                    "future_field":"preserved", "scope":"tool", "outcome":"unknown"}]}))
                        };
                        let body = body.to_string();
                        active.fetch_sub(1, Ordering::SeqCst);
                        write!(connection, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    });
                }
            });
        });
        let args = W::parse_from([
            "x", "cache", "--page", "2", "--limit", "5", "--author", "Bob",
        ])
        .a;
        let queries = (0..9).map(|n| format!("q{n}")).collect::<Vec<_>>();
        let (value, failed) = super::batch(&crate::hub::Client::for_hub(&base), &args, &queries);
        server.join().unwrap();
        assert!(failed);
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= super::BATCH_CONCURRENCY);
        let results = value["results"].as_array().unwrap();
        assert_eq!(results.len(), queries.len());
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result["query"], queries[index]);
            assert_eq!(result["ok"], index != 3);
            if index != 3 {
                assert_eq!(
                    result["result"]["hits"][0]["session_id"],
                    format!("s{index}")
                );
                assert_eq!(result["result"]["hits"][0]["future_field"], "preserved");
                assert_eq!(result["result"]["page"], 2);
                assert_eq!(result["result"]["has_more"], true);
                assert_eq!(result["result"]["incomplete"], true);
                assert_eq!(result["result"]["unknown"][0], "runtim:codex");
            }
        }
    }
    #[test]
    fn scopes_require_exact_repository_names() {
        let w = W::try_parse_from(["x", "cache", "--scope", "Alice/My-Repo"]).unwrap();
        assert_eq!(
            w.a.scope,
            Some(CorpusScope::Repository("alice/my-repo".into()))
        );
        for value in [
            "alice",
            "/repo",
            "alice/",
            "alice/repo/child",
            "alice/repo is:private",
            "alice/ repo",
            " alice/repo",
            "alice/repo ",
            "alice/repo\"",
        ] {
            assert!(W::try_parse_from(["x", "cache", "--scope", value]).is_err());
        }
    }

    #[test]
    fn scope_only_queries_are_filters_without_admitting_explicit_empty_queries() {
        for (scope, expected) in [
            ("public", "is:public"),
            ("mine", "owner:alice"),
            ("Alice/My-Repo", "agent:alice/my-repo"),
        ] {
            for kind in ["sessions", "agents"] {
                let args = W::parse_from(["x", "--scope", scope, "--type", kind]).a;
                let queries = super::effective_queries(&args).unwrap();
                assert_eq!(queries, [""]);
                assert_eq!(
                    super::scoped_queries(args.scope.as_ref().unwrap(), &queries, Some("alice"))
                        .unwrap(),
                    [expected]
                );
                for empty in ["", " \t "] {
                    for argv in [
                        vec!["x", "--scope", scope, "--type", kind, "-Q", empty],
                        vec!["x", "needle", "--scope", scope, "--type", kind, "-Q", empty],
                    ] {
                        assert!(super::effective_queries(&W::parse_from(argv).a).is_err());
                    }
                }
            }
        }
        assert!(super::effective_queries(&W::parse_from(["x"]).a).is_err());
    }

    #[test]
    fn scoped_queries_preserve_terms_and_other_qualifiers() {
        for query in [
            "cache in:tool -deprecated runtime:codex",
            "\"cache phrase\" -\"old phrase\"",
            "\"unfinished phrase",
            "path:\"a directory/file\" turns:>20",
            "https://example.test/cache runtime:codex",
            "runtim:codex",
        ] {
            for (scope, account) in [
                (CorpusScope::Mine, Some("Alice")),
                (CorpusScope::Org, None),
                (CorpusScope::Public, None),
                (CorpusScope::Repository("alice/my-repo".into()), None),
            ] {
                let original = Query::parse(query);
                let mut scoped = Query::parse(&scope.apply(query, account).unwrap());
                match scope {
                    CorpusScope::Mine => assert_eq!(scoped.owner.take().as_deref(), Some("alice")),
                    CorpusScope::Org => {}
                    CorpusScope::Public => {
                        assert_eq!(scoped.visibility.take().as_deref(), Some("public"));
                    }
                    CorpusScope::Repository(_) => {
                        assert_eq!(scoped.agent.take().as_deref(), Some("alice/my-repo"));
                    }
                }
                assert_eq!(scoped, original, "scope changed the query {query:?}");
            }
        }
    }

    #[test]
    fn scope_cannot_override_an_existing_contradictory_filter() {
        for (scope, account, query) in [
            (CorpusScope::Mine, Some("alice"), "cache owner:bob"),
            (CorpusScope::Mine, Some("alice"), "cache org:bob"),
            (CorpusScope::Public, None, "cache -is:public"),
            (CorpusScope::Public, None, "cache is:private"),
            (
                CorpusScope::Repository("alice/my-repo".into()),
                None,
                "cache repo:bob/my-repo",
            ),
        ] {
            assert!(scope.apply(query, account).is_err(), "{query}");
        }
        for query in ["cache owner:alice", "cache user:ALICE"] {
            assert!(CorpusScope::Mine.apply(query, Some("alice")).is_ok());
        }
        for query in ["cache is:public", "cache -is:private"] {
            assert!(CorpusScope::Public.apply(query, None).is_ok());
        }
        assert!(
            CorpusScope::Repository("alice/my-repo".into())
                .apply("cache agent:Alice/My-Repo", None)
                .is_ok()
        );
    }

    #[test]
    fn mine_requires_a_valid_authenticated_identity() {
        for account in [None, Some(""), Some("alice is:private"), Some(" alice ")] {
            assert!(CorpusScope::Mine.apply("cache", account).is_err());
        }
    }
    #[test]
    fn scope_applies_to_every_effective_batch_query_without_widening_shared_filters() {
        let args = W::parse_from([
            "x",
            "cache",
            "--query",
            "deploy",
            "--repo",
            "alice/demo",
            "--scope",
            "public",
        ])
        .a;
        let effective = super::effective_queries(&args).unwrap();
        let scoped = super::scoped_queries(args.scope.as_ref().unwrap(), &effective, None).unwrap();
        assert_eq!(
            scoped,
            [
                "is:public cache repo:\"alice/demo\"",
                "is:public deploy repo:\"alice/demo\""
            ]
        );
        for query in scoped {
            let parsed = Query::parse(&query);
            assert_eq!(parsed.agent.as_deref(), Some("alice/demo"));
            assert_eq!(parsed.visibility.as_deref(), Some("public"));
        }
        for argv in [
            vec![
                "x",
                "cache",
                "--query",
                "deploy is:private",
                "--scope",
                "public",
            ],
            vec!["x", "cache", "--repo", "bob/demo", "--scope", "alice/demo"],
            vec!["x", "cache", "--owner", "bob", "--scope", "mine"],
        ] {
            let args = W::parse_from(argv).a;
            assert!(
                super::scoped_queries(
                    args.scope.as_ref().unwrap(),
                    &super::effective_queries(&args).unwrap(),
                    Some("alice")
                )
                .is_err()
            );
        }
        let long = "x".repeat(super::MAX_QUERY_CHARS);
        assert!(super::scoped_queries(&CorpusScope::Public, &[long], None).is_err());
    }
}
