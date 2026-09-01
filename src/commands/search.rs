//! `agit search` — search the corpus you can read for "has anyone done this before".
//!
//! # The main entry point is MCP, not the terminal
//!
//! The moment it is really needed is the moment an **agent is stuck**, not the moment a person
//! opens a terminal. Alice's agent gets stuck configuring the CI cache; rather than ask Alice, it
//! searches "has anyone in this company handled the build cache of this monorepo", finds a session
//! from another team three months back, reads it, and carries on working.
//!
//! So the terminal form of this command mainly serves to check search quality by hand; `--mcp`
//! emits the JSON the MCP tool consumes directly.
//!
//! # This is the only part with a network effect
//!
//! Handoff is worth a linear amount (each further team is one further share; teams do not compound
//! each other); search is worth a superlinear amount (a larger corpus hits more often, and every
//! use enlarges the corpus). This is the real reason "public is free" — public sessions are the
//! fuel for search, and free storage buys a corpus nobody else can copy.
//!
//! # Three things that must be right
//!
//! 1. **Deduplication**: several people have hit the same trap, so return the most complete one
//!    rather than all of them. Two levels: a hit is granular to the **session**, not to the event
//!    (a word recurring throughout one session still yields a single row, carrying `+N more`);
//!    above that, sessions that open by asking the same thing are collapsed by the hub into one
//!    row, carrying `group_size` and `N more like this`.
//! 2. **Quality signal**: separate "it worked" from "it was tried and did not". The hub adds
//!    `confidence` and a one-line `outcome_reason` to `outcome` (`worked`/`failed`/`unknown`); the
//!    verdict is read off the shape of the transcript (whether the same question gets asked again,
//!    whether it ends in a change that landed), **not guessed by an LLM** — a wrong label that
//!    looks grounded is worse than a blank one. `unknown` is the normal case. `scope` is still an
//!    important signal of evidence strength: "someone actually ran this command" is far stronger
//!    than "someone asked about this word", and that is why `in:` exists.
//! 3. **Permission filtering**: the scope is strictly the corpus the caller may read, and it must
//!    be a **query condition** rather than a post-filter — a post-filter lets the hit count itself
//!    leak the existence of content the caller cannot see.
//!
//! # Why the terminal form shows `unknown` and `incomplete`
//!
//! Both are cases of "what you think happened is not what happened":
//!
//! * `unknown`: the user writes `runtim:codex` (one letter off) and it is searched as an ordinary
//!   word. Unreported, the results just look inexplicable, and the user does not suspect their own
//!   syntax — they suspect search is broken.
//! * `incomplete`: the hub's scan budget ran out. Quietly handing back a truncated number reads as
//!   "it was searched, there is nothing" — and this command exists precisely to decide from that
//!   whether a piece of work has to be done over.

use super::{CmdResult, require_login};
use crate::hub::{AgentHit, PersonHit, PrHit, SearchHit, SearchPage};
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;

/// The allowed types. Matches the hub's `SearchType`.
const KINDS: &[&str] = &["sessions", "agents", "prs", "people"];

#[derive(ClapArgs)]
pub struct Args {
    /// Query. Supports qualifiers: in:prompt|reply|tool|output|edit|summary, owner:,
    /// agent:, runtime:, tool:, path:, turns:>20, "quoted phrases", -exclude
    #[arg(value_name = "query")]
    pub query: String,

    /// What to search: sessions, agents, prs, people
    #[arg(
        short = 't',
        long = "type",
        default_value = "sessions",
        value_name = "kind"
    )]
    pub kind: String,

    /// Max hits to return
    #[arg(short = 'n', long, default_value = "10", value_name = "count")]
    pub limit: usize,

    /// Page of results (1-based)
    #[arg(long, default_value = "1", value_name = "n")]
    pub page: usize,

    /// Order: best, recent, turns
    #[arg(long, value_name = "order")]
    pub sort: Option<String>,

    /// Show how many hits each type has, then exit
    #[arg(long)]
    pub counts: bool,

    /// Emit JSON consumable by the MCP tool
    #[arg(long)]
    pub mcp: bool,
}

