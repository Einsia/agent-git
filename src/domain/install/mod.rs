//! Install a session into a runtime so it can be resumed.
//!
//! # The copy in its native format is rewritten byte by byte, never routed through the IR
//!
//! `parse → render` loses content, **within one runtime too**: both renderers `continue` on
//! `EventKind::Other` (encrypted reasoning, vendor-proprietary tool encodings), and Claude Code
//! additionally drops compact boundaries.
//!
//! So installing back into its own runtime rewrites line by line, and what may move is a
//! **closed set** (see [`localize_same_format`]): identity keys, placeholders for open calls, the
//! Codex bootstrap, and visible-event localization — the last two are deliberate conversions: the
//! writing machine's paginated store does not sync with the repo, and unlocalized this file does
//! not open on this machine. Outside the closed set the semantics are unchanged (lines are
//! rewritten through canonical serialization; key order and whitespace are not preserved).
//! Crossing runtimes forces the IR — the loss there is real and unavoidable, and the user is told
//! explicitly.
//!
//! # Why the id changes
//!
//! Reusing the original id overwrites the session the runtime already has. Both runtimes require
//! UUID form, so mint a new one (`mint_id()`).
//!
//! The cost is that the new session and the original one have **no inferable relationship** —
//! this is exactly why the store link carries a `parent` field: lineage must be recorded
//! explicitly at install time; it cannot be reconstructed afterward.

use crate::Result;
use crate::adapter::{self, Installed};
use std::path::Path;

/// Install a copy into the target runtime.
///
/// `source_rt` is the runtime the content comes from, `target_rt` the one it goes into. The same
/// on both sides takes the byte rewrite; different sides take the IR conversion.
pub fn install(
    content: &str,
    source_rt: &str,
    target_rt: &str,
    cwd: &Path,
) -> Result<(Installed, bool)> {
    let dst = adapter::get(target_rt)?;

    // Refuse **before any work**, not after rendering and writing the file.
    //
    // Cursor is the only such target: its transcript is a projection, not the source of truth, and
    // writing into it does not make it accept anything. The worst failure shape is "the command
    // returns success and the user opens Cursor to nothing", which is far worse than an outright
    // error — the user assumes the context is there and tells the whole story again.
    if !dst.installable() {
        // The reason and the alternative live in the adapter's own `install`; propagate that
        // error unchanged — the same paragraph maintained in two places goes stale in one of
        // them sooner or later.
        dst.install("", "", cwd)?;
        anyhow::bail!("{target_rt} does not support installing sessions");
    }

    let new_id = dst.mint_id();

    if !adapter::is_lossy_conversion(source_rt, target_rt) {
        // Same format family: byte rewrite plus bounded Codex localization, no IR. Which set of
        // identity keys is rewritten follows the format family, not the runtime name — several
        // runtimes can share one format (§4.3).
        let format = adapter::get(source_rt)?.format();
        let localized = localize_same_format(content, format, &new_id, cwd)?;
        return Ok((dst.install(&localized, &new_id, cwd)?, false));
    }

    // Across runtimes: the IR only, and it is lossy. Tool call arguments and their paired outputs
    // deliberately stay out of the IR; enrich reads them back from the source transcript by
    // `Event.line` and hands them to the target with the render — so the calls the model sees on
    // both sides are as close to the same content as possible.
    let src = adapter::get(source_rt)?;
    let ir = src.parse(content)?;
    let details = adapter::enrich::tool_details(src.format(), content, &ir);
    let rendered = dst.render_with(&ir, &new_id, cwd, &details)?;
    Ok((dst.install(&rendered, &new_id, cwd)?, true))
}

