//! `agit view` — read-only plumbing: print the ordered composition of the VIEW at a point.
//!
//! Output columns: index, event handle (log line number), kind, byte count, source label
//! (this branch / merged-from / compact-summary / merge_summary), excerpt.
//!
//! It is the `merge agent`'s first reconnaissance command; `--json` is its interface to scripts.
//! **The VIEW has no manual edit** — every change lands as a commit through
//! merge / cherry-pick / revert.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use serde::Serialize;

#[derive(ClapArgs)]
pub struct Args {
    /// Target `owner/repo@ref` or local ref (default: the context branch).
    #[arg(value_name = "owner/repo@ref | ref")]
    pub target: Option<String>,
    /// Machine-readable output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Item {
    index: usize,
    /// Line number of this full-envelope event occurrence in the LOG (0-based).
    log_index: Option<usize>,
    kind: String,
    bytes: usize,
    /// this branch | merged-from:<session prefix> | compact-summary | merge_summary | marker:<name>
    source: String,
    excerpt: String,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let parsed_target = match args.target.as_deref() {
        Some(raw) => match crate::commands::target::parse(raw) {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Usage);
            }
        },
        None => None,
    };
    let (repo, default_ref) = if let Some(target) = &parsed_target {
        if let Some(slug) = &target.repo {
            let (o, n) = super::parse_slug(slug)?;
            let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
                ui::error(&format!("{slug} doesn’t exist locally."));
                return Ok(ExitCode::Precondition);
            };
            (repo, None)
        } else {
            let ctx = match super::context::resolve(&cwd) {
                Ok(c) => c,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Ref);
                }
            };
            let (o, n) = super::parse_slug(&ctx.repo)?;
            let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
                ui::error(&format!("{} does not exist locally.", ctx.repo));
                return Ok(ExitCode::Precondition);
            };
            (repo, Some(ctx.branch))
        }
    } else {
        let ctx = match super::context::resolve(&cwd) {
            Ok(c) => c,
            Err(e) => {
                ui::error(&format!("{e:#}"));
                return Ok(ExitCode::Ref);
            }
        };
        let (o, n) = super::parse_slug(&ctx.repo)?;
        let Some(repo) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
            ui::error(&format!("{} does not exist locally.", ctx.repo));
            return Ok(ExitCode::Precondition);
        };
        (repo, Some(ctx.branch))
    };
    let spec = match parsed_target {
        Some(target) => crate::commands::target::to_spec(target),
        None => refs::parse(default_ref.as_deref().expect("context has a branch"))
            .expect("branch names are always valid refs"),
    };
    let spec = match super::context::substitute_at(spec) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    let resolved = match refs::resolve(&repo, &spec) {
        Ok(r) => r,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    };

    let Some(snap) = meta::read_at_ref(&repo, &resolved.sha) else {
        ui::error(&format!(
            "this point carries no {} — its line was never declared.",
            meta::FILE
        ));
        ui::hint("re-fetch this checkout: `agit fetch` (or `agit clone` again)");
        return Ok(ExitCode::Precondition);
    };
    if snap.is_file_line() {
        ui::error("file lines have no VIEW.");
        ui::hint("shared files: `agit show <ref>:<path>`; history: `agit log`");
        return Ok(ExitCode::Precondition);
    }
    let (log, view) = storage::materialize_pair_at(repo.root(), &resolved.sha)?;

    // LOG line-number index: full-envelope event id → the line number of each occurrence. The
    // content-only `_object_hash` cannot separate identical content coming from different
    // sources/sessions, and `position()` alone makes every occurrence of a repeated event point
    // at the first line.
    let mut log_positions = log_occurrence_positions(&log)?;

    let items: Vec<Item> = view
        .split_inclusive('\n')
        .enumerate()
        .map(|(i, line)| {
            let log_index = take_log_occurrence(&mut log_positions, line)?;
            Ok(item_of(i, line, &snap.session, log_index))
        })
        .collect::<crate::Result<_>>()?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(ExitCode::Ok);
    }

    println!(
        "{}",
        ui::dim(&format!(
            "  VIEW @ {} ({} events)",
            args.target
                .as_deref()
                .or(default_ref.as_deref())
                .unwrap_or("@"),
            items.len()
        ))
    );
    for it in &items {
        let coord = it
            .log_index
            .map(|i| format!("log#{i}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "  {:>4} {:>8} {:<10} {:>7}B {:<22} {}",
            it.index, coord, it.kind, it.bytes, it.source, it.excerpt
        );
    }
    Ok(ExitCode::Ok)
}

fn log_occurrence_positions(
    log: &str,
) -> crate::Result<std::collections::HashMap<String, std::collections::VecDeque<usize>>> {
    let mut positions =
        std::collections::HashMap::<String, std::collections::VecDeque<usize>>::new();
    for (index, line) in log.split_inclusive('\n').enumerate() {
        let id = storage::event_id(line)?;
        positions.entry(id).or_default().push_back(index);
    }
    Ok(positions)
}

