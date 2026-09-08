//! Local provider resolution for portable Codex rollouts.

use super::Adapter;
use crate::rc::harness::proc::{Line, Proc};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

const CONFIG_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONFIG_FRAMES: usize = 128;
const MAX_HEADER_BYTES: u64 = 8 * 1024 * 1024;

struct NativeConfig {
    registered: bool,
    resume_provider: Option<String>,
}

fn header(content: &str) -> Option<(std::ops::Range<usize>, Value)> {
    let mut start = 0;
    for line in content.split_inclusive('\n') {
        if !line.trim().is_empty() {
            let value: Value = serde_json::from_str(line).ok()?;
            return (value.get("type")?.as_str()? == "session_meta")
                .then_some((start..start + line.trim_end_matches('\n').len(), value));
        }
        start += line.len();
    }
    None
}

fn header_provider(content: &str) -> Option<String> {
    let (_, value) = header(content)?;
    value
        .pointer("/payload/model_provider")?
        .as_str()
        .map(str::to_owned)
}

fn localize_with(content: &str, registered: Option<bool>) -> Option<String> {
    if registered != Some(false) {
        return None;
    }
    let (range, mut value) = header(content)?;
    if value.pointer("/payload/model_provider")?.as_str()? != "OpenAI" {
        return None;
    }
    *value.pointer_mut("/payload/model_provider")? = json!("openai");
    let mut localized = content.to_owned();
    localized.replace_range(range, &serde_json::to_string(&value).ok()?);
    Some(localized)
}

/// Only an unregistered display label is localized; configured provider identifiers are opaque.
pub(super) fn localize(content: &str, cwd: &Path) -> Option<String> {
    if header_provider(content).as_deref() != Some("OpenAI") {
        return None;
    }
    localize_with(
        content,
        registered(cwd, None).map(|config| config.registered),
    )
}

fn rollout_provider(sid: &str, cwd: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let path = super::codex::Codex.resolve(sid, Some(cwd))?;
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(MAX_HEADER_BYTES));
    let mut first = String::new();
    loop {
        if reader.read_line(&mut first).ok()? == 0 {
            return None;
        }
        if !first.trim().is_empty() {
            break;
        }
        first.clear();
    }
    let (_, value) = header(&first)?;
    if value.pointer("/payload/id")?.as_str()? != sid {
        return None;
    }
    header_provider(&first)
}

/// Native metadata owns persisted provider selection, including relocated and versioned indexes.
pub(crate) fn resume_override(sid: &str, cwd: &Path) -> Option<String> {
    if rollout_provider(sid, cwd)?.is_empty() {
        return None;
    }
    registered(cwd, Some(sid))?.resume_provider
}

fn registry_response(response: &Value) -> Option<bool> {
    if response.get("error").is_some() {
        return None;
    }
    Some(
        response
            .pointer("/result/config/model_providers")?
            .as_object()?
            .contains_key("OpenAI"),
    )
}

fn metadata_provider(response: &Value, sid: &str) -> Option<String> {
    if response.get("error").is_some() || response.pointer("/result/thread/id")?.as_str()? != sid {
        return None;
    }
    response
        .pointer("/result/thread/modelProvider")?
        .as_str()
        .filter(|provider| !provider.is_empty())
        .map(str::to_owned)
}

fn provider_binding(response: &Value, provider: &str) -> Option<String> {
    if response.get("error").is_some() {
        return None;
    }
    let registry = response
        .pointer("/result/config/model_providers")?
        .as_object()?;
    if provider != "OpenAI" {
        return None;
    }
    // Registered providers must retain the native session's saved model and reasoning settings.
    (!registry.contains_key(provider)).then(|| "openai".to_owned())
}

async fn response(proc: &mut Proc, id: i64) -> Option<Value> {
    for _ in 0..MAX_CONFIG_FRAMES {
        match proc.next().await?.into_line() {
            Line::Json(value)
                if value.get("method").is_none()
                    && value.get("id").and_then(Value::as_i64) == Some(id) =>
            {
                return Some(value);
            }
            Line::Eof | Line::Fatal(_) => return None,
            _ => {}
        }
    }
    None
}

fn config_request(cwd: &Path) -> Option<Value> {
    Some(json!({
        "id": 2,
        "method": "config/read",
        "params": {"cwd": cwd.to_str()?, "includeLayers": false}
    }))
}

async fn read_registry(proc: &mut Proc, cwd: &Path, sid: Option<&str>) -> Option<NativeConfig> {
    let request = config_request(cwd)?;
    proc.write_line(&json!({
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "agit-provider-check", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true, "requestAttestation": false}
        }
    }))
    .await
    .ok()?;
    let initialized = response(proc, 1).await?;
    if initialized.get("error").is_some() || initialized.get("result").is_none() {
        return None;
    }
    proc.write_line(&json!({"method": "initialized"}))
        .await
        .ok()?;
    let provider = if let Some(sid) = sid {
        // Unindexed rollouts need native metadata reconciliation before launch to retain their settings.
        proc.write_line(&json!({
            "id": 3,
            "method": "thread/read",
            "params": {"threadId": sid, "includeTurns": false}
        }))
        .await
        .ok()?;
        Some(metadata_provider(&response(proc, 3).await?, sid)?)
    } else {
        None
    };
    proc.write_line(&request).await.ok()?;
    let response = response(proc, 2).await?;
    Some(NativeConfig {
        registered: registry_response(&response)?,
        resume_provider: provider.and_then(|provider| provider_binding(&response, &provider)),
    })
}

