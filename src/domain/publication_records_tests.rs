use super::*;
use serde_json::json;

const CLAUDE_MD: &str = "CANARY-PROJECT-CLAUDE-MD-7f3a";
const MEMORY_MD: &str = "CANARY-AUTO-MEMORY-MD-91c2";
const AGENTS_MD: &str = "CANARY-PROJECT-AGENTS-MD-4be8";
const GLOBAL_AGENTS_MD: &str = "CANARY-GLOBAL-CODEX-AGENTS-MD-d05e";

/// The class of `record` on its own, checked to be the same by the kind of the record and by
/// what publication keeps of it.
fn class(source: &str, record: &Value) -> RecordClass {
    let by_kind = record_class(source, record);
    assert_eq!(
        by_kind,
        class_from(published(source, record), record),
        "{source}: {record}"
    );
    by_kind
}

/// Every kept record is published in a form the Hub accepts as it is, and publishing it again
/// changes nothing.
fn assert_stable(source: &str, record: &Value) {
    if let Some(kept) = published(source, record) {
        assert_republishes(source, &kept);
    }
}

/// `kept`, a record as it was published, is published again unchanged and as conversation. A
/// published record the rule now drops fails here, rather than passing unnoticed.
fn assert_republishes(source: &str, kept: &Value) {
    assert_eq!(published(source, kept).as_ref(), Some(kept), "{kept}");
    assert_eq!(class(source, kept), RecordClass::Conversation, "{kept}");
}

