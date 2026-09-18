//! CLI metadata is checked against explicit policies; new arguments cannot silently emit values.

use clap::{ArgMatches, Command};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, ffi::OsString, sync::LazyLock};

pub const REGISTRY_JSON: &str = include_str!("registry.json");

#[derive(Deserialize)]
struct Registry {
    globals: BTreeMap<String, Field>,
    commands: BTreeMap<String, CommandPolicy>,
    rc_methods: BTreeMap<String, String>,
    mcp_tools: BTreeMap<String, Vec<String>>,
}

#[derive(Deserialize)]
struct CommandPolicy {
    #[allow(dead_code)]
    aliases: Vec<String>,
    args: BTreeMap<String, Field>,
}

#[derive(Deserialize)]
struct Field {
    #[allow(dead_code)]
    action: String,
    policy: String,
    #[serde(default)]
    values: Vec<String>,
}

static REGISTRY: LazyLock<Registry> =
    LazyLock::new(|| serde_json::from_str(REGISTRY_JSON).expect("valid telemetry registry"));

pub fn bucket(value: u64) -> &'static str {
    match value {
        0 => "0",
        1 => "1",
        2..=5 => "2-5",
        6..=20 => "6-20",
        21..=100 => "21-100",
        101..=1000 => "101-1000",
        _ => "1001+",
    }
}

fn fields(matches: &ArgMatches, policy: &BTreeMap<String, Field>, out: &mut Map<String, Value>) {
    for (id, field) in policy {
        let Ok(raw) = matches.try_get_raw(id) else {
            continue;
        };
        let provided = matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine);
        let values = raw.map(|raw| raw.collect::<Vec<_>>()).unwrap_or_default();
        let value = match field.policy.as_str() {
            "boolean" => json!(values.first().is_some_and(|v| *v == "true")),
            "enum" => {
                if values.is_empty() {
                    continue;
                }
                let mut safe = values
                    .iter()
                    .map(|raw| {
                        field
                            .values
                            .iter()
                            .find(|value| *raw == std::ffi::OsStr::new(value))
                            .map(String::as_str)
                            .unwrap_or("other")
                    })
                    .collect::<Vec<_>>();
                safe.sort_unstable();
                safe.dedup();
                if field.action == "Append" {
                    json!(safe)
                } else {
                    json!(safe[0])
                }
            }
            "bucket" => {
                let Some(raw) = values.first() else { continue };
                json!(
                    raw.to_str()
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(bucket)
                        .unwrap_or("invalid")
                )
            }
            "count" => json!(bucket(values.len() as u64)),
            "presence" => json!(provided),
            _ => continue,
        };
        out.insert(format!("arg_{id}"), value);
    }
}

/// Even rejected argv is traversed only through known metadata; raw errors never enter properties.
pub fn properties(argv: &[OsString]) -> Map<String, Value> {
    let mut out = Map::new();
    let mut definition = crate::commands::cli_def().ignore_errors(true);
    let Ok(matches) = definition.try_get_matches_from_mut(argv) else {
        out.insert("command".into(), json!("unparsed"));
        return out;
    };
    let mut current = &matches;
    let mut path = String::new();
    fields(current, &REGISTRY.globals, &mut out);
    while let Some((name, child)) = current.subcommand() {
        let next = format!("{path} {name}").trim().to_owned();
        let Some(policy) = REGISTRY.commands.get(&next) else {
            break;
        };
        fields(child, &REGISTRY.globals, &mut out);
        fields(child, &policy.args, &mut out);
        path = next;
        current = child;
    }
    let mut components = path.split_whitespace();
    if path == "config" {
        let key = current
            .try_get_one::<String>("key")
            .ok()
            .flatten()
            .map(String::as_str);
        let allowed: &[&str] = match key {
            Some("push.auto" | "commit.auto") => &["true", "false"],
            Some("push.visibility") => &["ask", "private", "public"],
            Some("memory.track") => &["session", "off"],
            Some("secrets.keystore") => &["os", "file"],
            Some("runtime.default") => &[
                "claude-code",
                "codex",
                "cursor",
                "opencode",
                "claude-desktop",
                "openclaw",
                "hermes",
                "workbuddy",
            ],
            _ => &[],
        };
        if !allowed.is_empty()
            && let Some(value) = current.try_get_one::<String>("value").ok().flatten()
        {
            out.insert(
                "config_value".into(),
                json!(
                    allowed
                        .iter()
                        .find(|allowed| **allowed == value)
                        .copied()
                        .unwrap_or("other")
                ),
            );
        }
    }
    out.insert("command".into(), json!(components.next().unwrap_or("bare")));
    out.insert(
        "subcommand".into(),
        json!(components.collect::<Vec<_>>().join(" ")),
    );
    out.insert("command_path".into(), json!(path));
    out
}

