//! `agit export` — export one branch or one range.
//!
//! * `jsonl`: raw bytes (the original log lines, envelope unwrapped);
//! * `ir`: the intermediate representation (IR) — common across harnesses, `tool_result` kept —
//!   the entry point of a data pipeline: downstream formats (training data, eval sets) are the
//!   pipeline's own to work up from ir, and agit makes no format decision for them;
//! * `markdown`: for a human to read;
//! * a registered runtime name: its native format — a file only, no runtime
//!   installed.
//!
//! `--view-only` exports only what the VIEW covers; the default exports the full log — detours
//! and failed attempts are worth as much to data and to an audit. `--redact` scans and masks
//! first.
//!
//! The split with `resume --as`: resume converts and starts working in place; export hands you a
//! file and you leave.

use super::CmdResult;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::transcript;
use crate::{ExitCode, adapter, ui};
use clap::Args as ClapArgs;

#[derive(ClapArgs)]
pub struct Args {
    /// Ref (branch / tag / #n / #a..#b / commit).
    pub target: String,
    /// Output format.
    #[arg(
        long,
        value_name = "jsonl|ir|markdown|RUNTIME",
        default_value = "jsonl"
    )]
    pub format: String,
    /// Export only what the VIEW covers (default: the full log).
    #[arg(long)]
    pub view_only: bool,
    /// Redact secrets on export.
    #[arg(long)]
    pub redact: bool,
    /// Output path (default or `-`: stdout).
    #[arg(short = 'o', long, value_name = "path")]
    pub out: Option<String>,
}