fn take_log_occurrence(
    positions: &mut std::collections::HashMap<String, std::collections::VecDeque<usize>>,
    line: &str,
) -> crate::Result<Option<usize>> {
    let id = storage::event_id(line)?;
    Ok(positions
        .get_mut(&id)
        .and_then(std::collections::VecDeque::pop_front))
}

fn item_of(i: usize, line: &str, claim: &str, log_index: Option<usize>) -> Item {
    let Ok(env) = storage::parse_envelope_line(line) else {
        return Item {
            index: i,
            log_index: None,
            kind: "corrupt".into(),
            bytes: line.len(),
            source: "-".into(),
            excerpt: "(not a valid envelope)".into(),
        };
    };
    let content = &env.content;
    let (kind, source, excerpt) = classify(content, &env.session_id, claim);
    Item {
        index: i,
        log_index,
        kind,
        bytes: line.len(),
        source,
        excerpt,
    }
}

fn classify(
    content: &serde_json::Value,
    session_id: &str,
    claim: &str,
) -> (String, String, String) {
    // Synthetic markers.
    if content
        .get("subtype")
        .and_then(|s| s.as_str())
        .is_some_and(|s| s.starts_with("agit:"))
    {
        let name = content["subtype"]
            .as_str()
            .unwrap()
            .trim_start_matches("agit:");
        return ("marker".into(), format!("marker:{name}"), String::new());
    }
    if content.get("agit").and_then(|s| s.as_str()) == Some("merge_summary") {
        let text = content
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        return (
            "merge_summary".into(),
            "merge_summary".into(),
            ui::truncate(text, 60),
        );
    }
    let t = content
        .get("type")
        .and_then(|s| s.as_str())
        .unwrap_or("?")
        .to_string();
    let source = if session_id == claim {
        "this branch".to_owned()
    } else {
        format!("merged-from:{}", &session_id[..13.min(session_id.len())])
    };
    let excerpt = content
        .pointer("/message/content")
        .and_then(|c| c.as_str())
        .or_else(|| {
            content
                .pointer("/payload/content/0/text")
                .and_then(|c| c.as_str())
        })
        .map(|t| ui::truncate(t, 60))
        .unwrap_or_default();
    let compact = content
        .get("isCompactSummary")
        .and_then(|c| c.as_bool())
        .unwrap_or(false)
        || t == "compacted";
    (
        if compact { "compact-summary".into() } else { t },
        if compact {
            "compact-summary".into()
        } else {
            source
        },
        excerpt,
    )
}

#[cfg(test)]
mod tests {
    use crate::domain::{storage, transcript};

    #[test]
    fn classifies_markers() {
        let v = serde_json::json!({"type":"system","subtype":"agit:__merge_start__"});
        assert_eq!(super::classify(&v, "x", "x").0, "marker");
    }

    #[test]
    fn log_coordinates_distinguish_equal_content_from_different_sessions() {
        let content = serde_json::json!({"type":"user","message":{"content":"same"}});
        let line = |session: &str| {
            storage::envelope_line(&transcript::Envelope {
                source: "codex".into(),
                session_id: session.into(),
                object_hash: transcript::object_hash(&content),
                content: content.clone(),
            })
        };
        let first = line(&format!("agit-{}", "a".repeat(40)));
        let second = line(&format!("agit-{}", "b".repeat(40)));
        assert_eq!(super::item_of(0, &second, "", Some(1)).log_index, Some(1));
        assert_ne!(
            storage::event_id(&first).unwrap(),
            storage::event_id(&second).unwrap()
        );
    }

    #[test]
    fn repeated_events_keep_distinct_log_coordinates() {
        let session = format!("agit-{}", "a".repeat(40));
        let content = serde_json::json!({"type":"user","message":{"content":"same"}});
        let line = storage::envelope_line(&transcript::Envelope {
            source: "codex".into(),
            session_id: session.clone(),
            object_hash: transcript::object_hash(&content),
            content,
        });
        let log = format!(
            "{line}{}{line}",
            storage::envelope_line(&transcript::Envelope {
                source: "codex".into(),
                session_id: session,
                object_hash: transcript::object_hash(&serde_json::json!({"type":"assistant"})),
                content: serde_json::json!({"type":"assistant"}),
            })
        );
        let mut positions = super::log_occurrence_positions(&log).unwrap();
        assert_eq!(
            super::take_log_occurrence(&mut positions, &line).unwrap(),
            Some(0)
        );
        assert_eq!(
            super::take_log_occurrence(&mut positions, &line).unwrap(),
            Some(2)
        );
    }
}