pub(crate) fn rc_method(name: &str) -> Option<&'static str> {
    match REGISTRY.rc_methods.get_key_value(name) {
        Some((name, policy)) if policy == "capture" => Some(name.as_str()),
        Some(_) => None,
        None => Some("unknown"),
    }
}

pub(crate) fn captured_rc_methods() -> impl Iterator<Item = &'static str> {
    REGISTRY
        .rc_methods
        .iter()
        .filter(|(_, policy)| policy.as_str() == "capture")
        .map(|(name, _)| name.as_str())
        .chain(std::iter::once("unknown"))
}

pub(crate) fn mcp_tool(name: &str) -> &'static str {
    REGISTRY
        .mcp_tools
        .get_key_value(name)
        .map(|(name, _)| name.as_str())
        .unwrap_or("unknown")
}

#[cfg(test)]
pub(crate) fn validate_mcp_tools(tools: &Value) -> bool {
    let Some(tools) = tools.as_array() else {
        return false;
    };
    let actual: BTreeMap<_, _> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_owned();
            let keys = tool
                .pointer("/inputSchema/properties")?
                .as_object()?
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            Some((name, keys))
        })
        .collect();
    actual == REGISTRY.mcp_tools
}

fn validate_command(command: &Command, path: &str, seen: &mut Vec<String>) -> Result<(), String> {
    let policy = REGISTRY
        .commands
        .get(path)
        .ok_or_else(|| format!("unclassified command: {path}"))?;
    seen.push(path.into());
    if command.get_all_aliases().collect::<Vec<_>>() != policy.aliases {
        return Err(format!("unclassified aliases: {path}"));
    }
    let mut local = Vec::new();
    for arg in command.get_arguments() {
        let id = arg.get_id().as_str();
        if matches!(id, "help" | "version") {
            continue;
        }
        let field = policy
            .args
            .get(id)
            .or_else(|| REGISTRY.globals.get(id))
            .ok_or_else(|| format!("unclassified argument: {path} {id}"))?;
        if !REGISTRY.globals.contains_key(id) {
            local.push(id.to_owned());
        }
        if field.action != format!("{:?}", arg.get_action()) {
            return Err(format!("changed argument action: {path} {id}"));
        }
        if !matches!(
            field.policy.as_str(),
            "boolean" | "enum" | "presence" | "count" | "bucket"
        ) {
            return Err(format!("invalid policy: {path} {id}"));
        }
        let choices = arg.get_possible_values();
        if !choices.is_empty()
            && (field.policy != "enum"
                || choices
                    .iter()
                    .any(|v| !field.values.iter().any(|s| s == v.get_name())))
        {
            return Err(format!("unclassified enum value: {path} {id}"));
        }
    }
    local.sort();
    if local != policy.args.keys().cloned().collect::<Vec<_>>() {
        return Err(format!("stale argument policy: {path}"));
    }
    for sub in command.get_subcommands().filter(|c| c.get_name() != "help") {
        validate_command(sub, format!("{path} {}", sub.get_name()).trim(), seen)?;
    }
    Ok(())
}