pub fn run(args: Args) -> CmdResult {
    let cwd = std::env::current_dir()?;
    let spec = match refs::parse(&args.target) {
        Ok(s) => s,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Usage);
        }
    };
    let source = super::echo::Source::for_spec(&spec);
    let spec = super::target::resolve_local_repo(spec)?;
    let spec = match super::context::substitute_at(spec) {
        Ok(spec) => spec,
        Err(e) => {
            ui::error(&format!("{e:#}"));
            return Ok(ExitCode::Ref);
        }
    };
    // Repo resolution: an explicit owner/repo, a locally unique name, context.
    let (repo, slug, resolved) = match &spec.repo {
        refs::RepoSel::Slug(o, n) => {
            let Some(r) = Repo::open(crate::infra::config::repo_dir(o, n).unwrap_or_default())
            else {
                ui::error(&format!("{o}/{n} does not exist locally."));
                ui::hint(&format!("`agit clone {o}/{n}` or `agit fetch {o}/{n}`"));
                return Ok(ExitCode::Precondition);
            };
            let Ok(res) = refs::resolve(&r, &spec) else {
                ui::error("the ref doesn’t resolve in this repo.");
                return Ok(ExitCode::Ref);
            };
            (r, format!("{o}/{n}"), res)
        }
        _ => {
            let ctx = match super::context::resolve(&cwd) {
                Ok(c) => c,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    ui::hint("or write it fully: `agit export <owner/repo>@<ref>`");
                    return Ok(ExitCode::Ref);
                }
            };
            let (o, n) = super::parse_slug(&ctx.repo)?;
            let Some(r) = Repo::open(crate::infra::config::repo_dir(&o, &n)?) else {
                ui::error(&format!("{} does not exist locally.", ctx.repo));
                return Ok(ExitCode::Precondition);
            };
            let res = match refs::resolve(&r, &spec) {
                Ok(res) => res,
                Err(e) => {
                    ui::error(&format!("{e:#}"));
                    return Ok(ExitCode::Ref);
                }
            };
            (r, ctx.repo, res)
        }
    };
    let sha = resolved.sha.clone();

    let which = if args.view_only {
        meta::VIEW_FILE
    } else {
        meta::LOG_FILE
    };
    let env_text = match required_sequence(&repo, &sha, which) {
        Ok(text) => text,
        Err(error) => {
            // `--view-only` is a security/selection boundary: a broken or missing VIEW must
            // never silently widen the export to the complete LOG.  In particular, v1
            // materialization also checks object identity, size limits, and LOG reachability;
            // swallowing any of those failures would export content the user did not select.
            ui::error(&format!("cannot read this point's {which}: {error:#}"));
            return Ok(ExitCode::Precondition);
        }
    };
    let selected = if let Some((a, b)) = spec_turn_range(&spec) {
        crop_to_turns(&env_text, a, b)?
    } else {
        env_text
    };
    let result = match args.format.as_str() {
        "jsonl" => Ok(transcript::unwrap_lossy(&selected).0),
        "ir" => to_ir(&selected),
        "markdown" => to_markdown(&selected),
        runtime if adapter::get(runtime).is_ok() => to_native(&selected, runtime),
        other => {
            ui::error(&format!("unknown format `{other}`."));
            ui::hint(&format!(
                "jsonl | ir | markdown | {}",
                adapter::RUNTIMES.join(" | ")
            ));
            return Ok(ExitCode::Usage);
        }
    };
    let mut out = match result {
        Ok(content) => content,
        Err(error) => {
            ui::error(&format!("cannot export to {}: {error:#}", args.format));
            return Ok(ExitCode::Precondition);
        }
    };

    if args.redact {
        // Redaction is `domain::redact`: per-hit placeholder substitution (secrets) plus
        // persona / path / IP masking. `secrets::redact` is not it — that computes one key mask
        // over the **entire export** (keep the first four and the last two characters, star the
        // rest), and the whole content is written off on the spot.
        let rep = crate::domain::redact::Redactor::try_this_machine()?.scrub(&out);
        out = rep.text;
        // The counts answer "is this export safe", so they must be visible whether the output
        // goes to -o or to stdout.
        eprintln!(
            "{}",
            ui::dim(&format!(
                "  redacted: {} secrets, {} path/host hits, {} public IPs",
                rep.secrets, rep.paths, rep.ips
            ))
        );
    }

    match &args.out {
        Some(p) if p != "-" => {
            if let Err(error) = std::fs::write(p, &out) {
                ui::error(&format!("cannot write export to {p}: {error}"));
                return Ok(ExitCode::Precondition);
            }
            let selected_ref = if spec.tail == refs::Tail::None {
                resolved.branch.as_deref().unwrap_or(&sha)
            } else {
                &sha
            };
            super::echo::emit(
                "export",
                &[super::echo::Selection::new(
                    format!("{slug}@{selected_ref}"),
                    source,
                )],
            );
            ui::success(&format!("exported to {p} ({} bytes)", out.len()));
        }
        _ => {
            if let Err(error) = write_output(&mut std::io::stdout().lock(), out.as_bytes()) {
                ui::error(&format!("cannot write export to stdout: {error}"));
                return Ok(ExitCode::Precondition);
            }
        }
    }
    Ok(ExitCode::Ok)
}

fn write_output(writer: &mut impl std::io::Write, bytes: &[u8]) -> std::io::Result<()> {
    writer.write_all(bytes)?;
    writer.flush()
}

fn to_native(envelopes: &str, runtime: &str) -> crate::Result<String> {
    // Native exports form replayable call/result pairs; jsonl preserves the raw native records.
    let (session, details) = transcript::display::parse_with_details(envelopes)?;
    adapter::get(runtime)?.render_with(&session, "export", std::path::Path::new("."), &details)
}

fn required_sequence(repo: &Repo, sha: &str, which: &str) -> crate::Result<String> {
    repo.show_result(sha, which)?
        .ok_or_else(|| anyhow::anyhow!("this point has no {which}"))
}

fn spec_turn_range(spec: &refs::RefSpec) -> Option<(u32, u32)> {
    match spec.tail {
        refs::Tail::Range { a, b } if a != refs::LAST_TURN && b != refs::LAST_TURN => Some((a, b)),
        refs::Tail::Turn(n) if n != refs::LAST_TURN => Some((n, n)),
        _ => None,
    }
}