/// Codex bootstraps from a first-line `session_meta`, and only accepts one whose
/// `payload.timestamp` is present — without it the line does not count as session metadata and
/// the whole file answers "No saved session found". Both defect shapes are treated: a VIEW that
/// opens at a compact boundary has no such line at all (synthesize one, with the id minted for
/// this install and a fixed timestamp — the same pure-function discipline as render); a legacy
/// line lacks `payload.timestamp` (fill in the required fields, keep the rest verbatim). A valid
/// line is not touched by a single byte; other format families have no bootstrap concept and pass
/// through unchanged.
fn ensure_codex_bootstrap(content: &str, format: &str, new_id: &str, cwd: &Path) -> Result<String> {
    if format != "codex" {
        return Ok(content.to_owned());
    }
    let now = "2026-01-01T00:00:00.000Z";
    let meta_line = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|x| x.as_str()) == Some("session_meta"));
    if let Some(mut v) = meta_line {
        // Valid = both the timestamp and a non-empty provider are present: either key missing
        // makes Codex fail hard (without the first the whole line does not count as metadata;
        // without the second the provider reads as an empty string), and both defect shapes are
        // written to disk in the wild.
        let valid = v
            .pointer("/payload/timestamp")
            .and_then(|x| x.as_str())
            .is_some()
            && v.pointer("/payload/model_provider")
                .and_then(|x| x.as_str())
                .is_some_and(|p| !p.is_empty());
        // The paginated store lives on the writing machine and never syncs with the repo; the
        // local Codex does not replay a rollout for `paginated` (both the model context and the
        // visible history come up empty); replay belongs to `legacy` alone. Rewriting the mode to
        // `legacy` is the only path that makes this file readable on this machine.
        let paginated =
            v.pointer("/payload/history_mode").and_then(|x| x.as_str()) == Some("paginated");
        if valid && !paginated {
            return Ok(content.to_owned());
        }
        if paginated && let Some(p) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
            p.insert(
                "history_mode".into(),
                serde_json::Value::String("legacy".into()),
            );
        }
        if valid {
            let mut out = String::new();
            let mut replaced = false;
            for line in content.lines() {
                if !replaced && !line.trim().is_empty() {
                    out.push_str(&serde_json::to_string(&v)?);
                    replaced = true;
                } else {
                    out.push_str(line);
                }
                out.push('\n');
            }
            return Ok(out);
        }
        // Defective line (the legacy shape; Codex only accepts a session_meta whose payload
        // carries a timestamp): fill in the required fields, keep the provider and the rest of
        // the metadata verbatim.
        let top_ts = v
            .get("timestamp")
            .and_then(|x| x.as_str())
            .unwrap_or(now)
            .to_string();
        if let Some(p) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
            p.insert("timestamp".into(), serde_json::Value::String(top_ts));
            p.entry("source")
                .or_insert(serde_json::Value::String("cli".into()));
            // Codex's thread/resume reads a missing provider as an empty string and fails hard
            // with "Model provider `` not found" — this key must be a **non-empty string**. A
            // valid value is kept verbatim; missing, empty, and non-string are uniformly
            // replaced with Codex's own default.
            let provider_ok = p
                .get("model_provider")
                .and_then(|x| x.as_str())
                .is_some_and(|s| !s.is_empty());
            if !provider_ok {
                p.insert(
                    "model_provider".into(),
                    serde_json::Value::String("openai".into()),
                );
            }
        }
        let mut out = String::new();
        let mut replaced = false;
        for line in content.lines() {
            if !replaced && !line.trim().is_empty() {
                out.push_str(&serde_json::to_string(&v)?);
                replaced = true;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        return Ok(out);
    }
    // The line is absent entirely: synthesize it. The original provider went with the bootstrap
    // that was cut away (`turn_context` carries the model, not the provider), and this key cannot
    // be missing — Codex's thread/resume reads a missing one as an empty string and fails hard
    // with "Model provider `` not found". Writing Codex's own default does not point the session
    // somewhere else: without it the session does not start at all, and a session on a non-OpenAI
    // backend still overrides explicitly with `codex resume <id> -c model_provider=<x>`.
    let meta = serde_json::to_string(&serde_json::json!({
        "type": "session_meta",
        "timestamp": now,
        "payload": {
            "id": new_id,
            "timestamp": now,
            "cwd": cwd.to_string_lossy(),
            "originator": "agit",
            "cli_version": env!("CARGO_PKG_VERSION"),
            "source": "cli",
            "model_provider": "openai",
        }
    }))?;
    Ok(format!("{meta}\n{content}"))
}

