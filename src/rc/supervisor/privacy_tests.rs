use super::*;
use crate::domain::secret_filter::{MatcherHandle, RepositoryDictionary, VaultStore};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

const SECRET: &str = "blue horse battery";
const TOKEN: &str = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
const NATIVE: &str = "3f6b1c2a-8d40-4e7b-9a15-2c0de4f8b731";
const RECORD_ID: &str = "cb71e9d8-a3d2-4c70-930d-98def38d9f54";

fn native_records(phase: &str) -> [serde_json::Value; 3] {
    let reply_id = "88bfaf5e-4107-4cf2-9038-a2a891d171f1";
    let call = format!("toolu_01Qz7mXv9LpZ4tNc8WjF3bHy_{phase}");
    [
        serde_json::json!({"type":"user","sessionId":NATIVE,"uuid":RECORD_ID,"message":{"role":"user","content":format!("{phase} question {SECRET}")}}),
        serde_json::json!({"type":"assistant","sessionId":NATIVE,"uuid":reply_id,"parentUuid":RECORD_ID,"message":{"type":"message","id":"msg_01Qz7mXv9LpZ4tNc8WjF3bHy","role":"assistant","content":[
            {"type":"text","text":format!("{phase} answer {NATIVE} {RECORD_ID} {TOKEN}")},
            {"type":"tool_use","id":call,"name":"Bash","input":{"command":format!("printf '{phase} command {SECRET} {call}'")}}
        ]}}),
        serde_json::json!({"type":"user","sessionId":NATIVE,"uuid":"81567b2e-195a-4ed3-a180-06eca3efc210","parentUuid":reply_id,"message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":call,"content":format!("{phase} output {TOKEN}"),"is_error":false}
        ]}}),
    ]
}

#[test]
fn claude_resume_protects_history_and_new_output_with_or_without_a_repository() {
    const CHILD: &str = "AGIT_RC_PRIVACY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rc::supervisor::privacy_tests::claude_resume_protects_history_and_new_output_with_or_without_a_repository", "--nocapture"])
            .env(CHILD, "1")
            .env("AGIT_HOME", home.path().join("agit"))
            .env("CLAUDE_CONFIG_DIR", home.path().join("claude"))
            .env("AGIT_SECRETS_KEYSTORE", "file")
            .output().unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    VaultStore::open_default()
        .unwrap()
        .add("fixture", SECRET.to_string().into(), false)
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            check_resume_projection(),
        )
        .await
        .expect("resume projection must complete");
    });
}