/// Records of each kind a Claude Code 2.1.311 transcript holds, with the person's memory and
/// instructions where the runtime puts them.
fn claude_transcript() -> Vec<(Value, RecordClass)> {
    use RecordClass::*;
    let base = |kind: &str| {
        json!({"type":kind,"uuid":format!("u-{kind}"),"parentUuid":null,"sessionId":"s",
               "timestamp":"2026-11-03T03:00:00.000Z","cwd":"/work","version":"2.1.311"})
    };
    let with = |mut record: Value, fields: Value| {
        for (key, value) in fields.as_object().unwrap() {
            record[key] = value.clone();
        }
        record
    };
    vec![
        (
            with(
                base("user"),
                json!({"promptSource":"typed","origin":{"kind":"human"},
                       "message":{"role":"user","content":"fix the failing test"}}),
            ),
            Conversation,
        ),
        (
            with(
                base("assistant"),
                json!({"message":{"role":"assistant","content":[
                    {"type":"thinking","thinking":"look at the test first"},
                    {"type":"text","text":"Reading the test."},
                    {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/work/a.rs"}}]}}),
            ),
            Conversation,
        ),
        (
            with(
                base("user"),
                json!({"message":{"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"toolu_1","content":"fn a() {}"}]},
                       "toolUseResult":{"type":"text"}}),
            ),
            Conversation,
        ),
        (
            with(
                base("attachment"),
                json!({"attachment":{"type":"instructions","path":"/work/CLAUDE.md",
                                     "content":format!("# Rules\n{CLAUDE_MD}")}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("attachment"),
                json!({"attachment":{"type":"instructions","path":"/home/p/.claude/projects/x/memory/MEMORY.md",
                                     "content":MEMORY_MD}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("attachment"),
                json!({"attachment":{"type":"environment","cwd":"/work","platform":"darwin"}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("attachment"),
                json!({"attachment":{"type":"skill_listing","skills":["agit"]}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("attachment"),
                json!({"attachment":{"type":"hook_success","command":"./hook.sh","stdout":"ok","stderr":""}}),
            ),
            RuntimeContext,
        ),
        (
            with(base("session_context"), json!({"gitStatus":"## main"})),
            RuntimeContext,
        ),
        (
            with(
                base("prompt_snapshot"),
                json!({"systemPrompt":format!("You are Claude Code. {CLAUDE_MD} {MEMORY_MD}"),"tools":["Bash"]}),
            ),
            RequestSnapshot,
        ),
        (
            with(
                base("api-request-blob"),
                json!({"hash":"h","message":{"role":"user","content":MEMORY_MD}}),
            ),
            RequestSnapshot,
        ),
        (
            with(base("api-request"), json!({"request":{"system":CLAUDE_MD}})),
            RequestSnapshot,
        ),
        (
            with(
                base("system"),
                json!({"subtype":"turn_duration","durationMs":12}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("system"),
                json!({"subtype":"compact_boundary","logicalParentUuid":"u-user"}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("queue-operation"),
                json!({"operation":"enqueue","content":"also update the README"}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("file-history-snapshot"),
                json!({"snapshot":{"trackedFileBackups":{}}}),
            ),
            RuntimeContext,
        ),
        (
            with(base("cost-state"), json!({"costUSD":0.1})),
            RuntimeContext,
        ),
        (
            with(base("credential_org"), json!({"organization":"org"})),
            RuntimeContext,
        ),
        (
            with(base("mode"), json!({"mode":"default"})),
            RuntimeContext,
        ),
        (
            with(base("permission-mode"), json!({"permissionMode":"default"})),
            RuntimeContext,
        ),
        (with(base("atis-latch"), json!({})), RuntimeContext),
        (
            with(
                base("last-prompt"),
                json!({"prompt":"fix the failing test"}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("a-type-from-a-later-version"),
                json!({"data":CLAUDE_MD}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("user"),
                json!({"isMeta":true,"message":{"role":"user","content":format!("Contents of CLAUDE.md: {CLAUDE_MD}")}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("user"),
                json!({"promptSource":"system","message":{"role":"user","content":"Goal set: fix CI."}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("user"),
                json!({"message":{"role":"user","content":"<local-command-stdout>memory: x</local-command-stdout>"}}),
            ),
            RuntimeContext,
        ),
        (
            with(
                base("assistant"),
                json!({"isMeta":true,"message":{"role":"assistant","content":[{"type":"text","text":MEMORY_MD}]}}),
            ),
            RuntimeContext,
        ),
    ]
}

/// A Claude Code 2.1.311 transcript publishes the person's words, the replies and the tool
/// activity, and none of the runtime's records: instructions, memory, environment, hooks, the
/// system prompt and the request copies are left out, and so is a type nobody knows yet.
#[test]
fn claude_code_publishes_only_its_conversation_records() {
    for source in ["claude-code", "claude-desktop"] {
        for (record, expected) in claude_transcript() {
            assert_eq!(class(source, &record), expected, "{source}: {record}");
            assert_eq!(
                published(source, &record).is_some(),
                expected == RecordClass::Conversation,
                "{record}"
            );
            assert_stable(source, &record);
        }
    }
    let published: Vec<_> = claude_transcript()
        .iter()
        .filter_map(|(record, _)| published("claude-code", record))
        .map(|record| record.to_string())
        .collect();
    assert_eq!(published.len(), 3);
    for canary in [CLAUDE_MD, MEMORY_MD] {
        assert!(published.iter().all(|record| !record.contains(canary)));
    }
}

/// Older Claude Code versions put CLAUDE.md and the memory into `<system-reminder>` blocks of
/// the first user record. The record is published with the person's words only.
#[test]
fn injected_blocks_leave_a_user_record_before_it_is_published() {
    let reminder = |body: &str| format!("<system-reminder>\n{body}\n</system-reminder>");
    let blocks = json!({"type":"user","uuid":"u1","sessionId":"s","message":{"role":"user","content":[
        {"type":"text","text":reminder(&format!("Contents of /work/CLAUDE.md:\n{CLAUDE_MD}"))},
        {"type":"text","text":reminder(&format!("Contents of MEMORY.md:\n{MEMORY_MD}"))},
        {"type":"text","text":"rename the flag"}]}});
    assert_eq!(class("claude-code", &blocks), RecordClass::RuntimeContext);
    let kept = published("claude-code", &blocks).unwrap();
    assert_eq!(
        kept["message"]["content"],
        json!([{"type":"text","text":"rename the flag"}])
    );
    assert_stable("claude-code", &blocks);

    let inline = json!({"type":"user","uuid":"u2","sessionId":"s","message":{"role":"user",
        "content":format!("{}\nrename the flag\n{}", reminder(CLAUDE_MD), reminder(MEMORY_MD))}});
    assert_eq!(
        published("claude-code", &inline).unwrap()["message"]["content"],
        "rename the flag"
    );
    assert_stable("claude-code", &inline);
    let typed_first = json!({"type":"user","uuid":"u3","sessionId":"s","message":{"role":"user",
        "content":format!("rename the flag\n{}", reminder(MEMORY_MD))}});
    let kept = published("claude-code", &typed_first).unwrap();
    assert_eq!(kept["message"]["content"], "rename the flag");
    assert_stable("claude-code", &typed_first);

    // A block that never closes hides everything after it: the end may be the runtime's too.
    let unclosed = json!({"type":"user","message":{"role":"user",
        "content":format!("rename the flag <system-reminder>{MEMORY_MD}")}});
    assert_eq!(
        published("claude-code", &unclosed).unwrap()["message"]["content"],
        "rename the flag"
    );
    let only_context =
        json!({"type":"user","message":{"role":"user","content":reminder(CLAUDE_MD)}});
    assert_eq!(published("claude-code", &only_context), None);
}

fn codex(kind: &str, payload: Value) -> Value {
    json!({"timestamp":"2026-11-03T03:00:00.000Z","type":kind,"payload":payload})
}

fn codex_user(text: &str, kinds: Option<&[&str]>) -> Value {
    let mut payload =
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]});
    if let Some(kinds) = kinds {
        payload["internal_chat_message_metadata_passthrough"] =
            json!({"content_item_kinds": kinds});
    }
    codex("response_item", payload)
}

/// Codex files the project and global AGENTS.md and the environment context as user-role
/// messages. Only a message its provenance marks as the person's own input is published.
#[test]
fn codex_publishes_the_persons_words_by_provenance_not_by_role() {
    use RecordClass::*;
    let agents = format!(
        "# AGENTS.md instructions for /work\n<INSTRUCTIONS>\n{AGENTS_MD}\n{GLOBAL_AGENTS_MD}\n</INSTRUCTIONS>"
    );
    let cases = [
        (
            codex_user("fix the parser", Some(&["user.text"])),
            Conversation,
        ),
        (codex_user("fix the parser", None), RuntimeContext),
        (
            codex_user(&agents, Some(&["agents_md.instructions"])),
            RuntimeContext,
        ),
        (codex_user(&agents, None), RuntimeContext),
        (
            codex_user(
                "<environment_context><cwd>/work</cwd></environment_context>",
                Some(&["environments.environment_context"]),
            ),
            RuntimeContext,
        ),
        (
            codex(
                "response_item",
                json!({"type":"message","role":"user","content":[
                    {"type":"input_text","text":"fix it"},{"type":"input_text","text":agents}],
                    "internal_chat_message_metadata_passthrough":{"content_item_kinds":["user.text","agents_md.instructions"]}}),
            ),
            RuntimeContext,
        ),
        (
            codex(
                "response_item",
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions>"}]}),
            ),
            RuntimeContext,
        ),
        (
            codex(
                "session_meta",
                json!({"id":"t","cwd":"/work","base_instructions":{"text":GLOBAL_AGENTS_MD}}),
            ),
            RuntimeContext,
        ),
        (
            codex("turn_context", json!({"cwd":"/work","model":"m"})),
            RuntimeContext,
        ),
        (
            codex(
                "world_state",
                json!({"agents_md":format!("{AGENTS_MD} {GLOBAL_AGENTS_MD}")}),
            ),
            RuntimeContext,
        ),
        (
            codex("event_msg", json!({"type":"token_count","info":null})),
            RuntimeContext,
        ),
        (
            codex("event_msg", json!({"type":"thread_settings_applied"})),
            RuntimeContext,
        ),
        (
            codex(
                "response_item",
                json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done."}]}),
            ),
            Conversation,
        ),
        (
            codex(
                "response_item",
                json!({"type":"reasoning","summary":[{"type":"summary_text","text":"plan"}]}),
            ),
            Conversation,
        ),
        (
            codex(
                "response_item",
                json!({"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}),
            ),
            Conversation,
        ),
        (
            codex(
                "response_item",
                json!({"type":"function_call_output","call_id":"c1","output":"ok"}),
            ),
            Conversation,
        ),
        (
            codex(
                "event_msg",
                json!({"type":"user_message","message":"fix the parser"}),
            ),
            Conversation,
        ),
        (
            codex("event_msg", json!({"type":"task_complete"})),
            Conversation,
        ),
        (codex("a_later_record", json!({"type":"x"})), RuntimeContext),
    ];
    for (record, expected) in &cases {
        assert_eq!(class("codex", record), *expected, "{record}");
        assert_stable("codex", record);
    }
    let compacted = codex(
        "compacted",
        json!({"message":"summary so far","replacement_history":[codex_user(&agents, None)["payload"].clone()]}),
    );
    assert_eq!(class("codex", &compacted), RuntimeContext);
    let kept = published("codex", &compacted).unwrap();
    assert_eq!(kept["payload"], json!({"message":"summary so far"}));
    assert_stable("codex", &compacted);
    for (record, _) in &cases {
        if let Some(kept) = published("codex", record) {
            let text = kept.to_string();
            assert!(!text.contains(AGENTS_MD) && !text.contains(GLOBAL_AGENTS_MD));
        }
    }
}

/// Runtimes whose user records carry no marker of who wrote them publish no user messages, and
/// Cursor and WorkBuddy publish the request inside a user message's wrapper only.
#[test]
fn every_runtime_keeps_its_replies_and_tools_and_withholds_unmarked_user_messages() {
    use RecordClass::*;
    let cases = [
        (
            "cursor",
            json!({"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfix\n</user_query>"}]}}),
            Conversation,
        ),
        (
            "cursor",
            json!({"role":"user","message":{"content":[{"type":"text","text":format!("<user_info>{AGENTS_MD}</user_info>")}]}}),
            RuntimeContext,
        ),
        (
            "cursor",
            json!({"role":"assistant","message":{"content":[{"type":"text","text":"sure"}]}}),
            Conversation,
        ),
        (
            "cursor",
            json!({"type":"turn_ended","status":"success"}),
            RuntimeContext,
        ),
        (
            "opencode",
            json!({"id":"prt_1","kind":"part","message_id":"msg_1","session_id":"ses","time_created":1,
                   "data":{"type":"text","text":"a part whose message nobody has seen"}}),
            RuntimeContext,
        ),
        (
            "opencode",
            json!({"id":"prt_2","kind":"part","message_id":"msg_1","session_id":"ses","time_created":1,
                   "data":{"type":"text","synthetic":true,"text":format!("<system-reminder>{AGENTS_MD}</system-reminder>")}}),
            RuntimeContext,
        ),
        (
            "opencode",
            json!({"id":"prt_3","kind":"part","message_id":"msg_2","data":{"type":"tool","tool":"read","state":{"input":{}}}}),
            Conversation,
        ),
        (
            "opencode",
            json!({"id":"prt_4","kind":"part","message_id":"msg_2","data":{"type":"step-finish","cost":0.1}}),
            RuntimeContext,
        ),
        (
            "opencode",
            json!({"id":"ses","kind":"opencode.meta","directory":"/work"}),
            RuntimeContext,
        ),
        (
            "opencode",
            json!({"id":"msg_3","kind":"session_message","type":"user","data":{"text":"x"}}),
            Conversation,
        ),
        (
            "hermes",
            json!({"type":"hermes_message","data":{"role":"user","content":"fix","active":1}}),
            RuntimeContext,
        ),
        (
            "hermes",
            json!({"type":"hermes_message","data":{"role":"system","content":AGENTS_MD,"active":1}}),
            RuntimeContext,
        ),
        (
            "hermes",
            json!({"type":"hermes_message","data":{"role":"assistant","content":"done","active":1}}),
            Conversation,
        ),
        (
            "hermes",
            json!({"type":"hermes_message","data":{"role":"assistant","content":"old","active":0}}),
            RuntimeContext,
        ),
        (
            "hermes",
            json!({"type":"hermes_message","data":{"role":"tool","tool_call_id":"c","content":"ok","active":1}}),
            Conversation,
        ),
        (
            "openclaw",
            json!({"type":"message","message":{"role":"user","content":"fix"}}),
            RuntimeContext,
        ),
        (
            "openclaw",
            json!({"type":"message","message":{"role":"assistant","content":"done"}}),
            Conversation,
        ),
        (
            "openclaw",
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"c","content":"ok"}}),
            Conversation,
        ),
        (
            "openclaw",
            json!({"type":"custom_message","role":"user","content":AGENTS_MD}),
            RuntimeContext,
        ),
        (
            "workbuddy",
            json!({"type":"message","role":"user","content":"fix"}),
            RuntimeContext,
        ),
        (
            "workbuddy",
            json!({"type":"message","role":"user","content":"<user_query>\nfix\n</user_query>"}),
            Conversation,
        ),
        (
            "workbuddy",
            json!({"type":"message","role":"assistant","content":"done"}),
            Conversation,
        ),
        (
            "workbuddy",
            json!({"type":"function_call","callId":"c","name":"read","arguments":"{}"}),
            Conversation,
        ),
        (
            "workbuddy",
            json!({"type":"session_info","cwd":"/work"}),
            RuntimeContext,
        ),
    ];
    for (source, record, expected) in &cases {
        assert_eq!(class(source, record), *expected, "{source}: {record}");
        assert_stable(source, record);
    }
    let header = json!({"id":"msg_1","kind":"message","session_id":"ses","time_created":1,
        "data":{"role":"user","mode":"build","path":{"cwd":"/work","root":"/work"},"system":AGENTS_MD}});
    assert_eq!(class("opencode", &header), RuntimeContext);
    assert_eq!(
        published("opencode", &header).unwrap(),
        json!({"id":"msg_1","kind":"message","session_id":"ses","time_created":1,
               "data":{"role":"user","mode":"build"}})
    );
    assert_stable("opencode", &header);
    // The request inside the wrapper, without the context around it.
    let wrapped = json!({"role":"user","message":{"content":[{"type":"text","text":format!(
        "<timestamp>Monday</timestamp>\n<user_info>{AGENTS_MD}</user_info>\n<user_query>\n  refactor it \n</user_query>")}]}});
    assert_eq!(
        published("cursor", &wrapped).unwrap()["message"]["content"][0]["text"],
        "<user_query>\nrefactor it\n</user_query>"
    );
    assert_stable("cursor", &wrapped);

    // An OpenCode part takes its role from the message header before it: the assistant's text and
    // the person's unmarked text are published, a part of a message whose header is missing is
    // not.
    let mut session = Classifier::default();
    let header = |id: &str, role: &str| json!({"id":id,"kind":"message","session_id":"ses","time_created":1,"data":{"role":role}});
    let text = |id: &str, message: &str, words: &str| json!({"id":id,"kind":"part","message_id":message,"data":{"type":"text","text":words}});
    assert_eq!(
        session.class("opencode", &header("m1", "user")),
        Conversation
    );
    assert_eq!(
        session.class("opencode", &text("p1", "m1", "fix the bug")),
        Conversation
    );
    assert_eq!(
        session.class("opencode", &text("p0", "m0", "fix the bug")),
        RuntimeContext
    );
    assert_eq!(
        session.class("opencode", &header("m2", "assistant")),
        Conversation
    );
    assert_eq!(
        session.class("opencode", &text("p2", "m2", "Fixed.")),
        Conversation
    );
    let mut covered: Vec<_> = cases.iter().map(|(source, ..)| *source).collect();
    covered.extend(["claude-code", "claude-desktop", "codex", "opencode"]);
    for runtime in RUNTIMES {
        assert!(covered.contains(runtime), "{runtime} has no case");
    }
    for runtime in USER_QUERY_ONLY {
        assert!(RUNTIMES.contains(runtime));
    }
}

/// agit's own merge markers and merge summary are conversation under every runtime; a source
/// nobody registered publishes nothing.
#[test]
fn agit_markers_are_conversation_and_an_unknown_source_publishes_nothing() {
    let markers = [
        json!({"type":"system","subtype":"agit:__merge_start__","source":"b#1"}),
        json!({"type":"system","subtype":"agit:__merge_end__","source":"b#1"}),
        json!({"type":"system","subtype":"agit:__cherry_pick_start__","source":"b#1"}),
        json!({"type":"system","subtype":"agit:__cherry_pick_end__","source":"b#1"}),
        json!({"type":"system","subtype":"agit:__revert__","source":"b#1"}),
        json!({"type":"user","agit":"merge_summary","message":{"role":"user","content":"merged"}}),
    ];
    for source in RUNTIMES {
        for marker in &markers {
            assert_eq!(class(source, marker), RecordClass::Conversation, "{source}");
        }
    }
    let forged = json!({"type":"system","subtype":"agit:__merge_start__","source":"b#1","content":CLAUDE_MD});
    assert_eq!(class("claude-code", &forged), RecordClass::RuntimeContext);
    let reply = json!({"type":"assistant","message":{"role":"assistant","content":"hi"}});
    assert_eq!(class("claude-code", &reply), RecordClass::Conversation);
    assert_eq!(class("a-new-runtime", &reply), RecordClass::RuntimeContext);
    assert_eq!(
        class("claude-code", &json!("text")),
        RecordClass::RuntimeContext
    );
}

/// A kept Claude Code record whose parent is left out is linked to its nearest kept ancestor.
#[test]
fn a_published_chain_skips_the_records_it_leaves_out() {
    let records = [
        json!({"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}),
        json!({"type":"attachment","uuid":"b","parentUuid":"a","attachment":{"type":"instructions"}}),
        json!({"type":"system","uuid":"c","parentUuid":"b","subtype":"turn_duration"}),
        json!({"type":"assistant","uuid":"d","parentUuid":"c","message":{"role":"assistant","content":"hello"}}),
        json!({"type":"system","uuid":"e","parentUuid":null,"logicalParentUuid":"d","subtype":"compact_boundary"}),
        json!({"type":"user","uuid":"f","parentUuid":"e","isCompactSummary":true,"message":{"role":"user","content":"summary"}}),
    ];
    let out = published_sequence(records.iter().map(|record| ("claude-code", record)));
    let chain: Vec<_> = out
        .iter()
        .flatten()
        .map(|record| (record["uuid"].clone(), record["parentUuid"].clone()))
        .collect();
    assert_eq!(
        chain,
        [
            (json!("a"), Value::Null),
            (json!("d"), json!("a")),
            (json!("f"), Value::Null)
        ]
    );
}

/// The block rule keeps what the person wrote, pasted text included.
#[test]
fn typed_text_keeps_the_persons_words() {
    assert_eq!(typed_text("plain words").as_deref(), Some("plain words"));
    assert_eq!(
        typed_text("<pasted_content>log</pasted_content> why?").as_deref(),
        Some("<pasted_content>log</pasted_content> why?")
    );
    assert_eq!(
        typed_text("<system-reminderish> stays").as_deref(),
        Some("<system-reminderish> stays")
    );
    assert_eq!(typed_text("   "), None);
    assert_eq!(typed_text("<command-name>/clear</command-name>"), None);
}

/// A Cursor or WorkBuddy user message is published only when it holds exactly one well-formed
/// `<user_query>` block, and then only that block.
#[test]
fn a_user_query_is_published_only_from_exactly_one_well_formed_block() {
    let user = |blocks: Value| json!({"role":"user","message":{"content":blocks}});
    let text = |text: String| json!({"type":"text","text":text});
    let outside = user(json!([
        text(format!("<user_info>{AGENTS_MD}</user_info>")),
        text("<user_query>\nship it\n</user_query>".into()),
        text(format!("note {CLAUDE_MD}"))
    ]));
    let kept = published("cursor", &outside).unwrap();
    assert_eq!(
        kept["message"]["content"],
        json!([{"type":"text","text":"<user_query>\nship it\n</user_query>"}])
    );
    assert!(!kept.to_string().contains(AGENTS_MD) && !kept.to_string().contains(CLAUDE_MD));
    assert_stable("cursor", &outside);
    for shape in [
        "no block at all",
        "<user_query>one</user_query><user_query>two</user_query>",
        "<user_query>outer <user_query>inner</user_query></user_query>",
        "<user_query>never closed",
        "</user_query>closed first<user_query>",
    ] {
        assert_eq!(
            published("cursor", &user(json!([text(shape.into())]))),
            None,
            "{shape}"
        );
        for content in [json!(shape), json!([{"type":"input_text","text":shape}])] {
            let message = json!({"type":"message","role":"user","content":content});
            assert_eq!(published("workbuddy", &message), None, "{shape}");
        }
    }
    let split = user(json!([
        text("<user_query>\nfirst".into()),
        text("second</user_query>".into())
    ]));
    assert_eq!(published("cursor", &split), None);
}

/// A WorkBuddy request written as the `input_text` block WorkBuddy uses is published in that block,
/// without the context around it, and the published session reads back with the request as its
/// prompt and the turn it opens. A rule that read only `text` blocks would leave the session with
/// replies and no question.
#[test]
fn a_workbuddy_request_in_its_own_block_type_survives_publication_and_parsing() {
    use crate::adapter::{Adapter, EventKind, workbuddy::WorkBuddy};
    let request = "Build a document comparison desk.";
    let asked = json!({"type":"message","role":"user","content":[{"type":"input_text","text":format!(
        "<system-reminder>{AGENTS_MD}</system-reminder><user_query>{request}</user_query><user_info>{CLAUDE_MD}</user_info>")}]});
    let kept = published("workbuddy", &asked).unwrap();
    assert_eq!(
        kept["content"],
        json!([{"type":"input_text","text":format!("<user_query>\n{request}\n</user_query>")}])
    );
    assert_republishes("workbuddy", &kept);
    let reply = json!({"type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"Done."}],"message":{"usage":{}}});
    let reply = published("workbuddy", &reply).unwrap();
    let session = WorkBuddy.parse(&format!("{kept}\n{reply}\n")).unwrap();
    assert!(session.events.iter().any(|event| {
        event.kind == EventKind::UserPrompt
            && event
                .text
                .as_deref()
                .is_some_and(|text| text.contains(request))
    }));
    assert_eq!(crate::domain::turn::completed_count(&session), 1);
}

/// Codex's own record of a submission is published when it is the person's plain input, without
/// injected elements; an older Codex filed AGENTS.md under the same event with another kind, and
/// in a `<user_instructions>` user message.
#[test]
fn codex_submissions_are_published_only_as_the_persons_plain_input() {
    let submission = |payload: Value| codex("event_msg", payload);
    let plain =
        submission(json!({"type":"user_message","message":"fix the parser","kind":"plain"}));
    assert_eq!(class("codex", &plain), RecordClass::Conversation);
    let unkinded = submission(json!({"type":"user_message","message":"fix the parser"}));
    assert_eq!(class("codex", &unkinded), RecordClass::Conversation);
    for kind in ["user_instructions", "environment_context", "something_new"] {
        let injected = submission(json!({"type":"user_message","message":AGENTS_MD,"kind":kind}));
        assert_eq!(published("codex", &injected), None, "{kind}");
    }
    // The instructions leave a submission and the person's words stay.
    let wrapped = submission(json!({"type":"user_message",
        "message":format!("<user_instructions>\n{GLOBAL_AGENTS_MD}\n</user_instructions>\nfix it")}));
    assert_eq!(
        published("codex", &wrapped).unwrap()["payload"]["message"],
        "fix it"
    );
    assert_stable("codex", &wrapped);
    let typed_first = submission(json!({"type":"user_message",
        "message":format!("fix it\n<user_instructions>\n{GLOBAL_AGENTS_MD}\n</user_instructions>")}));
    assert_eq!(
        published("codex", &typed_first).unwrap()["payload"]["message"],
        "fix it"
    );
    assert_stable("codex", &typed_first);
    let older = codex_user(
        &format!("<user_instructions>\n{AGENTS_MD}\n</user_instructions>"),
        None,
    );
    assert_eq!(published("codex", &older), None);
}

/// Elements the runtime appends to a tool's output leave it; the output itself stays.
#[test]
fn injected_elements_leave_tool_results_too() {
    let reminder = format!("<system-reminder>\n{MEMORY_MD}\n</system-reminder>");
    let record = json!({"type":"user","uuid":"t","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_1","content":format!("file text\n{reminder}")},
        {"type":"tool_result","tool_use_id":"toolu_2","content":[
            {"type":"text","text":"out"},{"type":"text","text":reminder.clone()}]},
        {"type":"tool_result","tool_use_id":"toolu_3","content":reminder.clone()}]}});
    assert_eq!(class("claude-code", &record), RecordClass::RuntimeContext);
    let kept = published("claude-code", &record).unwrap();
    assert!(!kept.to_string().contains(MEMORY_MD), "{kept}");
    let content = &kept["message"]["content"];
    assert_eq!(content[0]["content"], "file text");
    assert_eq!(content[1]["content"], json!([{"type":"text","text":"out"}]));
    assert_eq!(content[2]["content"], "", "the call keeps an empty result");
    // A closing tag with no element open is the tool's own text.
    let quoting = json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_4","content":"grep: found </system-reminder> in parser.rs"}]}});
    assert_eq!(class("claude-code", &quoting), RecordClass::Conversation);
    assert_stable("claude-code", &record);
}

/// Resume quotes earlier records from an encrypted session's original as user messages. A quote
/// is published only when the record it quotes would be, so nothing the rule leaves out comes
/// back through a quote.
#[test]
fn a_recovered_quote_inherits_the_decision_on_the_record_it_quotes() {
    use crate::domain::transcript::recovery::{EVIDENCE_FIELD, EVIDENCE_PREFIX};
    let quote = |runtime: &str, original: Value| {
        let mut quote = json!({"type":"user","agit":"recovered_evidence","message":{"role":"user",
            "content":format!("{EVIDENCE_PREFIX}{}", json!({"runtime":runtime,"session":"source","record":original}))}});
        quote[EVIDENCE_FIELD] =
            json!({"version":1,"source":runtime,"session":"source","object_hash":"h"});
        quote
    };
    let attachment =
        json!({"type":"attachment","attachment":{"type":"instructions","content":CLAUDE_MD}});
    assert_eq!(
        published("claude-code", &quote("claude-code", attachment.clone())),
        None
    );
    let meta = json!({"type":"user","isMeta":true,"message":{"role":"user","content":MEMORY_MD}});
    assert_eq!(published("claude-code", &quote("claude-code", meta)), None);
    let agents = codex_user(AGENTS_MD, None);
    assert_eq!(published("claude-code", &quote("codex", agents)), None);
    let prompt = json!({"type":"user","message":{"role":"user","content":"fix the test"}});
    let kept = quote("claude-code", prompt);
    assert_eq!(class("claude-code", &kept), RecordClass::Conversation);
    // A quote whose record cannot be read, or that names no runtime, is left out.
    let mut unreadable = kept.clone();
    unreadable["message"]["content"] = json!(format!("{EVIDENCE_PREFIX}not json {CLAUDE_MD}"));
    assert_eq!(published("claude-code", &unreadable), None);
    let mut marked = json!({"type":"user","message":{"role":"user","content":CLAUDE_MD}});
    marked[EVIDENCE_FIELD] = json!({"version":1});
    assert_eq!(published("claude-code", &marked), None);
    let nameless = json!({"type":"user","agit":"recovered_evidence","message":{"role":"user",
        "content":format!("{EVIDENCE_PREFIX}{}", json!({"record":attachment}))}});
    assert_eq!(published("claude-code", &nameless), None);
}

/// Words the person types while a tool runs are absorbed into the turn with no user record after
/// them; that removal is published with its words only, and nothing else the queue records is.
#[test]
fn an_absorbed_follow_up_is_published_as_the_persons_words() {
    let queue = |operation: &str, reason: Option<&str>, content: &str| {
        let mut record = json!({"type":"queue-operation","operation":operation,"timestamp":"t",
            "sessionId":"s","content":content});
        if let Some(reason) = reason {
            record["reason"] = json!(reason);
        }
        record
    };
    let absorbed = queue(
        "remove",
        Some("absorbed_mid_turn"),
        "also update the README",
    );
    assert_eq!(
        published("claude-code", &absorbed).unwrap(),
        json!({"type":"queue-operation","operation":"remove","reason":"absorbed_mid_turn",
               "timestamp":"t","sessionId":"s","content":"also update the README"})
    );
    assert_stable("claude-code", &absorbed);
    let mut prompt = absorbed.clone();
    prompt["mode"] = json!("prompt");
    assert_eq!(class("claude-code", &prompt), RecordClass::Conversation);
    let mut shell = absorbed.clone();
    shell["mode"] = json!("bash");
    let mut unknown = absorbed.clone();
    unknown["attachment"] = json!(CLAUDE_MD);
    for record in [
        shell,
        unknown,
        queue("enqueue", None, "also update the README"),
        queue("dequeue", None, "also update the README"),
        queue("remove", Some("cleared"), "never mind"),
        queue(
            "remove",
            Some("absorbed_mid_turn"),
            "<task-notification>\n<task-id>x</task-id>\n</task-notification>",
        ),
        queue(
            "remove",
            Some("absorbed_mid_turn"),
            &format!("<system-reminder>{MEMORY_MD}</system-reminder>"),
        ),
    ] {
        assert_eq!(published("claude-code", &record), None, "{record}");
    }
}

/// Cursor's wake-up notices use the same wrapper as the person's requests and are left out.
#[test]
fn cursor_notices_inside_the_wrapper_are_not_published() {
    for notice in [
        "Perform any necessary follow-up actions in response to the subagent completion above.",
        "Briefly inform the user about the task result and perform any follow-up actions.",
        "You are the forked subagent; continue executing your task.",
        "Your previous response was interrupted. Continue from where you left off.",
    ] {
        let record = json!({"role":"user","message":{"content":[
            {"type":"text","text":format!("<timestamp>Monday</timestamp>\n<user_query>\n{notice}\n</user_query>")}]}});
        assert_eq!(published("cursor", &record), None, "{notice}");
    }
}

/// OpenCode's other storage shape keeps a whole message on one row: an assistant row keeps its
/// text, reasoning and tool items, a user row its text without its attached files.
#[test]
fn opencode_single_row_messages_follow_the_same_rule() {
    let reply = json!({"id":"msg_v4","kind":"session_message","session_id":"ses_v2","type":"assistant","seq":9,
        "time_created":530,"data":{"time":{"created":530,"completed":538},"model":{"id":"m"},"system":AGENTS_MD,
        "content":[{"type":"text","id":"text-0","text":"Done."},{"type":"reasoning","text":"checked"},
                   {"type":"tool","name":"read","state":{"input":{"path":"a"}}},{"type":"step-finish","cost":1}]}});
    let kept = published("opencode", &reply).unwrap();
    assert_eq!(
        kept["data"],
        json!({"time":{"created":530,"completed":538},"content":[
            {"type":"text","id":"text-0","text":"Done."},{"type":"reasoning","text":"checked"},
            {"type":"tool","name":"read","state":{"input":{"path":"a"}}}]})
    );
    assert_eq!(kept["seq"], 9);
    assert_stable("opencode", &reply);
    let user = json!({"id":"msg_v3","kind":"session_message","session_id":"ses_v2","type":"user","seq":8,
        "data":{"text":"fix it"}});
    assert_eq!(published("opencode", &user).unwrap(), user);
    assert_stable("opencode", &user);
    let other = json!({"id":"msg_v5","kind":"session_message","type":"agent-switch","data":{"agent":AGENTS_MD}});
    assert_eq!(published("opencode", &other), None);
}

/// The ordered judgement of kinds is the judgement of the records: a session's records give the
/// same classes whether each is read in full in order or reduced to its kind first. An OpenCode
/// part takes its role from the header before it in both.
#[test]
fn the_ordered_judgement_of_kinds_matches_what_publication_keeps() {
    let header = |id: &str, role: &str| json!({"id":id,"kind":"message","session_id":"ses","time_created":1,"data":{"role":role}});
    let text = |id: &str, message: &str| json!({"id":id,"kind":"part","message_id":message,"data":{"type":"text","text":"words"}});
    let mut sessions: Vec<Vec<(&str, Value)>> = vec![
        vec![
            ("opencode", header("m1", "user")),
            ("opencode", text("p1", "m1")),
            ("opencode", header("m2", "assistant")),
            ("opencode", text("p2", "m2")),
            ("opencode", text("p3", "m3")),
            ("opencode", header("m3", "assistant")),
            ("opencode", text("p4", "m3")),
            ("opencode", header("m2", "user")),
            ("opencode", text("p5", "m2")),
            (
                "opencode",
                json!({"id":"m4","kind":"message","data":{"role":"assistant","system":AGENTS_MD}}),
            ),
            ("opencode", text("p6", "m4")),
            (
                "opencode",
                json!({"id":"p7","kind":"part","message_id":"m2","data":{"type":"tool","tool":"read"}}),
            ),
            ("opencode", header("m5", "user")),
            (
                "opencode",
                json!({"id":"p8","kind":"part","message_id":"m5","data":{"type":"text","synthetic":true,"text":AGENTS_MD}}),
            ),
            (
                "opencode",
                json!({"id":"p9","kind":"part","message_id":"m5","data":{"type":"file","mime":"text/plain","filename":"a.md","url":"file:///work/a.md"}}),
            ),
            ("opencode", text("p10", "m5")),
        ],
        claude_transcript()
            .into_iter()
            .map(|(record, _)| ("claude-code", record))
            .collect(),
    ];
    sessions.push(
        sessions[0]
            .iter()
            .chain(&sessions[1])
            .map(|(source, record)| (*source, record.clone()))
            .collect(),
    );
    for records in sessions {
        let mut by_kind = Classifier::default();
        let mut by_content = Classifier::default();
        for (source, record) in &records {
            let kind = record_kind(source, record);
            assert_eq!(
                by_kind.class_of(kind),
                class_from(by_content.published(source, record), record),
                "{source}: {record}"
            );
        }
    }
    // What a reader keeps per record is small and holds nothing of the record's text.
    fn copy<T: Copy>(_: T) {}
    let kind = record_kind("opencode", &header("m1", "assistant"));
    copy(kind);
    assert!(std::mem::size_of::<RecordKind>() <= 32);
    assert!(!format!("{kind:?}").contains("m1"));
}

/// A tool result's parts are kept only as text, stripped of injected elements, or as images; a
/// part of any other shape, such as one nesting further parts, publishes nothing, and the parts
/// beside it stay.
#[test]
fn a_nested_tool_result_part_publishes_nothing() {
    let reminder = format!("<system-reminder>{MEMORY_MD}</system-reminder>");
    let record = json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_1","content":[
            {"type":"text","text":"listing"},
            {"type":"container","content":[{"type":"text","text":reminder.clone()}]},
            [{"type":"text","text":reminder.clone()}],
            {"type":"document","source":{"type":"text","data":CLAUDE_MD}},
            {"type":"image","source":{"type":"base64","data":"IMAGE"}}]}]}});
    let kept = published("claude-code", &record).unwrap();
    assert_eq!(
        kept["message"]["content"][0]["content"],
        json!([{"type":"text","text":"listing"},
               {"type":"image","source":{"type":"base64","data":"IMAGE"}}])
    );
    assert!(!kept.to_string().contains(MEMORY_MD) && !kept.to_string().contains(CLAUDE_MD));
    assert_stable("claude-code", &record);
}

/// A merge marker that names no source is kept when it is exactly an agit marker, and withheld
/// with any other field.
#[test]
fn a_sourceless_agit_marker_is_kept_only_as_it_is() {
    for subtype in [
        "agit:__merge_start__",
        "agit:__merge_end__",
        "agit:__cherry_pick_start__",
        "agit:__cherry_pick_end__",
        "agit:__revert__",
    ] {
        let marker = json!({"type":"system","subtype":subtype});
        for source in RUNTIMES {
            assert_eq!(
                class(source, &marker),
                RecordClass::Conversation,
                "{subtype}"
            );
        }
        let mut extra = marker.clone();
        extra["note"] = json!(CLAUDE_MD);
        assert_eq!(class("claude-code", &extra), RecordClass::RuntimeContext);
    }
    for marker in [
        json!({"type":"system","subtype":"agit:__other__"}),
        json!({"type":"user","subtype":"agit:__merge_start__"}),
        json!({"type":"system","subtype":"agit:__merge_start__","source":7}),
    ] {
        assert_eq!(
            class("claude-code", &marker),
            RecordClass::RuntimeContext,
            "{marker}"
        );
    }
}

/// Where Claude Code puts its reminders in a person's text, as observed: ahead of the words, after
/// them, on both sides, next to another element, and around a tool's output. Each element goes and
/// every word outside the elements stays.
#[test]
fn every_observed_reminder_placement_keeps_the_persons_words() {
    let reminder = |body: &str| format!("<system-reminder>\n{body}\n</system-reminder>");
    let prepended = format!("{}\nThen deploy to staging", reminder(MEMORY_MD));
    assert_eq!(
        typed_text(&prepended).as_deref(),
        Some("Then deploy to staging")
    );
    let part = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":prepended}]}});
    assert_eq!(
        published("claude-code", &part).unwrap()["message"]["content"],
        json!([{"type":"text","text":"Then deploy to staging"}])
    );
    let sandwich = format!(
        "{}\nrename the flag\n{}",
        reminder(CLAUDE_MD),
        reminder(MEMORY_MD)
    );
    assert_eq!(typed_text(&sandwich).as_deref(), Some("rename the flag"));
    let then_tag = format!(
        "{}\n<environment_context>\n<cwd>/work</cwd>\n</environment_context>\nship it",
        reminder(MEMORY_MD)
    );
    assert_eq!(typed_text(&then_tag).as_deref(), Some("ship it"));
    let appended = format!("fix the test\n{}", reminder(MEMORY_MD));
    assert_eq!(typed_text(&appended).as_deref(), Some("fix the test"));
    assert_eq!(typed_text(&reminder(MEMORY_MD)), None);
    let tool = |output: String| {
        let record = json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":output}]}});
        published("claude-code", &record).unwrap()["message"]["content"][0]["content"].clone()
    };
    assert_eq!(
        tool(format!("file text\n{}", reminder(MEMORY_MD))),
        "file text"
    );
    assert_eq!(
        tool(format!("{}\nfile text", reminder(MEMORY_MD))),
        "file text"
    );
    assert_eq!(tool(reminder(MEMORY_MD)), "");
    // A hook's context the runtime appends to a tool's output goes the same way.
    assert_eq!(
        tool(
            "output <user-prompt-submit-hook>CANARY-HOOK-CONTEXT-5d21</user-prompt-submit-hook>"
                .into()
        ),
        "output"
    );
    assert_eq!(
        tool("output\n<session-start-hook>CANARY-HOOK-CONTEXT-5d21</session-start-hook>".into()),
        "output"
    );
}

/// A text the rule cannot read for certain keeps only what comes before its first element: an
/// element whose body quotes a tag, a tag left over, or an element never closed.
#[test]
fn an_ambiguous_text_keeps_only_what_precedes_its_first_element() {
    assert_eq!(
        typed_text(&format!(
            "<system-reminder>quote: </system-reminder> {MEMORY_MD}</system-reminder>"
        )),
        None
    );
    assert_eq!(
        typed_text(&format!(
            "fix it <system-reminder>quote </system-reminder> {MEMORY_MD}</system-reminder>"
        ))
        .as_deref(),
        Some("fix it")
    );
    assert_eq!(
        typed_text(&format!(
            "fix it <system-reminder>instructions: <system-reminder>example</system-reminder> {MEMORY_MD}</system-reminder>"
        ))
        .as_deref(),
        Some("fix it")
    );
    assert_eq!(
        typed_text(&format!("fix it <system-reminder>{MEMORY_MD}")).as_deref(),
        Some("fix it")
    );
    // An element whose body quotes an element of its name, its own close never coming: what
    // follows the quoted close is still the runtime's.
    assert_eq!(
        typed_text(&format!(
            "fix it <system-reminder>instructions: <system-reminder>example</system-reminder> {MEMORY_MD}"
        ))
        .as_deref(),
        Some("fix it")
    );
    // A text in which no element opens is the person's or the tool's own, closing tag and all.
    let quoting = json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_4","content":"grep: found </system-reminder> in parser.rs"}]}});
    assert_eq!(class("claude-code", &quoting), RecordClass::Conversation);
}

/// A name that only begins like a runtime element does not hide the real element after it.
#[test]
fn a_look_alike_tag_does_not_hide_a_runtime_element() {
    let text = format!(
        "Explain <system-reminderish> syntax. <system-reminder>{MEMORY_MD}</system-reminder>"
    );
    assert_eq!(
        typed_text(&text).as_deref(),
        Some("Explain <system-reminderish> syntax.")
    );
    let record = json!({"type":"user","message":{"role":"user","content":text}});
    assert_eq!(class("claude-code", &record), RecordClass::RuntimeContext);
    assert!(
        !published("claude-code", &record)
            .unwrap()
            .to_string()
            .contains(MEMORY_MD)
    );
}

/// Codex's usage: a turn's token counts as numbers and the model it ran on are published; the
/// account's rate limits, the turn's working directory, instructions and policies never are.
#[test]
fn codex_usage_publishes_counts_and_the_model_only() {
    let usage = codex(
        "event_msg",
        json!({"type":"token_count",
            "info":{
                "total_token_usage":{"input_tokens":1200,"cached_input_tokens":800,"output_tokens":90,
                    "reasoning_output_tokens":40,"total_tokens":1290,"note":CLAUDE_MD},
                "last_token_usage":{"input_tokens":"CANARY-NOT-A-NUMBER-7c1d","output_tokens":10},
                "model_context_window":272000,
                "extra":{"text":MEMORY_MD}},
            "rate_limits":{"primary":{"used_percent":12.5,"resets_at":"CANARY-RATE-LIMIT-9e04"}}}),
    );
    let kept = published("codex", &usage).unwrap();
    assert_eq!(
        kept,
        json!({"timestamp":"2026-11-03T03:00:00.000Z","type":"event_msg","payload":{"type":"token_count",
            "info":{"total_token_usage":{"input_tokens":1200,"cached_input_tokens":800,"output_tokens":90,
                "reasoning_output_tokens":40,"total_tokens":1290},
                "last_token_usage":{"output_tokens":10},"model_context_window":272000}}})
    );
    assert_stable("codex", &usage);
    let empty = codex(
        "event_msg",
        json!({"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":1}}}),
    );
    assert_eq!(published("codex", &empty), None);

    let context = codex(
        "turn_context",
        json!({"cwd":"/home/person/CANARY-TURN-CWD-41aa","model":"gpt-5.5-codex",
            "user_instructions":AGENTS_MD,"approval_policy":"never","sandbox_policy":{"mode":"workspace-write"}}),
    );
    assert_eq!(
        published("codex", &context).unwrap(),
        json!({"timestamp":"2026-11-03T03:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5.5-codex"}})
    );
    assert_stable("codex", &context);
    for model in [
        json!(format!("Follow {AGENTS_MD} first")),
        json!("/Users/canary/m"),
        json!("~/models/m"),
        json!("./m"),
        json!("models/../m"),
        json!("C:/Users/canary/private-model"),
        json!("c:\\Users\\canary\\m"),
        json!("//host/share/m"),
        json!("\\\\host\\share\\m"),
        json!("file:///Users/canary/m"),
        json!("a model with spaces"),
        json!(7),
        json!("x".repeat(101)),
    ] {
        let context = codex("turn_context", json!({"cwd":"/work","model":model}));
        assert_eq!(published("codex", &context), None, "{model}");
    }
}

/// A tool output a Codex-shaped result writes as `input_text` or `output_text` parts is text:
/// published through the same block rule, so an environment block inside it is stripped. An
/// `input_image` part, which can name a local file, is not.
#[test]
fn codex_text_parts_of_a_tool_result_are_published_as_text() {
    let record = json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_1","content":[
            {"type":"output_text","text":format!(
                "build passed\n<environment_context>\n<cwd>/home/person/{AGENTS_MD}</cwd>\n</environment_context>")},
            {"type":"input_text","text":"see log"},
            {"type":"input_image","image_url":"file:///Users/canary/screen.png"}]}]}});
    let kept = published("claude-code", &record).unwrap();
    assert_eq!(
        kept["message"]["content"][0]["content"],
        json!([{"type":"output_text","text":"build passed"},{"type":"input_text","text":"see log"}])
    );
    assert!(!kept.to_string().contains(AGENTS_MD));
    assert!(!kept.to_string().contains("canary"));
    assert_stable("claude-code", &record);
}

/// OpenCode 1.19.4 writes what it adds to the person's turn as text parts marked `synthetic`, and
/// what the person typed as an unmarked text part. Of the person's message only that typed text
/// is published, without injected elements; marked parts, parts with any other field and attached
/// files are not, nor a v2 user row's files.
#[test]
fn opencode_publishes_only_the_text_the_person_typed() {
    use RecordClass::*;
    let mut session = Classifier::default();
    let header = json!({"id":"m1","kind":"message","session_id":"ses","time_created":1,
        "data":{"role":"user","agent":"build","model":{"providerID":"p","modelID":"m"},"time":{"created":1}}});
    assert!(session.published("opencode", &header).is_some());
    let part = |id: &str, data: Value| json!({"id":id,"kind":"part","session_id":"ses","message_id":"m1","time_created":1,"data":data});
    // `opencode run -f README.md`: the read call and the file body as marked text, the file
    // itself as an unmarked file part, then the typed prompt.
    let withheld = [
        part(
            "p1",
            json!({"type":"text","synthetic":true,"text":"Called the Read tool with the following input: {\"filePath\":\"/Users/canary/README.md\"}"}),
        ),
        part(
            "p2",
            json!({"type":"text","synthetic":true,"text":format!("<path>/Users/canary/README.md</path>\n<content>{AGENTS_MD}</content>")}),
        ),
        part(
            "p3",
            json!({"type":"file","mime":"text/plain","filename":"README.md","url":"file:///Users/canary/README.md"}),
        ),
        // `-f shot.png`: the image's file part is marked.
        part(
            "p4",
            json!({"type":"file","synthetic":true,"mime":"image/png","filename":"shot.png","url":"data:image/png;base64,AAAA"}),
        ),
        // A mark of either value, or any field nobody knows, withholds the part.
        part(
            "p5",
            json!({"type":"text","synthetic":false,"text":"maybe typed"}),
        ),
        part(
            "p6",
            json!({"type":"text","ignored":true,"text":"maybe typed"}),
        ),
        part(
            "p7",
            json!({"type":"text","text":"maybe typed","metadata":{"plugin":"x"}}),
        ),
        // Nothing typed is left once the injected element is removed.
        part(
            "p8",
            json!({"type":"text","text":format!("<system-reminder>{MEMORY_MD}</system-reminder>")}),
        ),
    ];
    for record in &withheld {
        assert_eq!(session.published("opencode", record), None, "{record}");
    }
    let typed = part(
        "p9",
        json!({"type":"text","text":"\"summarize the readme\""}),
    );
    assert_eq!(session.published("opencode", &typed).unwrap(), typed);
    let reminded = part(
        "p10",
        json!({"type":"text","text":format!("explain this <system-reminder>{MEMORY_MD}</system-reminder>")}),
    );
    assert_eq!(
        session.published("opencode", &reminded).unwrap()["data"]["text"],
        "explain this"
    );
    let mut kinds = Classifier::default();
    kinds.class("opencode", &header);
    assert_eq!(kinds.class("opencode", &typed), Conversation);
    assert_eq!(kinds.class("opencode", &reminded), RuntimeContext);
    for record in &withheld {
        assert_eq!(kinds.class("opencode", record), RuntimeContext, "{record}");
    }

    // The v2 layout: a user row holds what was sent and its attached files.
    let row = json!({"id":"msg_v8","kind":"session_message","session_id":"ses_v2","type":"user","seq":8,
        "time_created":2,"data":{"time":{"created":2},"text":"look at this file",
        "files":[{"uri":"file:///Users/canary/notes.md","mime":"text/markdown","name":"notes.md"}]}});
    let kept = published("opencode", &row).unwrap();
    assert_eq!(
        kept["data"],
        json!({"time":{"created":2},"text":"look at this file"})
    );
    assert!(!kept.to_string().contains("canary"));
    assert_stable("opencode", &row);
    let flagged = json!({"id":"msg_v9","kind":"session_message","type":"user","seq":9,
        "data":{"text":"maybe typed","synthetic":true}});
    assert_eq!(published("opencode", &flagged), None);
    let system = json!({"id":"msg_v1","kind":"session_message","type":"system","seq":1,"data":{"text":AGENTS_MD}});
    assert_eq!(published("opencode", &system), None);
}

/// Claude Code 2.1.311 writes a pasted image and its caption as the person's typed record: one
/// text part with the `[Image #N]` placeholder and the caption, then the image. A companion
/// record the runtime writes after it names the pasted file and is not published.
#[test]
fn a_pasted_image_caption_is_published_from_the_typed_record() {
    let typed = json!({"parentUuid":"p0","isSidechain":false,"promptId":"q1","type":"user",
        "message":{"role":"user","content":[
            {"type":"text","text":"[Image #1] Here is the screenshot I mentioned."},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"IMAGE"}}]},
        "uuid":"u1","timestamp":"t","imagePasteIds":[1],"permissionMode":"default",
        "origin":{"kind":"human"},"promptSource":"typed","turnOrigin":"human",
        "turnPosition":{"promptIndex":1,"turnIndex":1},"userType":"external","entrypoint":"cli",
        "cwd":"/work","sessionId":"s","version":"2.1.311","gitBranch":"main"});
    let kept = published("claude-code", &typed).unwrap();
    assert_eq!(
        kept["message"]["content"],
        json!([{"type":"text","text":"[Image #1] Here is the screenshot I mentioned."},
               {"type":"image","source":{"type":"base64","media_type":"image/png","data":"IMAGE"}}])
    );
    assert_stable("claude-code", &typed);
    let companion = json!({"parentUuid":"u1","isSidechain":false,"promptId":"q1","type":"user",
        "message":{"role":"user","content":[
            {"type":"text","text":"[Image: source: /Users/canary/Desktop/screenshot.png]"}]},
        "isMeta":true,"turnCompanion":true,"uuid":"u2","timestamp":"t","userType":"external",
        "entrypoint":"cli","cwd":"/work","sessionId":"s","version":"2.1.311","gitBranch":"main"});
    assert_eq!(published("claude-code", &companion), None);
    assert_eq!(
        record_class("claude-code", &companion),
        RecordClass::RuntimeContext
    );
}