pub fn run(args: Args) -> CmdResult {
    let client = require_login()?;

    if args.query.trim().is_empty() {
        ui::error("query must not be empty.");
        return Ok(ExitCode::Usage);
    }
    if !KINDS.contains(&args.kind.as_str()) {
        ui::error(&format!("unknown type “{}”.", args.kind));
        ui::hint(&format!("one of: {}", KINDS.join(", ")));
        return Ok(ExitCode::Usage);
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

/// The one exit for a failed request.
///
/// Say "this hub may not have the feature yet" rather than a flat failure: a self-hosted hub can
/// lag the CLI, and what the user does then (upgrade the hub) has nothing to do with a mistyped
/// query.
fn failed(e: anyhow::Error) -> CmdResult {
    ui::error(&format!("search failed: {e:#}"));
    ui::hint("if this hub is self-hosted, it may be older than this CLI");
    Ok(ExitCode::Failure)
}

fn counts(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let c = match client.search_counts(&args.query) {
        Ok(c) => c,
        Err(e) => return failed(e),
    };
    if args.mcp {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "query": args.query,
                "counts": {
                    "sessions": c.sessions,
                    "agents": c.agents,
                    "prs": c.prs,
                    "people": c.people,
                    "sessions_incomplete": c.sessions_incomplete,
                },
            }))?
        );
        return Ok(ExitCode::Ok);
    }
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
        ui::hint("session count is a lower bound — the scan hit its budget");
    }
    Ok(ExitCode::Ok)
}

/// The line carrying the query string and the total, plus the notices that qualify it. Shared by
/// every type.
fn header<T>(p: &SearchPage<T>, query: &str) {
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
            "not {}: {} — searched as plain text instead",
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
        ui::warning("not everything was read — the scan hit its budget");
        ui::hint("narrow with owner: or agent: to cover the rest");
    }
    if p.per > 0 && p.total > p.page.max(1) * p.per {
        ui::hint(&format!(
            "more on page {} (--page {})",
            p.page + 1,
            p.page + 1
        ));
    }
}

fn sessions(client: &crate::hub::Client, args: &Args) -> CmdResult {
    let p: SearchPage<SearchHit> = match client.search_page(
        "sessions",
        &args.query,
        args.sort.as_deref(),
        args.page,
        args.limit,
    ) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };

    if args.mcp {
        return mcp_sessions(&p, &args.query);
    }
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
    ui::hint("this command is also exposed to agents over MCP (--mcp shows the JSON form)");
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
    let p: SearchPage<AgentHit> = match client.search_page(
        "agents",
        &args.query,
        args.sort.as_deref(),
        args.page,
        args.limit,
    ) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if args.mcp {
        return mcp_json(&p, &args.query, |h: &AgentHit| {
            serde_json::json!({
                "slug": h.slug, "visibility": h.visibility, "category": h.category,
                "fork": h.fork, "size_bytes": h.size_bytes,
                "updated_at": h.updated_at, "url": h.url,
            })
        });
    }
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
    let p: SearchPage<PrHit> = match client.search_page(
        "prs",
        &args.query,
        args.sort.as_deref(),
        args.page,
        args.limit,
    ) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if args.mcp {
        return mcp_json(&p, &args.query, |h: &PrHit| {
            serde_json::json!({
                "number": h.number, "agent": h.agent, "title": h.title,
                "state": h.state, "source": h.source, "target_branch": h.target_branch,
                "created_by": h.created_by, "matched_in": h.matched_in, "url": h.url,
            })
        });
    }
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
    let p: SearchPage<PersonHit> = match client.search_page(
        "people",
        &args.query,
        args.sort.as_deref(),
        args.page,
        args.limit,
    ) {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    if args.mcp {
        return mcp_json(&p, &args.query, |h: &PersonHit| {
            serde_json::json!({
                "name": h.name, "kind": h.kind, "agents": h.agents, "url": h.url,
            })
        });
    }
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
    println!("nothing found for “{query}”.");
    if !p.unknown.is_empty() {
        ui::warning(&format!(
            "{} was searched as plain text, not as a filter — check the spelling",
            p.unknown.join(", ")
        ));
    }
    if p.incomplete {
        ui::warning("the scan hit its budget, so this is not a definitive “no”");
    }
    ui::hint("the corpus only covers what you can read");
    Ok(ExitCode::Ok)
}

