//! `agit mcp` — the stdio MCP server (hidden subcommand).
//!
//! Exposes search / show / view / status / commit (PRD: "the main entry point for search is MCP,
//! not the terminal" — a stuck agent first searches for "has anyone handled this").
//!
//! The implementation is deliberately plain: a tool call starts an `agit <command>` subprocess
//! and wraps its stdout in the MCP response. Tool semantics and the CLI therefore always agree;
//! there is no duplicate implementation to drift.

use super::CmdResult;
use crate::ExitCode;
use clap::Args as ClapArgs;
use std::io::{BufRead as _, Write as _};

#[derive(ClapArgs)]
pub struct Args {}

pub fn run(_args: Args) -> CmdResult {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let Some(resp) = handle(&req) else { continue };
        let mut out = stdout.lock();
        let _ = writeln!(out, "{resp}");
        let _ = out.flush();
    }
    Ok(ExitCode::Ok)
}

fn handle(req: &serde_json::Value) -> Option<String> {
    let id = req.get("id").cloned();
    let method = req.get("method")?.as_str()?;
    match method {
        "initialize" => Some(result(
            id,
            serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "agit", "version": env!("CARGO_PKG_VERSION")},
            }),
        )),
        "notifications/initialized" | "ping" => {
            if method == "ping" {
                Some(result(id, serde_json::json!({})))
            } else {
                None
            }
        }
        "tools/list" => Some(result(
            id,
            serde_json::json!({
                "tools": [
                    {"name": "search", "description": "Search readable AgentGit history. Use query for one search, or queries for an ordered batch (up to 16, four in flight). Shared filters: repo (owner/name), owner, author (saved Git author name/email), since (inclusive UTC saved time), before (exclusive UTC saved time), runtime, scopes (prompt/reply/tool/output/edit/summary), tool, path. Queries also accept quoted phrases, -exclude and qualifiers such as turns:>20. Inspect incomplete and unknown before concluding no work exists. Scope identifies the evidence; secondhand means a compact summary. Outcome/confidence are heuristics: open a hit before relying on it. Pagination includes page, per and has_more. scope restricts sessions or agents to mine (personally owned repositories), public, or one owner/repo; every remote search requires login.", "inputSchema": {"type":"object","properties":{"query":{"type":"string"},"queries":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":16},"type":{"type":"string","enum":["sessions","agents","prs","people"]},"sort":{"type":"string","enum":["best","recent","turns"]},"limit":{"type":"integer","minimum":1,"maximum":100},"page":{"type":"integer","minimum":1},"scope":{"type":"string","description":"mine, public, or owner/repo; sessions and agents only"},"repo":{"type":"string"},"owner":{"type":"string"},"author":{"type":"string"},"since":{"type":"string"},"before":{"type":"string"},"runtime":{"type":"string"},"scopes":{"type":"array","items":{"type":"string","enum":["prompt","reply","tool","output","edit","summary"]}},"tool":{"type":"string"},"path":{"type":"string"}},"additionalProperties":false}},
                    {"name": "show", "description": "Read part of a session (ref, ref#n, ref#n.k)", "inputSchema": {"type":"object","properties":{"ref":{"type":"string"}}}},
                    {"name": "view", "description": "the ordered composition of a VIEW (plumbing)", "inputSchema": {"type":"object","properties":{"ref":{"type":"string"}}}},
                    {"name": "status", "description": "who am I + sync status", "inputSchema": {"type":"object","properties":{}}},
                    {"name": "commit", "description": "settle the current session", "inputSchema": {"type":"object","properties":{"milestone":{"type":"string"}}}},
                    {"name": "rc_status", "description": "Is this machine connected to a hub, and what sessions is the daemon supervising? Use it to find out whether you are being watched remotely.", "inputSchema": {"type":"object","properties":{}}},
                    {"name": "rc_list", "description": "The machines paired to this account (including offline ones)", "inputSchema": {"type":"object","properties":{}}},
                ]
            }),
        )),
        "tools/call" => {
            let name = req.pointer("/params/name")?.as_str()?.to_string();
            let args = req
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_default();
            let out = call_tool(&name, &args);
            Some(result(
                id,
                serde_json::json!({
                    "content": [{"type": "text", "text": out.text}],
                    "isError": out.is_error,
                }),
            ))
        }
        _ => Some(result(id, serde_json::json!({}))),
    }
}

fn result(id: Option<serde_json::Value>, r: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string()
}

struct ToolOutput {
    text: String,
    is_error: bool,
}

impl ToolOutput {
    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: Option<String>,
    #[serde(default)]
    queries: Vec<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    sort: Option<String>,
    limit: Option<usize>,
    page: Option<usize>,
    scope: Option<String>,
    repo: Option<String>,
    owner: Option<String>,
    author: Option<String>,
    since: Option<String>,
    before: Option<String>,
    runtime: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    tool: Option<String>,
    path: Option<String>,
}

fn search_arguments(args: &serde_json::Value) -> Result<Vec<String>, String> {
    if let Some(scope) = args.get("scope")
        && !scope.is_string()
    {
        return Err("invalid search scope: expected mine, public, or an owner/repo string".into());
    }
    let args: SearchArgs = serde_json::from_value(args.clone()).map_err(|e| e.to_string())?;
    let mut out = vec!["search".to_owned(), "--mcp".to_owned()];
    for query in args.query.into_iter().chain(args.queries) {
        out.extend(["--query".to_owned(), query]);
    }
    for (flag, value) in [
        ("--type", args.kind),
        ("--sort", args.sort),
        ("--scope", args.scope),
        ("--repo", args.repo),
        ("--owner", args.owner),
        ("--author", args.author),
        ("--since", args.since),
        ("--before", args.before),
        ("--runtime", args.runtime),
        ("--tool", args.tool),
        ("--path", args.path),
        ("--limit", args.limit.map(|n| n.to_string())),
        ("--page", args.page.map(|n| n.to_string())),
    ] {
        if let Some(value) = value {
            out.extend([flag.to_owned(), value]);
        }
    }
    for scope in args.scopes {
        out.extend(["--in".to_owned(), scope]);
    }
    Ok(out)
}

/// Subprocess stdin must be closed so a CLI prompt cannot consume the MCP request stream.
fn call_tool(name: &str, args: &serde_json::Value) -> ToolOutput {
    let exe = std::env::current_exe().unwrap_or_else(|_| "agit".into());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--no-color").stdin(std::process::Stdio::null());
    if !matches!(name, "search" | "show") {
        cmd.arg("--json");
    }
    // A tool's stdout is its result payload, independent of human presentation.
    cmd.env("AGIT_PROTOCOL_CHILD", "1").env_remove("AGIT_QUIET");
    match name {
        "search" => match search_arguments(args) {
            Ok(arguments) => {
                cmd.args(arguments);
            }
            Err(error) => return ToolOutput::error(format!("invalid search arguments: {error}")),
        },
        "show" => {
            cmd.arg("show");
            if let Some(r) = args.get("ref").and_then(|v| v.as_str()) {
                cmd.arg("--").arg(r);
            }
        }
        "view" => {
            cmd.arg("view");
            if let Some(r) = args.get("ref").and_then(|v| v.as_str()) {
                cmd.arg("--").arg(r);
            }
        }
        "status" => {
            cmd.arg("status");
        }
        "commit" => {
            cmd.arg("commit");
            if let Some(m) = args.get("milestone").and_then(|v| v.as_str()) {
                cmd.args(["--milestone", m]);
            }
        }
        "rc_status" => {
            cmd.args(["rc", "status"]);
        }
        "rc_list" => {
            cmd.args(["rc", "list"]);
        }
        other => return ToolOutput::error(format!("unknown tool {other}")),
    }
    match cmd.output() {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            if o.status.success() {
                ToolOutput {
                    text: mcp_result(name, &stdout),
                    is_error: false,
                }
            } else {
                if serde_json::from_str::<serde_json::Value>(&stdout).is_ok() {
                    return ToolOutput::error(stdout.into_owned());
                }
                ToolOutput::error(format!(
                    "(exit {})\n{}{}",
                    o.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                ))
            }
        }
        Err(e) => ToolOutput::error(format!("could not run the tool: {e}")),
    }
}