#[test]
fn codex_tool_outputs_strip_injected_elements() {
    // Codex tool output text follows the block rule of Claude tool results: an environment block
    // and a reminder both go, the tool's own words stay; parts other than text are dropped.
    let injected = format!(
        "<environment_context>{AGENTS_MD}</environment_context>\n<system-reminder>{MEMORY_MD}</system-reminder>"
    );
    for kind in [
        "function_call_output",
        "custom_tool_call_output",
        "local_shell_call_output",
    ] {
        let text_parts = |text: &str| {
            ["text", "input_text", "output_text"].map(|kind| json!({"type":kind,"text":text}))
        };
        let text = format!("build passed\n{injected}");
        let mut parts = Vec::from(text_parts(&text));
        parts.extend([
            json!({"type":"output_text","text":injected}),
            json!({"type":"image","data":AGENTS_MD}),
            json!({"type":"input_image","image_url":AGENTS_MD}),
            json!({"type":"nested","content":AGENTS_MD}),
        ]);
        for (output, expected) in [
            (json!(text), json!("build passed")),
            (json!(injected), json!("")),
            (json!(parts), json!(text_parts("build passed"))),
        ] {
            let record = codex(
                "response_item",
                json!({"type":kind,"call_id":"call-1","output":output}),
            );
            let mut expected_record = record.clone();
            expected_record["payload"]["output"] = expected;
            let kept = published("codex", &record);
            assert_eq!(kept, Some(expected_record), "{kind}");
            let kept = kept.unwrap().to_string();
            assert!(
                !kept.contains(AGENTS_MD) && !kept.contains(MEMORY_MD),
                "{kept}"
            );
            assert_eq!(class("codex", &record), RecordClass::RuntimeContext);
            assert_stable("codex", &record);
        }
    }
}