pub fn validate() -> Result<(), String> {
    let mut command = crate::commands::cli_def();
    command.build();
    let mut seen = Vec::new();
    validate_command(&command, "", &mut seen)?;
    seen.sort();
    if seen != REGISTRY.commands.keys().cloned().collect::<Vec<_>>() {
        return Err("stale command policy".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(args: &[&str]) -> Value {
        Value::Object(properties(
            &args.iter().map(OsString::from).collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn every_command_argument_alias_and_enum_has_a_reviewed_policy() {
        validate().unwrap();
        let source = include_str!("../protocol/mod.rs");
        let methods = source
            .split_once("pub mod method {")
            .unwrap()
            .1
            .split_once("\n}")
            .unwrap()
            .0;
        let names = methods
            .lines()
            .filter_map(|line| line.trim().strip_prefix("pub const "))
            .filter_map(|line| line.split_once("&str = \""))
            .filter_map(|(_, value)| value.split_once('"').map(|(name, _)| name.to_owned()))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names, REGISTRY.rc_methods.keys().cloned().collect());
    }

    #[test]
    fn values_are_whitelisted_instead_of_redacted_after_capture() {
        for argv in [
            vec![
                "agit",
                "commit",
                "private-owner/repo@branch",
                "--milestone",
                "private-canary",
                "--",
                "/secret/file",
            ],
            vec![
                "agit",
                "search",
                "private-canary",
                "--scope",
                "private-owner/repo",
                "--runtime",
                "private-canary",
                "--tool",
                "private-canary",
            ],
            vec![
                "agit",
                "rc",
                "grant",
                "private-canary",
                "rm -rf /private-canary",
            ],
            vec![
                "agit",
                "login",
                "--complete",
                "private-canary",
                "--hub",
                "https://private-canary",
            ],
            vec!["agit", "pr", "show", "123456789"],
            vec!["agit", "config", "hub.url", "https://private-canary"],
            vec![
                "agit",
                "doctor",
                "--repair-permissions",
                "/private-canary/state",
            ],
        ] {
            let output = capture(&argv).to_string();
            for canary in [
                "private-canary",
                "private-owner",
                "/secret/file",
                "123456789",
            ] {
                assert!(!output.contains(canary), "{output}");
            }
        }
        let value = capture(&[
            "agit", "search", "query", "--type", "agents", "--limit", "50", "--local", "--scope",
            "mine",
        ]);
        assert_eq!(value["arg_kind"], "agents");
        assert_eq!(value["arg_limit"], "21-100");
        assert_eq!(value["arg_local"], true);
        assert_eq!(value["arg_scope"], "mine");
        assert_eq!(
            capture(&[
                "agit",
                "doctor",
                "--repair-permissions",
                "/private-canary/state"
            ])["arg_repair_permissions"],
            true
        );
        assert_eq!(
            capture(&["agit", "doctor"])["arg_repair_permissions"],
            false
        );
    }

    #[test]
    fn local_reconciliation_captures_flags_without_arbitrary_feature_or_target_values() {
        let bridge = capture(&[
            "agit",
            "rc",
            "local",
            "bridge",
            "--ensure",
            "--require-current-build",
            "--require-feature",
            "peer-control-v1",
            "--require-feature",
            "private-canary",
        ]);
        assert_eq!(bridge["command_path"], "rc local bridge");
        assert_eq!(bridge["arg_ensure"], true);
        assert_eq!(bridge["arg_require_current_build"], true);
        assert_eq!(
            bridge["arg_required_features"],
            json!(["other", "peer-control-v1"])
        );
        assert!(!bridge.to_string().contains("private-canary"));

        let restart = capture(&["agit", "rc", "local", "restart", "--if-idle"]);
        assert_eq!(restart["command_path"], "rc local restart");
        assert_eq!(restart["arg_if_idle"], true);

        let recover = capture(&["agit", "rc", "local", "recover", "--confirm-stopped"]);
        assert_eq!(recover["command_path"], "rc local recover");
        assert_eq!(recover["arg_confirm_stopped"], true);

        let upgrade = capture(&[
            "agit",
            "rc",
            "local",
            "after-upgrade",
            "--target",
            r#"{"pid":123456789,"instance_id":"private-canary","executable":"/secret/file"}"#,
        ]);
        assert_eq!(upgrade["command_path"], "rc local after-upgrade");
        assert_eq!(upgrade["arg_target"], true);
        for value in ["private-canary", "/secret/file", "123456789"] {
            assert!(!upgrade.to_string().contains(value), "{upgrade}");
        }
    }

    #[test]
    fn canonical_and_nested_commands_keep_their_names() {
        assert_eq!(capture(&["agit", "run", "private"])["command"], "run");
        assert_eq!(
            capture(&["agit", "repo", "collab", "add", "private", "private"])["subcommand"],
            "collab add"
        );
        assert!(
            !capture(&["agit", "private-canary", "--private-canary"])
                .to_string()
                .contains("private-canary")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cloud_telemetry_keeps_permission_enums_without_endpoint_or_resource_values() {
        for args in [
            vec!["enroll", "--name", "private-canary"],
            vec!["devices", "--after", "private-canary"],
            vec!["status"],
            vec![
                "grant",
                "--account",
                "private-canary",
                "--resource",
                "session:private-canary",
                "--access",
                "read",
            ],
        ] {
            let mut argv = vec!["agit", "rc", "cloud"];
            argv.extend(args.iter().copied());
            argv.extend(["--hub", "https://private-canary"]);
            let value = capture(&argv);
            assert_eq!(value["command_path"], format!("rc cloud {}", args[0]));
            assert!(!value.to_string().contains("private-canary"));
            if args[0] == "grant" {
                assert_eq!(value["arg_access"], "read");
            }
        }
    }
}