/// For a codex transcript with no legacy visible events, mirror every message as an `event_msg`
/// carrying the same text.
///
/// Codex rebuilds its visible history only from `user_message` / `agent_message` events; a rollout
/// from a paginated writer holds none of them, so after replay the model remembers everything and
/// the person sees a blank screen. A transcript that already carries such events (one from a
/// legacy writer) is not touched by a single byte. The `replacement_history` of a `compacted`
/// record is the only carrier of the retained context, so it is mirrored as well.
fn ensure_codex_visible_history(content: &str, format: &str) -> Result<String> {
    if format != "codex" {
        return Ok(content.to_owned());
    }
    let has_legacy_events = content.lines().any(|l| {
        serde_json::from_str::<serde_json::Value>(l.trim()).is_ok_and(|v| {
            v.get("type").and_then(|x| x.as_str()) == Some("event_msg")
                && matches!(
                    v.pointer("/payload/type").and_then(|x| x.as_str()),
                    Some("user_message" | "agent_message")
                )
        })
    });
    if has_legacy_events {
        return Ok(content.to_owned());
    }
    let text_of = |content: Option<&serde_json::Value>| -> Option<String> {
        let parts: Vec<&str> = content?
            .as_array()?
            .iter()
            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
            .collect();
        let joined = parts.join("\n");
        (!joined.trim().is_empty()).then_some(joined)
    };
    // Role and injection are decided by the adapter's own classification (a runtime-injected user
    // message such as <environment_context>, and the developer role, are not conversation content
    // and Codex does not display them natively either; what is left once the attachment prefix is
    // stripped is what the person typed).
    let mirror = |role: &str, text: String, ts: &serde_json::Value| -> Option<String> {
        let (kind, text) = crate::adapter::codex::classify_message(role, text);
        let payload = match kind {
            crate::adapter::EventKind::UserPrompt => serde_json::json!({
                "type": "user_message", "message": text,
                "images": [], "local_images": [], "audio": [], "local_audio": [],
                "text_elements": []
            }),
            crate::adapter::EventKind::AssistantReply => serde_json::json!({
                "type": "agent_message", "message": text,
                "phase": "final_answer", "memory_citation": null
            }),
            _ => return None,
        };
        serde_json::to_string(&serde_json::json!({
            "type": "event_msg", "timestamp": ts, "payload": payload
        }))
        .ok()
    };
    let mut out = String::new();
    for line in content.lines() {
        out.push_str(line);
        out.push('\n');
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let ts = v
            .get("timestamp")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match v.get("type").and_then(|x| x.as_str()) {
            Some("response_item")
                if v.pointer("/payload/type").and_then(|x| x.as_str()) == Some("message") =>
            {
                let role = v
                    .pointer("/payload/role")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if let Some(t) = text_of(v.pointer("/payload/content"))
                    && let Some(m) = mirror(role, t, &ts)
                {
                    out.push_str(&m);
                    out.push('\n');
                }
            }
            Some("compacted") => {
                let Some(rh) = v
                    .pointer("/payload/replacement_history")
                    .and_then(|x| x.as_array())
                else {
                    continue;
                };
                for m in rh {
                    if m.get("type").and_then(|x| x.as_str()) != Some("message") {
                        continue;
                    }
                    let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("");
                    if let Some(t) = text_of(m.get("content"))
                        && let Some(line) = mirror(role, t, &ts)
                    {
                        out.push_str(&line);
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Every rewrite a same-format install performs, in order, in one pass.
///
/// What this path may move is a **closed set**: identity keys (id / session_id / cwd), placeholder
/// outputs for open calls, the Codex bootstrap line (fill in / synthesize / localize
/// `history_mode`), and Codex visible-event mirrors. Outside the closed set the **semantics are
/// unchanged** — lines are rewritten through canonical serialization, key order and whitespace
/// are not preserved (the same stance as the envelope hash normalization);
/// `only_the_closed_set_of_localizations_applies` pins this closed set.
fn localize_same_format(content: &str, format: &str, new_id: &str, cwd: &Path) -> Result<String> {
    let rewritten = rewrite_identity(content, format, new_id, cwd)?;
    let closed = close_open_calls(&rewritten, format, new_id, cwd)?;
    let bootstrapped = ensure_codex_bootstrap(&closed, format, new_id, cwd)?;
    ensure_codex_visible_history(&bootstrapped, format)
}

/// Body of the placeholder output written for an open tool call.
///
/// The resumed agent reads it, so it has to say exactly what is missing here: the call happened
/// and its result was never recorded — not "the command produced no output".
pub const OPEN_CALL_PLACEHOLDER_OUTPUT: &str =
    "[agit] this tool call had not returned when the turn was settled; its output is not recorded.";

/// Append one placeholder output for every tool call in the transcript that never got one.
///
/// # Why the calls must be closed before installing
///
/// A VIEW can be recorded with calls still open: the agent was interrupted, or it ran `agit
/// commit` itself inside a turn that a later user prompt closed. Resuming such a rollout, Codex
/// reports "output is missing for call id" before every request and the session does not start at
/// all. Installing into the runtime is the only place this invariant can be caught: before it lies
/// history already in the repo (which must not be changed), after it the runtime itself is
/// writing.
///
/// Only append at the end, never rewrite an existing line: the appended lines sit inside the
/// materialized baseline, so settlement treats them as history along with the baseline rather than
/// as new content.
fn close_open_calls(content: &str, format: &str, new_id: &str, cwd: &Path) -> Result<String> {
    let runtime = match format {
        "codex" | "claude-code" => format,
        _ => return Ok(content.to_owned()),
    };
    let open = adapter::get(runtime)?.open_tool_calls(content);
    if open.is_empty() {
        return Ok(content.to_owned());
    }

    let last: Option<serde_json::Value> = content
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok());
    let ts = last
        .as_ref()
        .and_then(|v| v.get("timestamp"))
        .and_then(|t| t.as_str())
        .unwrap_or("2026-01-01T00:00:00.000Z")
        .to_string();

    let mut out = content.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    match runtime {
        "codex" => {
            for c in open {
                // Each call family gets its own output record; the pairing check looks only at
                // `call_id`, so a wrong type still reads as "closed" while what Codex receives is
                // a record whose family does not match.
                let output_type = match c.record.as_str() {
                    "custom_tool_call" => "custom_tool_call_output",
                    "local_shell_call" => "local_shell_call_output",
                    _ => "function_call_output",
                };
                out.push_str(&serde_json::to_string(&serde_json::json!({
                    "type": "response_item",
                    "timestamp": ts,
                    "payload": {
                        "type": output_type,
                        "call_id": c.call_id,
                        "output": OPEN_CALL_PLACEHOLDER_OUTPUT
                    }
                }))?);
                out.push('\n');
            }
        }
        _ => {
            let mut parent = last
                .as_ref()
                .and_then(|v| v.get("uuid"))
                .and_then(|u| u.as_str())
                .map(str::to_string);
            for c in open {
                let uuid = uuid::Uuid::now_v7().to_string();
                out.push_str(&serde_json::to_string(&serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "isSidechain": false,
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": c.call_id,
                            "is_error": true,
                            "content": OPEN_CALL_PLACEHOLDER_OUTPUT
                        }]
                    },
                    "parentUuid": parent,
                    "sessionId": new_id,
                    "timestamp": ts,
                    "type": "user",
                    "userType": "external",
                    "uuid": uuid,
                }))?);
                out.push('\n');
                parent = Some(uuid);
            }
        }
    }
    Ok(out)
}