/// A tool output that is neither text nor parts, a structured result or a scalar, goes through the
/// one rule for Claude and Codex alike: it is kept as it is, for protection to mask its credential
/// fields and keep its references.
#[test]
fn a_tool_output_of_another_shape_follows_one_rule_for_claude_and_codex() {
    for output in [
        json!({"output":"STRUCTURED_STDOUT","metadata":{"exit_code":0}}),
        json!({"text":"NESTED_TEXT"}),
        json!(42),
        json!(true),
        Value::Null,
    ] {
        let record = codex(
            "response_item",
            json!({"type":"function_call_output","call_id":"call-1","output":output}),
        );
        let kept = published("codex", &record).unwrap();
        assert_eq!(kept["payload"]["output"], output, "{record}");
        assert_eq!(kept["payload"]["call_id"], "call-1");
        assert_stable("codex", &record);
        let result = json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":output}]}});
        let kept = published("claude-code", &result).unwrap();
        assert_eq!(kept["message"]["content"][0]["content"], output, "{result}");
        assert_stable("claude-code", &result);
    }
}

/// Real Hermes v0.22.5 rows (a session with a file mention and a compaction, canary words only).
const HERMES_ROWS: &str =
    include_str!("../../tests/fixtures/runtime-samples/hermes-0.22.5-messages.jsonl");
