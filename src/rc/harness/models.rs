//! Runtime-owned model discovery without submitting a user turn.
use super::proc::{Line, Proc};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Omission preserves a setting; JSON null (or a legacy empty string) restores its default.
#[derive(Debug, Clone, Default)]
pub struct ModelPatch {
    pub model: Option<Option<String>>,
    pub effort: Option<Option<String>>,
}

impl ModelPatch {
    pub fn parse(params: &Value) -> crate::Result<Self> {
        fn field(params: &Value, name: &str) -> crate::Result<Option<Option<String>>> {
            let Some(value) = params.get(name) else {
                return Ok(None);
            };
            if value.is_null() || value == "" {
                return Ok(Some(None));
            }
            let value = value
                .as_str()
                .filter(|s| {
                    !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control)
                })
                .with_context(|| format!("{name} must be an identifier or null"))?;
            Ok(Some(Some(value.into())))
        }
        let patch = Self {
            model: field(params, "model")?,
            effort: field(params, "effort")?,
        };
        ensure!(
            patch.model.is_some() || patch.effort.is_some(),
            "Choose a model or effort to change"
        );
        Ok(patch)
    }
}

pub(super) fn selected<'a>(catalog: &'a [Value], model: Option<&str>) -> Option<&'a Value> {
    catalog.iter().find(|entry| match model {
        Some(id) => entry["id"] == id || entry["native"]["resolvedModel"] == id,
        None => entry["is_default"] == true || entry["id"] == "default",
    })
}

pub(super) fn efforts(model: Option<&Value>) -> Value {
    model
        .and_then(|m| m.get("efforts"))
        .cloned()
        .unwrap_or_else(|| json!([]))
}

pub(super) fn check_effort(model: Option<&Value>, effort: &str) -> crate::Result<()> {
    ensure!(
        efforts(model)
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["id"] == effort)),
        "This model does not advertise the requested effort"
    );
    Ok(())
}

async fn response(proc: &mut Proc, id: Value, claude: bool) -> crate::Result<Value> {
    while let Some(line) = proc.next().await {
        if let Line::Json(value) = line.line() {
            if claude && value.pointer("/response/request_id") == Some(&id) {
                ensure!(
                    value.pointer("/response/subtype").and_then(Value::as_str) == Some("success"),
                    "Claude model discovery failed: {}",
                    value["response"]
                );
                return Ok(value["response"]["response"].clone());
            }
            if !claude && value.get("id") == Some(&id) {
                ensure!(
                    value.get("error").is_none(),
                    "Codex model discovery failed: {}",
                    value["error"]
                );
                return value
                    .get("result")
                    .cloned()
                    .context("Runtime returned no model discovery result");
            }
        }
    }
    anyhow::bail!("Runtime exited during model discovery")
}

pub(crate) async fn codex_goal(cwd: PathBuf, thread: &str) -> crate::Result<Value> {
    let mut proc = Proc::spawn("codex", &["app-server".into()], &cwd, &[])?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(25), async {
        proc.write_line(&json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"agentgit_goal","version":"0.1.0"},"capabilities":{"experimentalApi":true}}})).await?;
        response(&mut proc, json!(1), false).await?;
        proc.write_line(&json!({"method":"initialized"})).await?;
        proc.write_line(&json!({"id":2,"method":"thread/goal/get","params":{"threadId":thread}})).await?;
        response(&mut proc, json!(2), false).await
    }).await;
    let shutdown = proc.shutdown().await;
    let value = result.context("Native goal read timed out")?;
    shutdown?;
    value
}

