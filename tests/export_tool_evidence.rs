//! Native exports preserve paired tool evidence within the selected transcript. Missing outputs
//! remain placeholders, and output belonging to an unselected call cannot become conversation.

use agit::domain::{meta, repo::Repo, storage, transcript};
use serde_json::{Value, json};
use std::{collections::HashMap, process::Command};

fn source(runtime: &str) -> String {
    let calls = ["alpha", "beta", "empty", "pending"];
    let mut lines = if matches!(runtime, "claude-code" | "claude-desktop") {
        vec![
            json!({"type":"user","message":{"role":"user","content":"inspect the files"}}),
            json!({"type":"assistant","message":{"role":"assistant","content":calls.iter().map(|id| json!({
                "type":"tool_use","id":id,"name":"Bash","input":{"command":format!("cat {id}.txt")}
            })).collect::<Vec<_>>()}}),
        ]
    } else {
        let mut lines = vec![json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":[{"type":"input_text","text":"inspect the files"}]
        }})];
        for id in calls {
            lines.push(json!({"type":"response_item","payload":{
                "type":"function_call","call_id":id,"name":"Bash",
                "arguments":json!({"command":format!("cat {id}.txt")}).to_string()
            }}));
        }
        lines
    };
    for (id, body) in [
        ("beta", "BETA-RESULT"),
        ("orphan", "ORPHAN-RESULT"),
        ("empty", ""),
        ("alpha", "ALPHA-RESULT"),
    ] {
        lines.push(if matches!(runtime, "claude-code" | "claude-desktop") {
            json!({"type":"user","message":{"role":"user","content":[{
                "type":"tool_result","tool_use_id":id,"content":body,"is_error":id == "beta"
            }]}})
        } else {
            json!({"type":"response_item","payload":{
                "type":"function_call_output","call_id":id,"output":body
            }})
        });
    }
    lines.into_iter().map(|line| format!("{line}\n")).collect()
}

fn export(home: &std::path::Path, format: &str, view_only: bool) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
    command
        .args(["export", "me/evidence@main", "--format", format])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("AGIT_HOME", home)
        .env("AGIT_HUB_URL", "http://127.0.0.1:1")
        .env("AGIT_YES", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if view_only {
        command.arg("--view-only");
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{format}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn pairs(raw: &str, runtime: &str) -> HashMap<String, (String, bool)> {
    let mut calls = HashMap::new();
    let mut outputs = HashMap::new();
    for line in raw.lines() {
        let value: Value = serde_json::from_str(line).unwrap();
        if matches!(runtime, "claude-code" | "claude-desktop") {
            for block in value["message"]["content"].as_array().into_iter().flatten() {
                match block["type"].as_str() {
                    Some("tool_use") => {
                        calls.insert(
                            block["id"].as_str().unwrap().to_string(),
                            block["input"]["command"].as_str().unwrap().to_string(),
                        );
                    }
                    Some("tool_result") => {
                        outputs.insert(
                            block["tool_use_id"].as_str().unwrap().to_string(),
                            (
                                block["content"].as_str().unwrap().to_string(),
                                block["is_error"].as_bool().unwrap_or(false),
                            ),
                        );
                    }
                    _ => {}
                }
            }
        } else {
            let payload = &value["payload"];
            match payload["type"].as_str() {
                Some("function_call") => {
                    let input: Value =
                        serde_json::from_str(payload["arguments"].as_str().unwrap()).unwrap();
                    calls.insert(
                        payload["call_id"].as_str().unwrap().to_string(),
                        input["command"].as_str().unwrap().to_string(),
                    );
                }
                Some("function_call_output") => {
                    outputs.insert(
                        payload["call_id"].as_str().unwrap().to_string(),
                        (payload["output"].as_str().unwrap().to_string(), false),
                    );
                }
                _ => {}
            }
        }
    }
    assert_eq!(calls.len(), 4);
    assert_eq!(outputs.len(), calls.len());
    calls
        .into_iter()
        .map(|(id, command)| (command, outputs.remove(&id).unwrap()))
        .collect()
}

#[test]
fn native_exports_keep_arguments_and_outputs_paired_without_widening_the_view() {
    for runtime in ["claude-code", "claude-desktop", "codex"] {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path();
        let repo = Repo::init(&home.join("repos/me/evidence")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        let snapshot = meta::Meta::new(meta::mint_session_id(), runtime.into(), "/work".into());
        meta::write(repo.root(), &snapshot).unwrap();
        let raw = source(runtime);
        let view: String = raw
            .lines()
            .take_while(|line| {
                !line.contains("tool_result") && !line.contains("function_call_output")
            })
            .map(|line| format!("{line}\n"))
            .collect();
        storage::write_snapshot(
            repo.root(),
            &transcript::wrap_lines(&raw, runtime, &snapshot.session),
            &transcript::wrap_lines(&view, runtime, &snapshot.session),
        )
        .unwrap();
        repo.add_all().unwrap();
        repo.commit("tool evidence").unwrap();

        let ir = export(home, "ir", false);
        assert!(ir.contains("ToolResult") && ir.contains("ALPHA-RESULT"));
        for target in ["claude-code", "codex"] {
            let native = export(home, target, false);
            let paired = pairs(&native, target);
            assert_eq!(paired["cat alpha.txt"].0, "ALPHA-RESULT");
            assert_eq!(paired["cat beta.txt"].0, "BETA-RESULT");
            assert_eq!(paired["cat empty.txt"].0, "");
            assert_eq!(
                paired["cat pending.txt"].0,
                agit::adapter::codex::CROSS_RUNTIME_OUTPUT_PLACEHOLDER
            );
            if matches!(runtime, "claude-code" | "claude-desktop") && target == "claude-code" {
                assert!(paired["cat beta.txt"].1);
            }
            assert!(!native.contains("ORPHAN-RESULT"));
            assert!(
                agit::adapter::get(target)
                    .unwrap()
                    .open_tool_calls(&native)
                    .is_empty()
            );

            let selected = export(home, target, true);
            let paired = pairs(&selected, target);
            assert!(paired.values().all(|(output, _)| {
                output == agit::adapter::codex::CROSS_RUNTIME_OUTPUT_PLACEHOLDER
            }));
            assert!(!selected.contains("ALPHA-RESULT") && !selected.contains("BETA-RESULT"));
        }
    }
}
