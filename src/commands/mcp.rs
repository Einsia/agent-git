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
                    // The description is **all** the documentation the model sees: it does
                    // not read --help, and there is nowhere else to learn the qualifiers. So
                    // the syntax lives here, together with the test for which results can be
                    // trusted — an `in:tool` hit is someone who actually ran it, a
                    // `secondhand` one is what a compact summary paraphrased, and the two
                    // differ by an order of magnitude in evidential strength.
                    {"name": "search", "description": "Search the corpus you can access for \"has anyone done this before\". Qualifiers narrow by WHERE the match landed, which is the point: in:prompt (someone asked), in:reply, in:tool (someone actually ran it), in:output (what a tool printed), in:edit, in:summary. Also owner:, agent:, runtime:, tool:, path:, turns:>20, \"quoted phrases\", -exclude. Hits carry scope and secondhand — a secondhand hit comes from a compact summary, so a summariser wrote it and nobody said it; open the session before relying on it. type defaults to sessions, which is where the work is.", "inputSchema": {"type":"object","properties":{"query":{"type":"string","description":"query with optional qualifiers"},"type":{"type":"string","enum":["sessions","agents","prs","people"],"description":"defaults to sessions"},"sort":{"type":"string","enum":["best","recent","turns"]},"limit":{"type":"integer"}},"required":["query"]}},
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
                    "content": [{"type": "text", "text": out}],
                }),
            ))
        }
        _ => Some(result(id, serde_json::json!({}))),
    }
}

fn result(id: Option<serde_json::Value>, r: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string()
}

/// A tool is the stdout of the matching CLI subcommand.
fn call_tool(name: &str, args: &serde_json::Value) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| "agit".into());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--no-color");
    match name {
        "search" => {
            cmd.arg("search");
            if let Some(q) = args.get("query").and_then(|v| v.as_str()) {
                cmd.arg(q);
            }
            if let Some(t) = args.get("type").and_then(|v| v.as_str()) {
                cmd.args(["--type", t]);
            }
            if let Some(s) = args.get("sort").and_then(|v| v.as_str()) {
                cmd.args(["--sort", s]);
            }
            if let Some(n) = args.get("limit").and_then(|v| v.as_u64()) {
                cmd.args(["-n", &n.to_string()]);
            }
            // `--mcp` is not optional: without it the model gets the layout meant for people
            // (tree glyphs, hint lines, the "this command is also exposed over MCP" sentence)
            // and has to guess the fields out of it. In the JSON form scope / secondhand / line
            // are structured.
            cmd.arg("--mcp");
        }
        "show" => {
            cmd.arg("show");
            if let Some(r) = args.get("ref").and_then(|v| v.as_str()) {
                cmd.arg(r);
            }
        }
        "view" => {
            cmd.arg("view");
            if let Some(r) = args.get("ref").and_then(|v| v.as_str()) {
                cmd.arg(r);
            }
            cmd.arg("--json");
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
        other => return format!("unknown tool {other}"),
    }
    match cmd.output() {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            if o.status.success() {
                mcp_result(name, &stdout)
            } else {
                format!(
                    "(exit {})\n{}{}",
                    o.status.code().unwrap_or(-1),
                    stdout,
                    stderr
                )
            }
        }
        Err(e) => format!("could not run the tool: {e}"),
    }
}

/// `view --json` is now wrapped in the common CLI envelope.  MCP predates the
/// envelope and its tool contract is the VIEW value itself, so unwrap only a
/// successful JSON result here.  Other commands (and failures) keep their
/// stdout unchanged.
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
