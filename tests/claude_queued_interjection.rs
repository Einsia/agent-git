//! Claude Code lets the user keep typing while the agent is still running a tool: that sentence
//! does not become the next turn, it is absorbed into the current one. The transcript keeps only a
//! `queue-operation` for it (`remove`, `reason: absorbed_mid_turn`), no `user` record. From the CLI
//! entry point this pins two things: after adoption the sentence is readable in the VIEW, and it
//! does not split one turn into two.

use std::process::Command;
use std::{fs, io::Write as _};

const SID: &str = "cccccccc-0000-4000-8000-000000000003";
const HUB: &str = "http://127.0.0.1:1";

struct Lab {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    agit_home: std::path::PathBuf,
    work: std::path::PathBuf,
}

impl Lab {
    fn new() -> Lab {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let agit_home = tmp.path().join("agit");
        let work = tmp.path().join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&work).unwrap();
        // The author field comes from the credentials; the hub points at an unreachable address,
        // so import/log/show never go to the network.
        let cred = agit::infra::credentials::HubCredential {
            username: "me".into(),
            email: None,
            hub: Some(HUB.into()),
            access_token: "fake".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: "fake".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        };
        let key = agit::infra::config::hub_host_key(HUB);
        agit::infra::credentials::save_at(
            &agit_home.join("credentials").join(format!("{key}.json")),
            &cred,
        )
        .unwrap();
        Lab {
            _tmp: tmp,
            home,
            agit_home,
            work,
        }
    }

    fn run(&self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&self.work)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("AGIT_HOME", &self.agit_home)
            .env("AGIT_HUB_URL", HUB)
            .env("AGIT_YES", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`agit {}` failed:\n{}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write_transcript(&self, lines: &[serde_json::Value]) {
        let slug = agit::adapter::claude_code::slug_for(&self.work);
        let p = self
            .home
            .join(".claude")
            .join("projects")
            .join(slug)
            .join(format!("{SID}.jsonl"));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = fs::File::create(p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }
}

/// One turn: prompt → reply + tool call → the user interjects mid-turn (enqueue) → tool output
/// (the runtime restates the interjection inside it) → the interjection is absorbed (remove) →
/// reply.
fn one_turn_with_an_interjection(work: &std::path::Path) -> Vec<serde_json::Value> {
    let cwd = work.to_string_lossy().to_string();
    let base = |n: usize, extra: serde_json::Value| {
        let mut v = serde_json::json!({
            "sessionId": SID, "cwd": cwd,
            "uuid": format!("{SID}-{n}"),
            "timestamp": format!("2026-08-29T11:00:{n:02}.000Z"),
        });
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        v
    };
    vec![
        base(
            1,
            serde_json::json!({"type": "user",
            "message": {"role": "user", "content": "merge this branch into main"}}),
        ),
        base(
            2,
            serde_json::json!({"type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "check the working tree first"},
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "git status"}}
            ]}}),
        ),
        base(
            3,
            serde_json::json!({"type": "queue-operation", "operation": "enqueue",
            "content": "also update the CHANGELOG"}),
        ),
        base(
            4,
            serde_json::json!({"type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1",
                 "content": "clean\n\nThe user sent a new message while you were working:\nalso update the CHANGELOG\n\nThis is how Claude Code surfaces messages the user sends mid-turn"}
            ]}}),
        ),
        base(
            5,
            serde_json::json!({"type": "queue-operation", "operation": "remove",
            "reason": "absorbed_mid_turn", "content": "also update the CHANGELOG"}),
        ),
        base(
            6,
            serde_json::json!({"type": "assistant",
            "message": {"role": "assistant", "content": [
                {"type": "text", "text": "ok, the merge and the CHANGELOG together"}
            ]}}),
        ),
    ]
}

#[test]
fn an_absorbed_message_is_readable_in_the_view_and_does_not_split_the_turn() {
    let lab = Lab::new();
    lab.write_transcript(&one_turn_with_an_interjection(&lab.work));

    lab.run(&["init", "qa"]);
    lab.run(&["import", SID, "--from", "claude-code", "--into", "me/qa@s1"]);

    let log = lab.run(&["log", "me/qa@s1", "--oneline"]);
    let turns: Vec<&str> = log.lines().filter(|l| l.contains("[turn ]")).collect();
    assert_eq!(
        turns.len(),
        1,
        "an interjection must not split one turn into two:\n{log}"
    );
    assert!(turns[0].contains("merge this branch into main"), "{log}");

    let shown = lab.run(&["show", "me/qa@s1#1"]);
    assert!(
        shown.contains("also update the CHANGELOG"),
        "the interjection must be readable in the VIEW:\n{shown}"
    );
    assert!(
        shown.contains("ok, the merge and the CHANGELOG together"),
        "{shown}"
    );

    // In the exported IR it is its own kind of event, not a prompt.
    let ir = lab.run(&["export", "me/qa@s1", "--format", "ir"]);
    assert!(ir.contains("\"kind\":\"UserInterjection\""), "{ir}");
    assert_eq!(ir.matches("\"kind\":\"UserPrompt\"").count(), 1, "{ir}");
}