async fn check_resume_projection() {
    for bound in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = crate::domain::repo::Repo::init(&dir.path().join("workspace")).unwrap();
        let program = dir.path().join("claude-fixture");
        std::fs::write(&program, "#!/bin/sh\nexec cat\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _program =
            crate::rc::harness::proc::override_harness_program(program.to_string_lossy());
        let lineage = bound.then(|| {
            crate::rc::lineage::AgitSession::new(
                "alice/privacy",
                "8d5c2a51-8af0-4e5f-9f9e-731acee20c17",
                "work",
            )
            .unwrap()
        });
        let repo = lineage
            .as_ref()
            .map(|lineage| crate::domain::repo::Repo::init(&lineage.repo_dir().unwrap()).unwrap());
        if bound {
            let store = crate::domain::store::Store::open_or_init().unwrap();
            let mut link =
                crate::domain::link::Link::new("claude-code", NATIVE, Some(workspace.root()));
            link.owner = Some("alice".into());
            link.agent = Some("privacy".into());
            link.branch = Some("work".into());
            crate::domain::link::write(&store, &link).unwrap();
        }
        let info: SessionInfo = serde_json::from_value(serde_json::json!({
            "session_id":"privacy-session", "workspace_id":"workspace", "runtime":"claude-code",
            "status":"running", "last_seq":0, "created_at":"now", "updated_at":"now"
        }))
        .unwrap();
        let spec = LaunchSpec {
            cwd: workspace.root().to_owned(),
            resume_from: Some(NATIVE.into()),
            agit_session: lineage,
            model: None,
            dangerous: false,
            permission_mode: None,
        };
        let (out, mut frames) = mpsc::channel(64);
        let (notes, mut notices) = mpsc::channel(8);
        let (_confinement, confinement) =
            tokio::sync::watch::channel(crate::rc::Confinement::default());
        let (_settlement, settlement) = tokio::sync::watch::channel(SettlementState::default());
        let project = crate::adapter::claude_code::projects_dir()
            .unwrap()
            .join(crate::adapter::claude_code::slug_for(workspace.root()));
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{NATIVE}.jsonl"));
        let mut transcript = std::fs::File::create(&path).unwrap();
        for record in native_records("history") {
            writeln!(transcript, "{record}").unwrap();
        }
        let mut session = Session::launch(
            info,
            spec,
            out,
            notes,
            confinement,
            settlement,
            1,
            MatcherHandle::load_default().unwrap(),
        )
        .await
        .unwrap();
        let Some(SessionNote::Bound {
            agit_session,
            expected_agent_id,
            ..
        }) = notices.recv().await
        else {
            panic!("missing binding notice")
        };
        assert_eq!(agit_session.is_some(), bound);
        assert_eq!(expected_agent_id.is_some(), bound);
        session.tailer = Some(Tailer::new(&path, true));
        for phase in ["history", "live"] {
            let call = format!("toolu_01Qz7mXv9LpZ4tNc8WjF3bHy_{phase}");
            let records = native_records(phase);
            if phase == "live" {
                for record in &records {
                    writeln!(transcript, "{record}").unwrap();
                }
            }
            session.drain_transcript().await;
            let items: Vec<ItemCompleted> = std::iter::from_fn(|| frames.try_recv().ok())
                .filter(|frame| frame.method.as_deref() == Some(method::ITEM_COMPLETED))
                .map(|frame| serde_json::from_value(frame.params.unwrap()).unwrap())
                .collect();
            let wire = serde_json::to_string(&items).unwrap();
            for suffix in ["question", "answer", "command", "output"] {
                assert!(wire.contains(&format!("{phase} {suffix}")), "{wire}");
            }
            assert!(!wire.contains(SECRET));
            assert!(!wire.contains(TOKEN));
            assert!(!wire.contains(redact::PROTECTION_ERROR_TEXT));
            let page = crate::rc::local_history::read(serde_json::json!({
                "runtime":"claude-code", "session_id":NATIVE, "cwd":workspace.root()
            }))
            .unwrap();
            let history = page["items"].as_array().unwrap();
            for item in &items {
                let paged = history
                    .iter()
                    .find(|paged| paged["source_id"].as_str() == item.source_id.as_deref())
                    .expect("paged history and resumed output must share source identities");
                assert_eq!(paged["raw"], item.raw);
                let mut event = item.event.clone();
                event.line = None;
                assert_eq!(paged["event"], serde_json::to_value(event).unwrap());
            }
            assert!(
                items
                    .iter()
                    .any(|item| item.event.kind == crate::adapter::EventKind::ToolUse)
            );
            assert!(
                items
                    .iter()
                    .any(|item| item.event.kind == crate::adapter::EventKind::ToolResult)
            );
            let raw: Vec<_> = items
                .iter()
                .filter(|item| !item.raw.is_null())
                .map(|item| &item.raw)
                .collect();
            assert_eq!(raw.len(), records.len());
            for (protected, original) in raw.into_iter().zip(&records) {
                for field in ["sessionId", "uuid", "parentUuid"] {
                    assert_eq!(protected[field], original[field]);
                }
                let content = protected["message"]["content"].to_string();
                assert!(!content.contains(NATIVE));
                assert!(!content.contains(RECORD_ID));
                if original["type"] == "assistant" {
                    assert_eq!(protected["message"]["id"], original["message"]["id"]);
                    assert_eq!(protected["message"]["content"][1]["id"], call);
                    assert!(
                        !protected["message"]["content"][1]["input"]
                            .to_string()
                            .contains(&call)
                    );
                } else if original["message"]["content"].is_array() {
                    assert_eq!(protected["message"]["content"][0]["tool_use_id"], call);
                }
                if let Some(repo) = &repo {
                    let dictionary = RepositoryDictionary::open(repo.root()).unwrap();
                    let hydrated: serde_json::Value = serde_json::from_str(
                        &dictionary
                            .hydrate_jsonl(&protected.to_string())
                            .unwrap()
                            .text,
                    )
                    .unwrap();
                    assert_eq!(&hydrated, original);
                }
            }
            session
                .on_harness_event(HarnessEvent::Delta {
                    item_id: phase.into(),
                    text: format!("{phase} delta {SECRET} {TOKEN}"),
                })
                .await;
            session
                .on_harness_event(HarnessEvent::ItemCompleted {
                    item_id: phase.into(),
                })
                .await;
            let deltas: Vec<_> = std::iter::from_fn(|| frames.try_recv().ok()).collect();
            let wire = serde_json::to_string(&deltas).unwrap();
            assert!(wire.contains(&format!("{phase} delta")));
            assert!(!wire.contains(SECRET));
            assert!(!wire.contains(TOKEN));
        }
        assert!(
            !workspace
                .root()
                .join(".git/agit/secret-dictionary")
                .exists()
        );
        let original = ["history", "live"]
            .into_iter()
            .flat_map(native_records)
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        session.driver.shutdown().await.unwrap();
    }
}
