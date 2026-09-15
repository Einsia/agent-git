//! Retry approvals address native denials retained by this driver, never caller-supplied actions.
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};

#[derive(Default)]
pub(super) struct Denials {
    entries: VecDeque<Value>,
    consumed: HashSet<String>,
    consumed_order: VecDeque<String>,
}

impl Denials {
    pub(super) fn observe(&mut self, params: &Value) {
        let Some(id) = params["reviewId"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 512)
        else {
            return;
        };
        self.entries.retain(|entry| entry["id"] != id);
        if self.consumed.contains(id) || params["review"]["status"] != "denied" {
            return;
        }
        let Some(event) = native_event(params) else {
            return;
        };
        self.entries.push_front(event);
        self.entries.truncate(10);
    }

    pub(super) fn choices(&self) -> Value {
        if self.entries.is_empty() {
            return json!({"text":"No recent auto-review denials in this conversation."});
        }
        json!({"text":"Select an action to approve one retry. The retry still goes through native auto-review.","choices":self.entries.iter().map(|event|json!({"id":event["id"],"name":summary(&event["action"]),"description":event["rationale"]})).collect::<Vec<_>>()})
    }

    pub(super) fn take(&mut self, id: &str) -> Option<Value> {
        let index = self.entries.iter().position(|event| event["id"] == id)?;
        let event = self.entries.remove(index)?;
        self.consumed.insert(id.to_string());
        self.consumed_order.push_back(id.to_string());
        while self.consumed_order.len() > 256 {
            self.consumed
                .remove(&self.consumed_order.pop_front().unwrap());
        }
        Some(event)
    }
}

fn native_event(params: &Value) -> Option<Value> {
    let review = &params["review"];
    let mut action = params["action"].clone();
    let kind = match action["type"].as_str()? {
        "command" => "command",
        "execve" => "execve",
        "writeStdin" => "write_stdin",
        "applyPatch" => "apply_patch",
        "networkAccess" => "network_access",
        "mcpToolCall" => "mcp_tool_call",
        "requestPermissions" => "request_permissions",
        _ => return None,
    };
    action["type"] = json!(kind);
    for (native, core) in [
        ("approvalId", "approval_id"),
        ("processId", "process_id"),
        ("toolName", "tool_name"),
        ("connectorId", "connector_id"),
        ("connectorName", "connector_name"),
        ("toolTitle", "tool_title"),
    ] {
        rename(&mut action, native, core);
    }
    if action["source"] == "unifiedExec" {
        action["source"] = json!("unified_exec")
    }
    match action["protocol"].as_str() {
        Some("socks5Tcp") => action["protocol"] = json!("socks5_tcp"),
        Some("socks5Udp") => action["protocol"] = json!("socks5_udp"),
        _ => {}
    }
    if kind == "write_stdin" {
        let cwd = action["cwd"].as_str()?;
        let uri = if cwd.starts_with("file:") {
            url::Url::parse(cwd).ok()?
        } else {
            url::Url::from_file_path(cwd).ok()?
        };
        action["cwd"] = json!(uri.as_str());
    }
    if kind == "request_permissions" {
        let permissions = action.get_mut("permissions")?;
        rename(permissions, "fileSystem", "file_system");
        if let Some(fs) = permissions
            .get_mut("file_system")
            .filter(|value| value.is_object())
        {
            rename(fs, "globScanMaxDepth", "glob_scan_max_depth");
            if fs["entries"].is_array() {
                fs.as_object_mut()?.remove("read");
                fs.as_object_mut()?.remove("write");
            } else {
                fs.as_object_mut()?.remove("entries");
                if fs["glob_scan_max_depth"].is_null() {
                    fs.as_object_mut()?.remove("glob_scan_max_depth");
                } else {
                    let mut entries = Vec::new();
                    for access in ["read", "write"] {
                        if let Some(paths) = fs
                            .as_object_mut()?
                            .remove(access)
                            .filter(|value| !value.is_null())
                        {
                            for path in paths.as_array()? {
                                entries.push(
                                    json!({"path":{"type":"path","path":path},"access":access}),
                                );
                            }
                        }
                    }
                    fs["entries"] = json!(entries);
                }
            }
        }
    }
    let event = json!({"id":params["reviewId"],"target_item_id":params["targetItemId"],"turn_id":params["turnId"].as_str()?,"started_at_ms":params["startedAtMs"].as_i64()?,"completed_at_ms":params["completedAtMs"].as_i64()?,"status":"denied","risk_level":review["riskLevel"],"user_authorization":review["userAuthorization"],"rationale":review["rationale"],"decision_source":params["decisionSource"],"action":action});
    (serde_json::to_vec(&event).ok()?.len() <= 128 * 1024).then_some(event)
}