/// The VIEW tool returns its structured value directly; the CLI envelope is transport.
/// Other tools and failed operations retain their own result contracts.
fn mcp_result(name: &str, stdout: &str) -> String {
    if name != "view" {
        return stdout.to_owned();
    }
    let Ok(envelope) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return stdout.to_owned();
    };
    if envelope.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        && envelope
            .pointer("/result/format")
            .and_then(serde_json::Value::as_str)
            == Some("json")
        && let Some(value) = envelope.pointer("/result/value")
    {
        return value.to_string();
    }
    stdout.to_owned()
}

#[cfg(test)]
mod workspace_tool_tests {
    use super::mcp_result;

    #[test]
    fn view_tool_unwraps_the_cli_envelope() {
        let envelope =
            r#"{"schema":"cli-output","ok":true,"result":{"format":"json","value":[{"index":1}]}}"#;
        assert_eq!(mcp_result("view", envelope), r#"[{"index":1}]"#);
    }

    #[test]
    fn non_view_tools_and_failures_are_not_unwrapped() {
        let envelope = r#"{"schema":"cli-output","ok":false,"result":{"format":"empty"}}"#;
        assert_eq!(mcp_result("view", envelope), envelope);
        assert_eq!(mcp_result("status", envelope), envelope);
        assert_eq!(mcp_result("view", "plain output\n"), "plain output\n");
    }

    #[test]
    fn search_batch_arguments_are_literal_and_typed() {
        let arguments = super::search_arguments(&serde_json::json!({
            "queries": ["--counts", "cache"], "repo":"alice/demo", "author":"Bob", "since":"2026-09-01", "page":2,
            "scopes":["tool", "output"], "scope":"public", "limit":5,
        }))
        .unwrap();
        assert_eq!(
            &arguments[..6],
            ["search", "--mcp", "--query", "--counts", "--query", "cache"]
        );
        use clap::Parser;
        let command = super::super::Cli::try_parse_from(
            std::iter::once("agit").chain(arguments.iter().map(String::as_str)),
        )
        .unwrap();
        let Some(super::super::Commands::Search(args)) = command.command else {
            panic!("search command expected")
        };
        assert!(!args.counts);
        assert_eq!(args.queries, ["--counts", "cache"]);
        assert_eq!(args.repo.as_deref(), Some("alice/demo"));
        assert_eq!(args.scope, Some(super::super::search::CorpusScope::Public));
        assert!(super::search_arguments(&serde_json::json!({"limit":-1})).is_err());
        assert!(super::search_arguments(&serde_json::json!({"queries":[false]})).is_err());
        assert!(super::search_arguments(&serde_json::json!({"author_typo":"alice"})).is_err());
    }

    #[test]
    fn tool_failures_are_marked_as_errors() {
        let response = super::handle(&serde_json::json!({"id":1, "method":"tools/call",
            "params":{"name":"search", "arguments":{"limit":-1}}}))
        .unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("invalid search arguments")
        );
    }

    /// MCP exposes only the **read** side of the workspace tools.
    ///
    /// Binding a directory widens this machine's allowlist, and inviting a member hands access
    /// to a real machine to someone else — both must be clicked by a human in the web
    /// interface. Making them tools an agent can call itself turns "a compromised agent" and "a
    /// person with permission" into the same thing.
    #[test]
    fn no_write_side_workspace_tool_is_exposed() {
        let src = include_str!("mcp.rs");
        for forbidden in [
            "\"project_bind\"",
            "\"workspace_create\"",
            "\"member_add\"",
            "\"terminal_open\"",
            "\"rc_revoke\"",
        ] {
            assert!(
                !src.contains(forbidden),
                "{forbidden} is a write-side operation and must not appear in the MCP tool table"
            );
        }
        assert!(src.contains("\"rc_status\"") && src.contains("\"rc_list\""));
    }
}