/// Real OpenClaw 2026.10.9 transcript events from the same kind of session.
const OPENCLAW_EVENTS: &str =
    include_str!("../../tests/fixtures/runtime-samples/openclaw-2026.10.9-events.jsonl");

fn sample(text: &str) -> Vec<Value> {
    text.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn published_user_texts(
    source: &str,
    records: &[Value],
    text: fn(&Value) -> &Value,
) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| {
            assert_stable(source, record);
            published(source, record)
        })
        .filter_map(|kept| text(&kept).as_str().map(str::to_owned))
        .collect()
}

/// `row` as it publishes in a Hermes session from `source`, judged after that session's header.
fn hermes_in(header: &Value, source: &str, row: &Value) -> Option<Value> {
    let mut header = header.clone();
    header["data"]["source"] = json!(source);
    let mut classifier = Classifier::default();
    assert_eq!(classifier.published("hermes", &header), None);
    classifier.published("hermes", row)
}

/// Hermes publishes the person's own words, in a session from the owner's own console as Hermes
/// records it: a user row the gateway took in, with the platform's id and nothing Hermes adds. The
/// file text Hermes appends to a mentioning row stays out, and so does its compaction summary; a
/// row with any other field set is withheld. On any other platform no user row publishes.
#[test]
fn hermes_publishes_only_the_words_the_person_typed() {
    let records = sample(HERMES_ROWS);
    let (header, rows) = records.split_first().unwrap();
    assert_eq!(header["data"]["source"], "local");
    let in_order: Vec<(&str, &Value)> = records.iter().map(|record| ("hermes", record)).collect();
    let typed: Vec<String> = published_sequence(in_order.clone())
        .into_iter()
        .flatten()
        .filter(|kept| kept["data"]["role"] == "user")
        .map(|kept| kept["data"]["content"].as_str().unwrap().to_owned())
        .collect();
    // Rows a compaction replaced are inactive and stay out, as before; their copies publish.
    assert_eq!(
        typed,
        [
            "CANARY-HERMES-10 a fourth prompt",
            "CANARY-HERMES-11 a prompt after compression"
        ]
    );
    // Each published row, the person's two included, is published again unchanged and as
    // conversation, on its own and without its session's header.
    let mut typed_rows = 0;
    for kept in published_sequence(in_order).into_iter().flatten() {
        assert_republishes("hermes", &kept);
        typed_rows += usize::from(kept["data"]["role"] == "user");
        let out = kept.to_string();
        assert!(!out.contains("README-1"), "{out}");
        assert!(!out.contains("CONTEXT COMPACTION"), "{out}");
        if kept["data"]["role"] == "user" {
            assert!(kept["data"].get("platform_message_id").is_none(), "{out}");
        }
    }
    assert_eq!(typed_rows, 2);
    // The file mention, in a session not yet compacted: the typed words only.
    let mut mention = rows
        .iter()
        .find(|row| {
            row["data"]["content"]
                .as_str()
                .is_some_and(|c| c.contains("@file:"))
        })
        .unwrap()
        .clone();
    mention["data"]["active"] = json!(1);
    let kept = hermes_in(header, "local", &mention).unwrap();
    assert_eq!(
        kept["data"]["content"],
        "CANARY-HERMES-8 please look at @file:README.md and tell me what it says"
    );
    assert_republishes("hermes", &kept);
    // Only the one attachment shape the samples show is cut off; anything else after the marker,
    // or a column the rule doesn't know, withholds the whole row.
    let content = mention["data"]["content"].as_str().unwrap().to_owned();
    let (typed_part, _) = content
        .split_once("\n\n--- Attached Context ---\n\n")
        .unwrap();
    for attached in [
        "🖼️ @image:photo.png (12 tokens)\n```\nIMAGE_DATA\n```",
        "🔗 @url:https://example.test (40 tokens)\n```\nPAGE_TEXT\n```",
        "📁 @folder:src (30 tokens)\n```\nFOLDER_LIST\n```",
        "📄 @file:README.md (9 tokens)\n```markdown\nReadme line.\n```\nTRAILING_TEXT",
        "📄 @file:README.md (9 tokens)\nno fence",
    ] {
        let mut other = mention.clone();
        other["data"]["content"] = json!(format!(
            "{typed_part}\n\n--- Attached Context ---\n\n{attached}"
        ));
        assert_eq!(hermes_in(header, "local", &other), None, "{attached}");
    }
    let mut unknown = mention.clone();
    unknown["data"]["attachments"] = json!(["photo.png"]);
    assert_eq!(hermes_in(header, "local", &unknown), None);
    unknown["data"]["attachments"] = Value::Null;
    assert!(
        hermes_in(header, "local", &unknown).is_some(),
        "an unset column changes nothing"
    );
    let typed = rows
        .iter()
        .find(|row| row["data"]["content"] == "CANARY-HERMES-11 a prompt after compression")
        .unwrap();
    assert!(hermes_in(header, "local", typed).is_some());
    // Another platform's session, or one judged without its header: no user row publishes, while
    // the replies still do.
    for source in ["telegram", "discord", "slack", "whatsapp", "email", "cli"] {
        assert_eq!(hermes_in(header, source, typed), None, "{source}");
    }
    assert_eq!(published("hermes", typed), None);
    let reply = rows
        .iter()
        .find(|row| row["data"]["role"] == "assistant" && row["data"]["active"] == 1)
        .unwrap();
    assert!(hermes_in(header, "telegram", reply).is_some());
    for (field, value) in [
        ("platform_message_id", Value::Null),
        ("_compressed_summary", json!(1)),
        ("api_content", json!("injected")),
        ("tool_calls", json!("[]")),
        ("display_kind", json!("notice")),
        ("session_source", json!("telegram")),
    ] {
        let mut other = typed.clone();
        other["data"][field] = value;
        assert_eq!(hermes_in(header, "local", &other), None, "{field}");
    }
}