/// Crop to a turn range: parse into IR → rebuild the bytes segment by segment.
fn crop_to_turns(envelopes: &str, a: u32, b: u32) -> crate::Result<String> {
    let ir = transcript::display::parse(envelopes)?;
    let groups = crate::domain::turn::groups_of(&ir);
    let lines: Vec<&str> = envelopes.lines().collect();
    let mut take: Vec<usize> = vec![];
    for (i, g) in groups.iter().enumerate() {
        let n = (i + 1) as u32;
        if n < a || n > b {
            continue;
        }
        for &j in g {
            if let Some(l) = ir.events[j].line {
                take.push(l);
            }
        }
    }
    take.sort_unstable();
    take.dedup();
    let mut out = String::new();
    for i in take {
        if let Some(l) = lines.get(i) {
            out.push_str(l);
            out.push('\n');
        }
    }
    Ok(out)
}

/// The ir form: one `{agit-ir, kind, text, timestamp, tool, paths}` per line.
fn to_ir(envelopes: &str) -> crate::Result<String> {
    let ir = transcript::display::parse(envelopes)?;
    let mut out = String::new();
    for e in &ir.events {
        let line = serde_json::json!({
            "agit-ir": true,
            "kind": format!("{:?}", e.kind),
            "text": e.text,
            "timestamp": e.timestamp,
            "tool": e.tool,
            "paths": e.paths,
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    Ok(out)
}

/// An interjection renders as one whole blockquote, every line carrying `> `. Otherwise only the
/// first line of a multi-line original stays quoted, the rest fall back to top level, and a `##`
/// the user wrote grows into a heading of the exported document itself.
fn interjection_block(text: &str) -> String {
    let mut out = String::from("\n> 🧑 *(mid-turn)*\n>\n");
    for line in text.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn to_markdown(envelopes: &str) -> crate::Result<String> {
    let ir = transcript::display::parse(envelopes)?;
    let mut out = String::from("# session export\n\n");
    for e in &ir.events {
        match e.kind {
            adapter::EventKind::UserPrompt => {
                out.push_str(&format!(
                    "\n## 🧑 {}\n\n{}\n",
                    e.timestamp.as_deref().unwrap_or(""),
                    e.text.as_deref().unwrap_or("")
                ));
            }
            adapter::EventKind::UserInterjection => {
                out.push_str(&interjection_block(e.text.as_deref().unwrap_or("")));
            }
            adapter::EventKind::AssistantReply => {
                out.push_str(&format!("\n**🤖** {}\n", e.text.as_deref().unwrap_or("")));
            }
            adapter::EventKind::ToolUse => {
                out.push_str(&format!(
                    "\n- 🔧 `{}`\n",
                    e.tool.as_deref().unwrap_or("tool")
                ));
            }
            adapter::EventKind::CompactSummary => {
                out.push_str("\n> *(compact boundary)*\n");
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    fn render_native(raw: &str, from: &str, target: &str) -> crate::Result<String> {
        let saved =
            crate::domain::transcript::wrap_lines(raw, from, &format!("agit-{}", "a".repeat(40)));
        crate::domain::transcript::display::render_native(
            &saved,
            target,
            "export",
            std::path::Path::new("."),
        )
        .map(|(content, _)| content)
    }
    #[test]
    fn registered_native_exports_preserve_tool_history_and_report_invalid_sources() {
        let raw = concat!(
            "{\"type\":\"user\",\"sessionId\":\"source\",\"message\":{\"role\":\"user\",\"content\":\"Read the file\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"Read\",\"input\":{\"file_path\":\"probe.txt\"}}]}}\n",
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"call\",\"content\":\"TOOL_OUTPUT\"}]}}\n"
        );
        for runtime in ["openclaw", "hermes", "workbuddy"] {
            let exported = render_native(raw, "claude-code", runtime).unwrap();
            let adapter = crate::adapter::get(runtime).unwrap();
            let parsed = adapter.parse(&exported).unwrap();
            assert!(
                parsed
                    .events
                    .iter()
                    .any(|event| event.text.as_deref() == Some("TOOL_OUTPUT"))
            );
            assert!(exported.contains("probe.txt"));
            assert_eq!(
                render_native(&exported, runtime, runtime).unwrap(),
                exported
            );
            assert!(render_native("invalid", "hermes", runtime).is_err());
        }
    }

    #[test]
    fn output_flush_failure_is_not_a_successful_delivery() {
        #[derive(Default)]
        struct FlushFailure {
            bytes: Vec<u8>,
            flushes: usize,
        }
        impl std::io::Write for FlushFailure {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                self.flushes += 1;
                Err(std::io::Error::other("synthetic output flush failure"))
            }
        }
        let mut writer = FlushFailure::default();
        let error = super::write_output(&mut writer, b"synthetic output").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(writer.bytes, b"synthetic output");
        assert_eq!(writer.flushes, 1);
    }

    use super::*;
    use crate::domain::meta::Meta;

    #[test]
    fn view_only_read_never_falls_back_to_a_valid_log() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repo::init(&dir.path().join("r")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let claim = format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN));
        let raw = super::super::merge::envelope_line(
            &serde_json::json!({"type":"user","message":{"content":"private log"}}).to_string(),
            "codex",
            &claim,
        );
        let envelope: crate::domain::transcript::Envelope = serde_json::from_str(&raw).unwrap();
        let line = crate::domain::storage::envelope_line(&envelope);
        crate::domain::storage::write_snapshot(repo.root(), &line, &line).unwrap();
        meta::write(repo.root(), &Meta::new(claim, "codex".into(), "/w".into())).unwrap();
        repo.add_all().unwrap();
        repo.commit("valid snapshot").unwrap();

        // Keep LOG valid but make VIEW reference an event that LOG does not reach.
        std::fs::write(
            repo.root().join(meta::VIEW_FILE),
            format!("{}\n", "0".repeat(40)),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("broken VIEW").unwrap();
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap();

        assert!(required_sequence(&repo, head.trim(), meta::LOG_FILE).is_ok());
        let error = required_sequence(&repo, head.trim(), meta::VIEW_FILE).unwrap_err();
        assert!(error.to_string().contains("not reachable"));
    }

    /// Export takes this line's transcript, even when the tree also holds a root `LOG` / `VIEW`
    /// user file of the same name.
    ///
    /// v0 does not reserve those two names, so a user file coexisting with `session/log.jsonl` is
    /// legal history. Once a same-named entry wins the logical read, `agit export` exports that
    /// ordinary text — the user gets a file that is not a transcript and on which `--view-only`
    /// means nothing, with no error anywhere along the way.
    #[test]
    fn export_never_takes_a_same_named_user_file_for_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, head, transcript) =
            crate::domain::repo::v0_repo_with_shadowing_user_files(&dir.path().join("r"));

        for which in [meta::LOG_FILE, meta::VIEW_FILE] {
            let text = required_sequence(&repo, &head, which).unwrap();
            assert_eq!(
                text, transcript,
                "export's {which} must be the transcript, not the same-named user file"
            );
        }

        // Unwrapped, it is that session's original text; not one byte of the user file got in.
        let (raw, _skipped) = crate::domain::transcript::unwrap_lossy(&transcript);
        assert!(raw.contains("hello"), "{raw}");
        assert!(
            !raw.contains(crate::domain::repo::V0_SHADOWING_USER_LOG.trim_end()),
            "{raw}"
        );
    }

    /// Every line of a multi-line interjection stays inside the blockquote, including blank
    /// lines and a `##` that would otherwise read as a heading.
    #[test]
    fn interjection_block_quotes_every_line() {
        let md = super::interjection_block("first line\n\n## not a heading\nthird line");
        for line in md.lines().skip(1) {
            assert!(line.starts_with('>'), "{md}");
        }
        assert!(md.contains("> ## not a heading\n"), "{md}");
        assert!(
            md.contains("> \n"),
            "a blank line must stay inside the quote: {md}"
        );
        assert!(!md.contains("\n## "), "{md}");
    }
}