/// The MCP shape of a session.
///
/// The field selection is for the **model**, not for a person: `scope` / `secondhand` / `turns`
/// are all there because the model judges from them whether a hit is worth trusting — "someone
/// actually ran this command" and "the word turns up in a compact summary" are completely
/// different strengths of evidence. `line` lets it go back to the raw transcript for detail (the
/// intermediate representation (IR) is a lossy projection; tool arguments and diffs are not in it).
///
/// `outcome` / `confidence` / `outcome_reason` come out together: the tag on its own is taken for a
/// truth value when it is only a heuristic read off the shape of the transcript. `failed` is **just
/// as useful** to the model — "this path was tried and did not work" lets it skip a plan outright.
fn mcp_sessions(p: &SearchPage<SearchHit>, query: &str) -> CmdResult {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "query": query,
            "type": "sessions",
            "total": p.total,
            "incomplete": p.incomplete,
            "unknown": p.unknown,
            "hits": p.items.iter().map(|h| serde_json::json!({
                "agent": h.agent,
                "session_id": h.session_id,
                "scope": h.scope,
                "secondhand": h.secondhand,
                "runtime": h.runtime,
                "tool": h.tool,
                "paths": h.paths,
                "turns": h.turns,
                "other_hits": h.other_hits,
                "line": h.line,
                "excerpt": h.excerpt,
                "url": h.url,
                // The verdict fields ship together. With `outcome` and no `outcome_reason` the
                // model can only use the tag as a truth value — and it is a heuristic. Only with
                // the reason present can the model decide for itself whether to believe it.
                "outcome": h.outcome,
                "confidence": h.confidence,
                "outcome_reason": h.outcome_reason,
                // Collapse information: `group_size > 1` means other sessions opened by asking
                // the same thing.
                "group_size": h.group_size,
                "grouped": h.grouped,
            })).collect::<Vec<_>>(),
        }))?
    );
    Ok(ExitCode::Ok)
}

fn mcp_json<T>(p: &SearchPage<T>, query: &str, f: impl Fn(&T) -> serde_json::Value) -> CmdResult {
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "query": query,
            "type": p.kind,
            "total": p.total,
            "incomplete": p.incomplete,
            "unknown": p.unknown,
            "hits": p.items.iter().map(f).collect::<Vec<_>>(),
        }))?
    );
    Ok(ExitCode::Ok)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[derive(Parser)]
    struct W {
        #[command(flatten)]
        a: super::Args,
    }

    #[test]
    fn mcp_output_is_opt_in_and_plain() {
        // The terminal form is for people, the MCP form is for agents; the two do not mix.
        let w = W::parse_from(["x", "query term"]);
        assert!(!w.a.mcp, "the default is the human-readable form");
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

    /// Qualifiers are part of the query string, not flags.
    ///
    /// `agit search "cache in:tool"`, not `agit search cache --in tool`: one query string pastes
    /// unchanged between the terminal, the web interface and MCP. As flags, every added qualifier
    /// means editing every client, and the string no longer pastes.
    #[test]
    fn qualifiers_ride_inside_the_query_string() {
        let w = W::parse_from(["x", "cache in:tool owner:alice -deprecated"]);
        assert_eq!(w.a.query, "cache in:tool owner:alice -deprecated");
    }
}