/// OpenClaw publishes the person's own words: a user message with string content marked as the
/// owner's and keyed as a user message, and nothing else. Its compaction and the context events it
/// supplies stay out; a message with any other field or shape is withheld.
#[test]
fn openclaw_publishes_only_the_words_the_person_typed() {
    let events = sample(OPENCLAW_EVENTS);
    fn user(kept: &Value) -> &Value {
        if kept["message"]["role"] == "user" {
            &kept["message"]["content"]
        } else {
            &Value::Null
        }
    }
    let texts = published_user_texts("openclaw", &events, user);
    assert_eq!(
        texts,
        [
            "CANARY-OPENCLAW-7 please list the files",
            "CANARY-OPENCLAW-8 please look at README.md and tell me what it says",
            "CANARY-OPENCLAW-9 a third prompt",
            "CANARY-OPENCLAW-10 a prompt after compaction",
            "CANARY-OPENCLAW-11 a prompt after the gateway compaction",
        ]
    );
    for event in &events {
        if event["type"] != "message" {
            assert_eq!(published("openclaw", event), None, "{event}");
        }
    }
    let typed = events
        .iter()
        .find(|event| event["message"]["content"] == "CANARY-OPENCLAW-9 a third prompt")
        .unwrap();
    for (field, value) in [
        ("__openclaw", json!({"senderIsOwner": false})),
        ("idempotencyKey", json!("k:system")),
        (
            "content",
            json!([{"type":"text","text":"CANARY-OPENCLAW-9 a third prompt"}]),
        ),
        ("attachments", json!([])),
    ] {
        let mut other = typed.clone();
        other["message"][field] = value;
        assert_eq!(published("openclaw", &other), None, "{field}");
    }
}