pub async fn discover(runtime: &str, cwd: PathBuf) -> crate::Result<Value> {
    ensure!(
        cwd.is_dir(),
        "Model discovery requires an existing directory"
    );
    let (program, args) = match runtime {
        "codex" => ("codex", vec!["app-server"]),
        "claude-code" => (
            "claude",
            vec![
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ],
        ),
        _ => anyhow::bail!("This runtime does not provide a native model catalog"),
    };
    let mut proc = Proc::spawn(
        program,
        &args.into_iter().map(str::to_string).collect::<Vec<_>>(),
        &cwd,
        &[],
    )?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(25), async {
        if runtime == "claude-code" {
            proc.write_line(&json!({"type":"control_request", "request_id":"models", "request":{"subtype":"initialize", "hooks":{}}})).await?;
            let native = response(&mut proc, json!("models"), true).await?;
            let models = normalize(runtime, &native)?;
            Ok(json!({"models":models, "source":"initialize", "runtime":runtime}))
        } else {
            proc.write_line(&json!({"id":1, "method":"initialize", "params":{"clientInfo":{"name":"agentgit_models", "version":"0.1.0"}, "capabilities":{}}})).await?;
            response(&mut proc, json!(1), false).await?;
            proc.write_line(&json!({"method":"initialized"})).await?;
            let mut models = vec![];
            let mut cursor = Value::Null;
            let mut seen = std::collections::HashSet::new();
            loop {
                proc.write_line(&json!({"id":2, "method":"model/list", "params":{"limit":100,"cursor":cursor}})).await?;
                let native = response(&mut proc, json!(2), false).await?;
                models.extend(normalize(runtime, &native)?);
                cursor = native.get("nextCursor").cloned().unwrap_or(Value::Null);
                if cursor.is_null() { break; }
                ensure!(models.len() <= 2048 && seen.insert(cursor.to_string()), "Runtime model pagination did not converge");
            }
            Ok(json!({"models":models, "source":"model/list", "runtime":runtime}))
        }
    }).await;
    let shutdown = proc.shutdown().await;
    let value: crate::Result<Value> = result.context("Runtime model discovery timed out")?;
    shutdown?;
    value
}

pub(super) fn normalize(runtime: &str, native: &Value) -> crate::Result<Vec<Value>> {
    let list = native
        .get(if runtime == "codex" { "data" } else { "models" })
        .and_then(Value::as_array)
        .context("Runtime did not advertise a model list")?;
    Ok(list.iter().filter(|entry| entry.get("hidden") != Some(&json!(true))).filter_map(|entry| {
        let id = entry.get(if runtime == "codex" { "model" } else { "value" })?.as_str()?;
        let efforts = if runtime == "codex" {
            entry["supportedReasoningEfforts"].as_array().into_iter().flatten().filter_map(|v| {
                let id = v["reasoningEffort"].as_str()?;
                Some(json!({"id":id,"name":id,"description":v["description"]}))
            }).collect::<Vec<_>>()
        } else {
            entry["supportedEffortLevels"].as_array().into_iter().flatten().filter_map(|v| {
                let id = v.as_str()?;
                Some(json!({"id":id,"name":id}))
            }).collect()
        };
        Some(json!({"id":id, "name":entry.get("displayName").and_then(Value::as_str).unwrap_or(id), "description":entry.get("description").and_then(Value::as_str).unwrap_or(""), "efforts":efforts,"default_effort":entry["defaultReasoningEffort"],"is_default":entry["isDefault"], "native":entry}))
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_patches_distinguish_omission_reset_and_native_identifiers() {
        let patch = ModelPatch::parse(&json!({"model":"provider/model", "effort":null})).unwrap();
        assert_eq!(patch.model, Some(Some("provider/model".into())));
        assert_eq!(patch.effort, Some(None));
        let patch = ModelPatch::parse(&json!({"model":""})).unwrap();
        assert_eq!(patch.model, Some(None));
        assert_eq!(patch.effort, None);
        for params in [
            json!({}),
            json!({"effort":false}),
            json!({"model":"bad\nmodel"}),
        ] {
            assert!(ModelPatch::parse(&params).is_err());
        }
    }
    #[test]
    fn catalogs_preserve_native_ids_and_metadata() {
        let c = normalize("codex", &json!({"data":[{"id":"ui-id","model":"wire-model","displayName":"Model","supportedReasoningEfforts":[{"reasoningEffort":"high"}]},{"model":"hidden","hidden":true}]})).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["id"], "wire-model");
        assert!(c[0]["native"]["supportedReasoningEfforts"].is_array());
        assert_eq!(c[0]["efforts"][0]["id"], "high");
        let c = normalize("claude-code", &json!({"models":[{"value":"custom-alias","resolvedModel":"provider-id","displayName":"Custom"}]})).unwrap();
        assert_eq!(c[0]["id"], "custom-alias");
        assert_eq!(c[0]["native"]["resolvedModel"], "provider-id");
        assert!(normalize("claude-code", &json!({})).is_err());
    }
}