/// Rewrite the identity fields line by line, keep everything else unchanged.
///
/// This implements "a lossless install back into a runtime of the same format family". The test is
/// **touch only the known identity keys**, with no structural transformation — every field we do
/// not recognize must pass through unchanged.
///
/// `format` is the transcript's format family ([`crate::adapter::Adapter::format`]), not the
/// runtime name: `claude-desktop` and `claude-code` share one set of identity keys (§4.3).
fn rewrite_identity(content: &str, format: &str, new_id: &str, cwd: &Path) -> Result<String> {
    let cwd_s = cwd.to_string_lossy().to_string();
    let mut out = String::with_capacity(content.len());

    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(t) else {
            // A malformed line is kept unchanged: a transcript can be truncated, and a partial
            // session is still worth recovering.
            out.push_str(line);
            out.push('\n');
            continue;
        };

        match format {
            "codex" => {
                // id / session_id / cwd in the first-line `session_meta` (`session_id` holds the
                // same value as `id`, the companion identity key of a newer rollout — leaving the
                // stale value there sets two identities against each other).
                if v.get("type").and_then(|x| x.as_str()) == Some("session_meta")
                    && let Some(p) = v.get_mut("payload").and_then(|p| p.as_object_mut())
                {
                    p.insert("id".into(), serde_json::Value::String(new_id.into()));
                    if p.contains_key("session_id") {
                        p.insert(
                            "session_id".into(),
                            serde_json::Value::String(new_id.into()),
                        );
                    }
                    p.insert("cwd".into(), serde_json::Value::String(cwd_s.clone()));
                }
            }
            "claude-code" => {
                // Every line carries `sessionId`; `cwd` is per-line too.
                if let Some(o) = v.as_object_mut() {
                    if o.contains_key("sessionId") {
                        o.insert("sessionId".into(), serde_json::Value::String(new_id.into()));
                    }
                    if o.contains_key("cwd") {
                        o.insert("cwd".into(), serde_json::Value::String(cwd_s.clone()));
                    }
                }
            }
            _ => {}
        }

        out.push_str(&serde_json::to_string(&v)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A VIEW that opens at a compact boundary has no `session_meta`: installing back into Codex
    /// must prepend one, or `codex resume` scans the file and counts none of it as a session. This
    /// also pins that a transcript that already has a `session_meta` is untouched, and that other
    /// format families pass through unchanged.
    #[test]
    fn a_headless_codex_view_gets_a_session_meta_before_install() {
        let content = concat!(
            r#"{"type":"compacted","ordinal":1,"payload":{"window_number":4,"replacement_history":[]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}"#,
            "\n",
        );
        let got = ensure_codex_bootstrap(content, "codex", "NEW", Path::new("/w")).unwrap();
        let first: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "session_meta");
        assert_eq!(first["payload"]["id"], "NEW");
        assert_eq!(first["payload"]["cwd"], "/w");
        assert!(
            first["payload"]["timestamp"].is_string(),
            "without payload.timestamp Codex refuses to read the line as session_meta"
        );
        assert_eq!(
            first["payload"]["model_provider"], "openai",
            "a synthesized meta writes Codex's default provider; a missing one fails hard"
        );
        assert!(
            got.ends_with(content),
            "the existing content is unchanged, byte for byte"
        );

        let valid = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"OLD","cwd":"/x","timestamp":"t","model_provider":"azure"}}"#,
            "\n",
        );
        assert_eq!(
            ensure_codex_bootstrap(valid, "codex", "NEW", Path::new("/w")).unwrap(),
            valid,
            "a valid bootstrap is unchanged, byte for byte"
        );
        assert_eq!(
            ensure_codex_bootstrap(content, "claude-code", "NEW", Path::new("/w")).unwrap(),
            content
        );
    }

    /// An empty or non-string provider is as bad as a missing one: the repair path uniformly
    /// writes the default, and a valid value is kept verbatim.
    #[test]
    fn an_empty_provider_value_is_replaced_not_passed_through() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"NEW","timestamp":"t","cwd":"/w","model_provider":""}}"#,
            "\n",
        );
        let got = ensure_codex_bootstrap(content, "codex", "NEW", Path::new("/w")).unwrap();
        let first: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(first["payload"]["model_provider"], "openai");
    }

    /// Same-format localization may move the closed set only: a native legacy transcript (valid
    /// bootstrap, its own visible events, calls closed) is semantically unchanged apart from the
    /// identity keys (lines go through canonical serialization and key order is not preserved —
    /// so the assertion compares parsed values, not raw bytes).
    #[test]
    fn only_the_closed_set_of_localizations_applies() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"OLD","session_id":"OLD","timestamp":"t","cwd":"/old","model_provider":"azure","history_mode":"legacy","originator":"codex_cli_rs"}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t1","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"a question"}]}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"t1","payload":{"type":"user_message","message":"a question","images":[],"local_images":[],"audio":[],"local_audio":[],"text_elements":[]}}"#,
            "\n",
        );
        let got = localize_same_format(content, "codex", "NEW", Path::new("/new")).unwrap();
        let want = content
            .replace(r#""id":"OLD""#, r#""id":"NEW""#)
            .replace(r#""session_id":"OLD""#, r#""session_id":"NEW""#)
            .replace(r#""cwd":"/old""#, r#""cwd":"/new""#);
        let norm = |s: &str| -> Vec<serde_json::Value> {
            s.lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        };
        assert_eq!(
            norm(&got),
            norm(&want),
            "semantics outside the closed set are unchanged"
        );
    }

    /// A meta from a paginated writer is rewritten to `legacy` on this machine: the paginated
    /// store does not sync with the repo, Codex does not replay a rollout for `paginated`, and
    /// both the model context and the visible history come up empty. This pins that every other
    /// key is kept verbatim, and that a line already on `legacy` (or with no mode written) is
    /// untouched.
    #[test]
    fn a_paginated_history_mode_is_rewritten_to_legacy() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"NEW","timestamp":"t","cwd":"/w","model_provider":"azure","history_mode":"paginated","originator":"Codex Desktop"}}"#,
            "\n",
        );
        let got = ensure_codex_bootstrap(content, "codex", "NEW", Path::new("/w")).unwrap();
        let first: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(first["payload"]["history_mode"], "legacy");
        assert_eq!(
            first["payload"]["model_provider"], "azure",
            "every other key is kept verbatim"
        );
        assert_eq!(first["payload"]["originator"], "Codex Desktop");
    }

    /// In a transcript with no legacy visible events, every message — including the context a
    /// `compacted` record retains — gets a mirror carrying the same text; a transcript that
    /// already has such events is untouched.
    #[test]
    fn visible_history_mirrors_are_injected_once() {
        let content = concat!(
            r#"{"type":"compacted","ordinal":1,"timestamp":"t0","payload":{"window_number":4,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"the opening request"}]}]}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t1","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"will do"}]}}"#,
            "\n",
        );
        let got = ensure_codex_visible_history(content, "codex").unwrap();
        let lines: Vec<serde_json::Value> = got
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let users: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|v| v.pointer("/payload/type").and_then(|x| x.as_str()) == Some("user_message"))
            .collect();
        let agents: Vec<&serde_json::Value> = lines
            .iter()
            .filter(|v| {
                v.pointer("/payload/type").and_then(|x| x.as_str()) == Some("agent_message")
            })
            .collect();
        assert_eq!(users.len(), 1, "{got}");
        assert_eq!(users[0]["payload"]["message"], "the opening request");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["payload"]["message"], "will do");
        assert_eq!(
            ensure_codex_visible_history(&got, "codex").unwrap(),
            got,
            "a transcript that already carries visible events gets no second injection"
        );
        assert_eq!(
            ensure_codex_visible_history(content, "claude-code").unwrap(),
            content
        );
    }

    /// A runtime-injected user message (environment_context and the like) never becomes a visible
    /// mirror: Codex does not display them natively either, and mirroring one passes machine text
    /// off as something a person said.
    #[test]
    fn injected_user_messages_are_not_mirrored() {
        let content = concat!(
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<cwd>/w</cwd>\n</environment_context>"}]}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"what a person actually typed"}]}}"#,
            "\n",
        );
        let got = ensure_codex_visible_history(content, "codex").unwrap();
        assert_eq!(got.matches("\"user_message\"").count(), 1, "{got}");
        assert!(got.contains("what a person actually typed"));
    }

    /// A line with a timestamp but no provider is repaired too (a shape that lands on disk from
    /// another synthesis path): checking the timestamp alone lets it through early, and Codex reads
    /// the missing provider as an empty string and fails hard.
    #[test]
    fn a_meta_with_timestamp_but_no_provider_is_repaired_too() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"t","payload":{"id":"NEW","timestamp":"t","cwd":"/w","originator":"agit","source":"cli"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
            "\n",
        );
        let got = ensure_codex_bootstrap(content, "codex", "NEW", Path::new("/w")).unwrap();
        let first: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(
            first["payload"]["model_provider"], "openai",
            "a missing provider must be filled in"
        );
        assert_eq!(
            first["payload"]["timestamp"], "t",
            "an existing timestamp is left alone"
        );
        assert_eq!(got.lines().count(), 2);
    }

    /// A legacy `session_meta` lacking `payload.timestamp` is, to Codex, no `session_meta` at all.
    /// The required fields are filled in; the provider and the rest of the metadata are kept
    /// verbatim.
    #[test]
    fn a_defective_legacy_session_meta_is_repaired_not_passed_through() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"NEW","cwd":"/w","originator":"agit","cli_version":"0.8.0","model_provider":"azure"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
            "\n",
        );
        let got = ensure_codex_bootstrap(content, "codex", "NEW", Path::new("/w")).unwrap();
        let first: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(
            first["payload"]["timestamp"], "2026-01-01T00:00:00Z",
            "the top-level timestamp is filled into the payload"
        );
        assert_eq!(first["payload"]["source"], "cli");
        assert_eq!(
            first["payload"]["model_provider"], "azure",
            "the original provider is kept verbatim, not overwritten by the default"
        );
        assert_eq!(first["payload"]["cli_version"], "0.8.0");
        assert_eq!(got.lines().count(), 2);
        assert!(got.contains("input_text"));
    }

    /// A Codex call that had not returned when the turn was recorded must be closed before the
    /// install: resuming a rollout with an open `custom_tool_call`, Codex reports output missing
    /// before every request.
    #[test]
    fn a_codex_call_without_output_gets_a_placeholder_before_install() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"t0","payload":{"id":"OLD","cwd":"/old"}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t1","payload":{"type":"custom_tool_call","call_id":"c1","name":"exec_command","input":"agit commit"}}"#,
            "\n",
        );
        let got = close_open_calls(content, "codex", "NEW", Path::new("/new")).unwrap();
        let lines: Vec<serde_json::Value> = got
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2]["payload"]["type"], "custom_tool_call_output");
        assert_eq!(lines[2]["payload"]["call_id"], "c1");
        assert_eq!(lines[2]["payload"]["output"], OPEN_CALL_PLACEHOLDER_OUTPUT);
        assert_eq!(lines[2]["timestamp"], "t1");
        assert!(
            adapter::get("codex")
                .unwrap()
                .open_tool_calls(&got)
                .is_empty()
        );
        // A transcript whose calls are already closed is not touched by a single byte.
        assert_eq!(
            close_open_calls(&got, "codex", "NEW", Path::new("/new")).unwrap(),
            got
        );
    }

    /// `function_call` pairs with `function_call_output`, separate from the custom family.
    #[test]
    fn a_function_call_is_closed_with_a_function_call_output() {
        let content = r#"{"type":"response_item","payload":{"type":"function_call","call_id":"f1","name":"shell","arguments":"{}"}}"#;
        let got = close_open_calls(content, "codex", "NEW", Path::new("/new")).unwrap();
        let last: serde_json::Value = serde_json::from_str(got.lines().last().unwrap()).unwrap();
        assert_eq!(last["payload"]["type"], "function_call_output");
        assert_eq!(last["payload"]["call_id"], "f1");
    }

    /// Every call family lands on the output record of its own family; the pairing check looks
    /// only at `call_id` and cannot tell a wrong type apart, so this asserts the resulting payload
    /// type directly.
    #[test]
    fn each_call_family_is_closed_with_its_own_output_record() {
        for (call, output) in [
            ("function_call", "function_call_output"),
            ("local_shell_call", "local_shell_call_output"),
            ("custom_tool_call", "custom_tool_call_output"),
        ] {
            let content = format!(
                r#"{{"type":"response_item","payload":{{"type":"{call}","call_id":"x","name":"shell"}}}}"#
            );
            let got = close_open_calls(&content, "codex", "NEW", Path::new("/new")).unwrap();
            let last: serde_json::Value =
                serde_json::from_str(got.lines().last().unwrap()).unwrap();
            assert_eq!(last["payload"]["type"], output, "{call}");
        }
    }

    /// An open Claude Code `tool_use` gets a `tool_result` record, hung at the end of the chain
    /// and carrying the new identity.
    #[test]
    fn a_claude_tool_use_without_result_gets_a_placeholder_before_install() {
        let content = concat!(
            r#"{"type":"user","sessionId":"OLD","cwd":"/old","uuid":"u1","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"OLD","cwd":"/old","uuid":"u2","parentUuid":"u1","timestamp":"t2","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"agit commit"}}]}}"#,
            "\n",
        );
        let got = close_open_calls(content, "claude-code", "NEW", Path::new("/new")).unwrap();
        let last: serde_json::Value = serde_json::from_str(got.lines().last().unwrap()).unwrap();
        assert_eq!(last["type"], "user");
        assert_eq!(last["parentUuid"], "u2");
        assert_eq!(last["sessionId"], "NEW");
        assert_eq!(last["cwd"], "/new");
        assert_eq!(last["timestamp"], "t2");
        assert_eq!(last["message"]["content"][0]["tool_use_id"], "toolu_1");
        assert!(
            adapter::get("claude-code")
                .unwrap()
                .open_tool_calls(&got)
                .is_empty()
        );
    }

    /// A format family whose pairing protocol is unknown passes through unchanged.
    #[test]
    fn unknown_formats_are_left_alone() {
        let content = "whatever\n";
        assert_eq!(
            close_open_calls(content, "opencode", "NEW", Path::new("/new")).unwrap(),
            content
        );
    }

    /// `session_id` is the companion identity key of a newer rollout, holding the same value as
    /// `id`: leaving the stale value there sets two identities against each other.
    #[test]
    fn codex_rewrite_updates_the_companion_session_id() {
        let content = r#"{"type":"session_meta","payload":{"id":"OLD","session_id":"OLD","cwd":"/old","history_mode":"paginated"}}"#;
        let got = rewrite_identity(content, "codex", "NEW", Path::new("/new")).unwrap();
        let v: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(v["payload"]["id"], "NEW");
        assert_eq!(v["payload"]["session_id"], "NEW");
        assert_eq!(
            v["payload"]["history_mode"], "paginated",
            "a non-identity key is left alone"
        );
    }

    #[test]
    fn codex_rewrite_touches_only_identity() {
        let content = concat!(
            r#"{"type":"session_meta","timestamp":"t0","payload":{"id":"OLD","cwd":"/old","originator":"codex","cli_version":"1.2"}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t1","payload":{"type":"reasoning","encrypted_content":"KEEP-ME"}}"#,
            "\n",
        );
        let got = rewrite_identity(content, "codex", "NEW", Path::new("/new")).unwrap();

        let l1: serde_json::Value = serde_json::from_str(got.lines().next().unwrap()).unwrap();
        assert_eq!(l1["payload"]["id"], "NEW");
        assert_eq!(l1["payload"]["cwd"], "/new");
        // An unrecognized field must be kept unchanged.
        assert_eq!(l1["payload"]["originator"], "codex");
        assert_eq!(l1["payload"]["cli_version"], "1.2");

        // Encrypted reasoning must survive — this is exactly why this path avoids the IR.
        let l2: serde_json::Value = serde_json::from_str(got.lines().nth(1).unwrap()).unwrap();
        assert_eq!(l2["payload"]["encrypted_content"], "KEEP-ME");
    }

    #[test]
    fn claude_code_rewrite_updates_every_line() {
        let content = concat!(
            r#"{"type":"user","sessionId":"OLD","cwd":"/old","uuid":"u1","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"OLD","cwd":"/old","uuid":"u2","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"KEEP"}]}}"#,
            "\n",
        );
        let got = rewrite_identity(content, "claude-code", "NEW", Path::new("/new")).unwrap();
        for line in got.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["sessionId"], "NEW");
            assert_eq!(v["cwd"], "/new");
        }
        // The uuid / parentUuid chain and the thinking block are both left unchanged.
        let l2: serde_json::Value = serde_json::from_str(got.lines().nth(1).unwrap()).unwrap();
        assert_eq!(l2["uuid"], "u2");
        assert_eq!(l2["parentUuid"], "u1");
        assert_eq!(l2["message"]["content"][0]["thinking"], "KEEP");
    }

    #[test]
    fn malformed_lines_survive() {
        // A transcript can be truncated (the process was killed), and a partial session is still
        // worth recovering.
        let content = "not json at all\n{\"type\":\"user\",\"sessionId\":\"OLD\"}\n";
        let got = rewrite_identity(content, "claude-code", "NEW", Path::new("/w")).unwrap();
        assert!(
            got.contains("not json at all"),
            "a malformed line is kept unchanged"
        );
        assert!(got.contains("NEW"));
    }

    /// A target that cannot be installed into must be refused **before any work**, and the error
    /// must spell out the alternative.
    ///
    /// This guards the worst failure shape of the whole system: the command returns success and
    /// the user opens the runtime to nothing. So what is asserted is not only "it failed" but
    /// "it failed usefully".
    #[test]
    fn an_uninstallable_target_is_refused_before_any_work() {
        let d = tempfile::tempdir().unwrap();
        let content =
            r#"{"type":"user","sessionId":"OLD","message":{"role":"user","content":"hi"}}"#;
        let e = install(content, "claude-code", "cursor", d.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("state.vscdb"), "the error must say why: {e}");
        assert!(
            e.contains("--as codex"),
            "the error must give a next step to follow: {e}"
        );
        // And nothing at all was written.
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn rewrite_is_idempotent_on_ids() {
        let content = r#"{"type":"session_meta","payload":{"id":"A","cwd":"/x"}}"#;
        let a = rewrite_identity(content, "codex", "N", Path::new("/y")).unwrap();
        let b = rewrite_identity(&a, "codex", "N", Path::new("/y")).unwrap();
        assert_eq!(
            a, b,
            "rewriting twice with the same target identity gives the same result"
        );
    }
}