fn rename(value: &mut Value, from: &str, to: &str) {
    if let Some(object) = value.as_object_mut()
        && let Some(field) = object.remove(from)
    {
        object.insert(to.to_string(), field);
    }
}

fn summary(action: &Value) -> String {
    match action["type"].as_str().unwrap_or_default() {
        "command" => action["command"].as_str().unwrap_or("Command").to_string(),
        "execve" => action["argv"]
            .as_array()
            .filter(|args| !args.is_empty())
            .and_then(|args| shlex::try_join(args.iter().filter_map(Value::as_str)).ok())
            .unwrap_or_else(|| action["program"].as_str().unwrap_or("Command").to_string()),
        "write_stdin" => format!(
            "Send input to terminal {}: {}",
            action["process_id"].as_str().unwrap_or_default(),
            action["stdin"]
                .to_string()
                .chars()
                .take(120)
                .collect::<String>()
        ),
        "apply_patch" => format!("Patch {}", action["files"]),
        "network_access" => format!(
            "Network access to {}",
            action["target"].as_str().unwrap_or_default()
        ),
        "mcp_tool_call" => format!(
            "{} on {}",
            action["tool_name"].as_str().unwrap_or("MCP tool"),
            action["server"].as_str().unwrap_or_default()
        ),
        _ => action["reason"]
            .as_str()
            .unwrap_or("Permission request")
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn notification() -> Value {
        json!({"reviewId":"review-1","threadId":"thread","turnId":"turn","startedAtMs":1,"completedAtMs":2,"decisionSource":"agent","review":{"status":"denied","riskLevel":"low","userAuthorization":"high","rationale":"Fixture denial"},"action":{"type":"command","source":"unifiedExec","command":"printf fixture","cwd":"/tmp"}})
    }
    #[test]
    fn native_denials_are_bounded_and_consumed_ids_cannot_be_replayed() {
        let mut denials = Denials::default();
        let mut event = notification();
        for id in 0..12 {
            event["reviewId"] = json!(id.to_string());
            denials.observe(&event)
        }
        assert_eq!(denials.choices()["choices"].as_array().unwrap().len(), 10);
        assert!(denials.take("0").is_none());
        assert_eq!(
            denials.take("11").unwrap()["action"]["source"],
            "unified_exec"
        );
        denials.observe(&event);
        assert!(denials.take("11").is_none());
        event["reviewId"] = json!("approved");
        event["review"]["status"] = json!("approved");
        denials.observe(&event);
        assert!(denials.take("approved").is_none());
    }
    #[test]
    fn native_stdin_and_permission_actions_preserve_their_distinct_payloads() {
        let mut event = notification();
        event["action"] = json!({"type":"writeStdin","approvalId":"child","processId":"42","stdin":"confirm\n","cwd":std::env::temp_dir()});
        let action = &native_event(&event).unwrap()["action"];
        assert_eq!(action["approval_id"], "child");
        assert_eq!(action["stdin"], "confirm\n");
        assert!(action["cwd"].as_str().unwrap().starts_with("file:"));
        event["action"] = json!({"type":"requestPermissions","reason":"Fixture","permissions":{"network":{"enabled":true},"fileSystem":{"read":["/tmp"],"write":null,"entries":null,"globScanMaxDepth":null}}});
        assert_eq!(
            native_event(&event).unwrap()["action"]["permissions"],
            json!({"network":{"enabled":true},"file_system":{"read":["/tmp"],"write":null}})
        );
    }

    #[test]
    fn native_network_enums_and_canonical_permissions_match_core_wire_shapes() {
        let mut event = notification();
        for (native, core) in [("socks5Tcp", "socks5_tcp"), ("socks5Udp", "socks5_udp")] {
            event["action"] = json!({"type":"networkAccess","target":"example.com:443","host":"example.com","protocol":native,"port":443});
            assert_eq!(native_event(&event).unwrap()["action"]["protocol"], core);
        }
        let entry = json!({"path":{"type":"glob_pattern","pattern":"/tmp/*.log"},"access":"deny"});
        event["action"] = json!({"type":"requestPermissions","permissions":{"fileSystem":{"entries":[entry],"read":["/tmp"],"write":null,"globScanMaxDepth":2}}});
        assert_eq!(
            native_event(&event).unwrap()["action"]["permissions"]["file_system"],
            json!({"entries":[entry],"glob_scan_max_depth":2})
        );
        event["action"]["permissions"]["fileSystem"]["entries"] = Value::Null;
        assert_eq!(
            native_event(&event).unwrap()["action"]["permissions"]["file_system"],
            json!({"entries":[{"path":{"type":"path","path":"/tmp"},"access":"read"}],"glob_scan_max_depth":2})
        );
    }
}