async fn registered_async(cwd: &Path, sid: Option<&str>) -> Option<NativeConfig> {
    let cwd = cwd.to_path_buf();
    let mut proc = Proc::spawn("codex", &["app-server".into()], &cwd, &[]).ok()?;
    let registered = tokio::time::timeout(CONFIG_TIMEOUT, read_registry(&mut proc, &cwd, sid))
        .await
        .ok()
        .flatten();
    // Native configuration can contain credentials; neither responses nor diagnostics escape.
    proc.shutdown().await.ok()?;
    registered
}

fn registered(cwd: &Path, sid: Option<&str>) -> Option<NativeConfig> {
    // Installation can run inside an async supervisor; a separate runtime avoids nested polling.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok()?;
                runtime.block_on(registered_async(cwd, sid))
            })
            .ok()?
            .join()
            .ok()
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(provider: &str) -> String {
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"old\",\"model_provider\":{provider},\"base_instructions\":\"preserved\"}}}}\n{{\"type\":\"response_item\",\"payload\":{{\"encrypted_content\":\"opaque\"}}}}\n"
        )
    }

    #[test]
    fn a_display_label_is_localized_only_after_registry_absence_is_proven() {
        let original = transcript("\"OpenAI\"");
        let localized = localize_with(&original, Some(false)).unwrap();
        assert_eq!(header_provider(&localized).as_deref(), Some("openai"));
        assert_eq!(
            localized.split_once('\n').unwrap().1,
            original.split_once('\n').unwrap().1
        );
        assert_eq!(
            header(&localized).unwrap().1["payload"]["base_instructions"],
            "preserved"
        );
        assert!(localize_with(&original, Some(true)).is_none());
        assert!(localize_with(&original, None).is_none());
        assert_eq!(header_provider(&original).as_deref(), Some("OpenAI"));
    }

    #[test]
    fn provider_identifiers_and_unknown_metadata_are_not_reinterpreted() {
        for provider in [
            "\"openai\"",
            "\"azure\"",
            "\"OPENAI\"",
            "\"OpenAi\"",
            "\"\"",
            "null",
            "7",
        ] {
            assert!(localize_with(&transcript(provider), Some(false)).is_none());
        }
        assert!(localize_with("not json\n", Some(false)).is_none());
    }

    #[test]
    fn native_metadata_and_registry_preserve_exact_provider_identity() {
        let registry = json!({"result":{"config":{"model_providers":{
            "OpenAI":{"name":"custom"},"provider with \"quotes\"":{"name":"custom"}
        }}}});
        assert_eq!(provider_binding(&registry, "OpenAI").as_deref(), None);
        assert_eq!(
            provider_binding(&registry, "provider with \"quotes\"").as_deref(),
            None
        );
        assert_eq!(provider_binding(&registry, "openai").as_deref(), None);
        assert_eq!(provider_binding(&registry, "unknown"), None);
        assert_eq!(
            provider_binding(
                &json!({"result":{"config":{"model_providers":{}}}}),
                "OpenAI"
            )
            .as_deref(),
            Some("openai")
        );
        assert_eq!(
            provider_binding(
                &json!({"result":{"config":{"model_providers":{"OpenAI":null}}}}),
                "OpenAI"
            ),
            None
        );
        let metadata = json!({"result":{"thread":{"id":"selected","modelProvider":"OpenAI"}}});
        assert_eq!(
            metadata_provider(&metadata, "selected").as_deref(),
            Some("OpenAI")
        );
        assert_eq!(metadata_provider(&metadata, "other"), None);
        assert_eq!(
            metadata_provider(
                &json!({"result":{"thread":{"id":"selected","modelProvider":""}}}),
                "selected"
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_workspaces_do_not_produce_a_lossy_config_lookup() {
        use std::os::unix::ffi::OsStringExt;
        let cwd = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/\xff".to_vec()));
        assert!(config_request(&cwd).is_none());
    }

    #[test]
    fn only_an_effective_registry_object_proves_absence() {
        assert_eq!(
            registry_response(&json!({"result":{"config":{"model_providers":{}}}})),
            Some(false)
        );
        assert_eq!(
            registry_response(
                &json!({"result":{"config":{"model_providers":{"OpenAI":{"name":"custom"}}}}})
            ),
            Some(true)
        );
        for value in [
            json!({}),
            json!({"result":{"config":{}}}),
            json!({"result":{"config":{"model_providers":null}}}),
            json!({"result":{"config":{"model_providers":[]}}}),
            json!({"error":{"message":"private"},"result":{"config":{"model_providers":{}}}}),
        ] {
            assert_eq!(registry_response(&value), None);
        }
    }
}
