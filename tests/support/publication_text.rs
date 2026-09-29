use agit::domain::{privacy::ReplacementRule, storage};
use serde_json::{Value, json};
use std::path::Path;

pub const SECRET: &str = "PRIVATE_VALUE\n\"quoted\"";

pub fn replacement() -> ReplacementRule {
    ReplacementRule {
        pattern: r#"PRIVATE_VALUE\n"quoted""#.into(),
        replacement: "MASKED_VALUE".into(),
        regex: true,
    }
}

pub fn records(workspace: &Path) -> Vec<Value> {
    let nested = json!({"text":"NESTED_TEXT","secret":SECRET,"path":workspace.join("src/main.rs")});
    vec![
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"fixture-read","name":"Read","input":{"file_path":workspace.join("src/main.rs")}}
        ]}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"fixture-read","content":{"stdout":"STRUCTURED_STDOUT","exit_code":0,"nested":nested}}
        ]}}),
        json!({"type":"future_record","label":"UNKNOWN_RECORD","nested":nested,
            "payload":{"type":"image","source":{"type":"base64","data":"NESTED_IMAGE_BODY"}}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"PUBLIC_MESSAGE"},
            "RAW_BLOCK",
            {"type":"future_text","text":"UNKNOWN_BLOCK","nested":nested}
        ]}}),
        json!({"type":"attachment","attachment":{"type":"skill_listing","content":"ATTACHMENT_SKILL_BODY"}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"fixture-read","content":[
                {"type":"text","text":"MIXED_TEXT_BEFORE"},
                {"type":"image","source":{"type":"base64","data":"MIXED_IMAGE_BODY"}},
                {"type":"file","file":{"file_data":"PRIVATE_FILE_BODY"}},
                {"type":"document","source":{"type":"text","data":"PRIVATE_DOCUMENT_BODY"}},
                {"type":"audio","source":{"type":"base64","data":"PRIVATE_AUDIO_BODY"}},
                {"type":"text","text":"MIXED_TEXT_AFTER"},
                {"type":"future_text","nested":nested}
            ]}
        ]}}),
        json!({"type":"future_record","text":"ATTACHMENT_SIBLING_TEXT",
            "attachments":[{"content":"UNTYPED_ATTACHMENT_BODY"}]}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"fixture-read","content":{
                "status":200,"mime_type":"application/json","body":{"message":"HTTP_JSON_METADATA","nested":nested},
                "headers":{"mimeType":"application/json","media_type":"text/plain"}
            }}
        ]}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"fixture-read","content":[
                {"type":"file","name":"FILE_LIST","path":workspace.join("src/main.rs"),"secret":SECRET},
                {"type":"directory","name":"src","size":0}
            ]}
        ]}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"fixture-read","content":["SCALAR_ARRAY",7,true,null,nested,[SECRET]]}
        ]}}),
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"tool_use","id":"fixture-shell","name":"Bash","input":{"command":"SHELL_COMMAND"}},
            {"type":"tool_use","id":"fixture-mcp","name":"mcp__files__read_file","input":{"path":workspace.join("src/main.rs")}}
        ]}}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"fixture-shell","content":"PRIVATE_SHELL_RESULT"},
            {"type":"tool_result","tool_use_id":"fixture-mcp","content":"PRIVATE_MCP_RESULT"}
        ]}}),
    ]
}

pub fn assert_public(public: &Value, workspace: &Path) {
    let log = public["session"]["log"].as_str().unwrap();
    for marker in [
        "STRUCTURED_STDOUT",
        "PUBLIC_MESSAGE",
        "MIXED_TEXT_BEFORE",
        "MIXED_TEXT_AFTER",
        "NESTED_TEXT",
        "MASKED_VALUE",
        "<workspace>/src/main.rs",
    ] {
        assert!(log.contains(marker), "missing public content: {marker}");
    }
    let text = public.to_string();
    for marker in [
        "PRIVATE_VALUE",
        "SHELL_COMMAND",
        "PRIVATE_SHELL_RESULT",
        "PRIVATE_MCP_RESULT",
        "UNKNOWN_RECORD",
        "RAW_BLOCK",
        "UNKNOWN_BLOCK",
        "ATTACHMENT_SIBLING_TEXT",
        "ATTACHMENT_SKILL_BODY",
        "NESTED_IMAGE_BODY",
        "MIXED_IMAGE_BODY",
        "PRIVATE_FILE_BODY",
        "PRIVATE_DOCUMENT_BODY",
        "PRIVATE_AUDIO_BODY",
        "UNTYPED_ATTACHMENT_BODY",
        workspace.to_str().unwrap(),
    ] {
        assert!(!text.contains(marker), "private content escaped: {marker}");
    }
    let records = storage::parse_envelopes(log).unwrap();
    let result = |marker: &str| {
        records
            .iter()
            .flat_map(|record| record.content["message"]["content"].as_array().unwrap())
            .find_map(|block| {
                let content = block["content"].as_str()?;
                content
                    .contains(marker)
                    .then(|| serde_json::from_str::<Value>(content).unwrap())
            })
            .unwrap_or_else(|| panic!("missing structured result: {marker}"))
    };
    let structured = result("STRUCTURED_STDOUT");
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["nested"]["secret"], "MASKED_VALUE");
    assert_eq!(structured["nested"]["path"], "<workspace>/src/main.rs");
    let nested =
        json!({"text":"NESTED_TEXT","secret":"MASKED_VALUE","path":"<workspace>/src/main.rs"});
    assert_eq!(
        result("HTTP_JSON_METADATA"),
        json!({
            "status":200,"mime_type":"application/json","body":{"message":"HTTP_JSON_METADATA","nested":nested},
            "headers":{"mimeType":"application/json","media_type":"text/plain"}
        })
    );
    assert_eq!(
        result("FILE_LIST"),
        json!([
            {"type":"file","name":"FILE_LIST","path":"<workspace>/src/main.rs","secret":"MASKED_VALUE"},
            {"type":"directory","name":"src","size":0}
        ])
    );
    assert_eq!(
        result("SCALAR_ARRAY"),
        json!(["SCALAR_ARRAY", 7, true, null, nested, ["MASKED_VALUE"]])
    );
    if let Some(report) = public.get("report") {
        let omissions = report["omissions"].as_array().unwrap();
        assert!(
            omissions
                .iter()
                .filter(
                    |notice| notice["reason"] == "attachment body is excluded from public content"
                )
                .count()
                >= 4
        );
    }
}
