//! Bounded local buffering and a consent gate shared with the detached sender.

use super::state;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};

pub const MAX_QUEUE_BYTES: u64 = 1024 * 1024;
const MAX_EVENTS: usize = 1000;
const MAX_EVENT_BYTES: usize = 8192;
const TTL_MS: i64 = 24 * 60 * 60 * 1000;
const BATCH_SIZE: usize = 50;

#[derive(Clone)]
pub(crate) struct Destination {
    pub url: String,
    pub key: String,
    pub route: String,
    pub environment: &'static str,
}

pub(crate) fn environment(hub: &str) -> &'static str {
    if crate::infra::hub_authority::HubAuthority::parse(hub).is_err() {
        return "unknown";
    }
    let Ok(uri) = hub.parse::<http::Uri>() else {
        return "unknown";
    };
    match (uri.scheme_str(), uri.host(), uri.port_u16()) {
        (Some("https"), Some("agent-git.com" | "www.agent-git.com"), None | Some(443)) => {
            "production"
        }
        (Some("https"), Some("staging.agent-git.com"), _) => "staging",
        (_, Some("dev.agent-git.com" | "localhost" | "127.0.0.1" | "[::1]"), _) => "development",
        _ => "self_hosted",
    }
}

impl Destination {
    pub fn for_hub(hub: &str) -> Option<Self> {
        let environment = environment(hub);
        let host = std::env::var("AGIT_TELEMETRY_HOST").ok();
        let key = std::env::var("AGIT_TELEMETRY_KEY").ok();
        let (host, key) = match (host, key) {
            (Some(host), Some(key)) => (host, key),
            (None, None) if environment == "production" => (
                "https://us.i.posthog.com".into(),
                // Public ingestion token shared with the Hub frontend, never a Hub access token.
                "phc_p24ACGfxjfJLPLeJM3T4d5BwaaecLBquGk8Fg9xpdZZ8".into(),
            ),
            _ => return None,
        };
        Self::checked(hub, host, key, environment)
    }

    fn checked(hub: &str, host: String, key: String, environment: &'static str) -> Option<Self> {
        if key.is_empty()
            || key.len() > 256
            || !key
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
        {
            return None;
        }
        crate::infra::hub_authority::HubAuthority::parse(&host).ok()?;
        let uri = host.parse::<http::Uri>().ok()?;
        if uri.scheme_str() != Some("https")
            && !(uri.scheme_str() == Some("http")
                && matches!(uri.host(), Some("localhost" | "127.0.0.1" | "[::1]")))
        {
            return None;
        }
        let url = format!("{}/batch/", host.trim_end_matches('/'));
        let authority = crate::infra::hub_authority::HubAuthority::parse(hub)
            .ok()?
            .storage_key();
        let route = hex::encode(Sha256::digest(format!("{authority}\0{url}\0{key}")));
        Some(Self {
            url,
            key,
            route,
            environment,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum EventName {
    #[serde(rename = "cli_command_started")]
    Started,
    #[serde(rename = "cli_command_finished")]
    Finished,
    #[serde(rename = "cli_operation_finished")]
    Operation,
    #[serde(rename = "cli_integration_summary")]
    Integration,
    #[serde(rename = "cli_onboarding_completed")]
    Onboarding,
    #[serde(rename = "cli_session_started")]
    Session,
    #[serde(rename = "cli_install_succeeded")]
    InstallSucceeded,
    #[serde(rename = "cli_install_stage")]
    InstallStage,
    #[serde(rename = "cli_acquisition_linked")]
    AcquisitionLinked,
    #[serde(rename = "cli_install_attributed")]
    InstallAttributed,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Event {
    pub event: EventName,
    pub uuid: uuid::Uuid,
    pub distinct_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub properties: Map<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    route: String,
    generation: u64,
    event: Event,
}

#[derive(Default, Serialize, Deserialize)]
struct Queue {
    entries: Vec<Entry>,
    next_send: i64,
    failures: u32,
}

fn load(dir: &Path) -> Result<Queue> {
    Ok(state::read_json(&dir.join("queue.json"), MAX_QUEUE_BYTES)?.unwrap_or_default())
}

fn prune(queue: &mut Queue, generation: u64, now: i64) {
    queue.entries.retain(|entry| {
        entry.generation == generation
            && (0..=TTL_MS).contains(&(now - entry.event.timestamp.timestamp_millis()))
    });
    if queue.entries.len() > MAX_EVENTS {
        queue.entries.drain(..queue.entries.len() - MAX_EVENTS);
    }
    while serde_json::to_vec(queue).is_ok_and(|bytes| bytes.len() as u64 > MAX_QUEUE_BYTES)
        && !queue.entries.is_empty()
    {
        queue.entries.remove(0);
    }
}

pub(crate) fn enqueue(event: Event, generation: u64, destination: &Destination) -> Result<()> {
    ensure!(
        serde_json::to_vec(&event)?.len() <= MAX_EVENT_BYTES,
        "telemetry event exceeds its limit"
    );
    let dir = state::directory()?;
    let _guard = state::gate(
        &dir,
        matches!(
            event.event,
            EventName::InstallSucceeded | EventName::InstallAttributed | EventName::InstallStage
        ),
    )?;
    let mut preferences = state::read_at(&dir)?;
    if !state::enabled(&preferences) || preferences.generation != generation {
        return Ok(());
    }
    if (event.event == EventName::InstallSucceeded && preferences.install_reported)
        || (event.event == EventName::AcquisitionLinked && preferences.acquisition_completed)
        || (event.event == EventName::InstallAttributed && preferences.acquisition_reported)
    {
        return Ok(());
    }
    let mut queue = load(&dir)?;
    let now = chrono::Utc::now().timestamp_millis();
    prune(&mut queue, generation, now);
    if event.event == EventName::Integration {
        let key = |event: &Event| {
            let mut value = event.properties.clone();
            for name in [
                "invocation_id",
                "event_id",
                "event_ts",
                "duration_bucket",
                "integration_count",
                "parent_invocation_id",
            ] {
                value.remove(name);
            }
            value
        };
        if let Some(entry) = queue.entries.iter_mut().rev().find(|entry| {
            entry.route == destination.route
                && entry.event.event == EventName::Integration
                && entry.event.distinct_id == event.distinct_id
                && now - entry.event.timestamp.timestamp_millis() < 60_000
                && key(&entry.event) == key(&event)
        }) {
            let count = entry
                .event
                .properties
                .get("integration_count")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            let added = event
                .properties
                .get("integration_count")
                .and_then(Value::as_u64)
                .unwrap_or(1);
            entry.event.properties.insert(
                "integration_count".into(),
                json!(count.saturating_add(added)),
            );
            return state::write_json(&dir.join("queue.json"), &queue);
        }
    }
    let event_name = event.event;
    queue.entries.retain(|entry| entry.event.uuid != event.uuid);
    queue.entries.push(Entry {
        route: destination.route.clone(),
        generation,
        event,
    });
    prune(&mut queue, generation, now);
    state::write_json(&dir.join("queue.json"), &queue)?;
    match event_name {
        EventName::InstallSucceeded => preferences.install_reported = true,
        EventName::AcquisitionLinked => preferences.acquisition_completed = true,
        EventName::InstallAttributed => preferences.acquisition_reported = true,
        _ => return Ok(()),
    }
    state::write_json(&dir.join("preferences.json"), &preferences)
}

pub fn queue_status() -> (usize, u64) {
    let Ok(dir) = state::directory() else {
        return (0, 0);
    };
    let size = std::fs::metadata(dir.join("queue.json"))
        .map(|v| v.len())
        .unwrap_or(0);
    (load(&dir).map(|v| v.entries.len()).unwrap_or(0), size)
}

pub(crate) fn spawn_worker(hub: &str) {
    if state::override_reason().is_some() || state::positive("AGIT_TELEMETRY_DEBUG") {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = std::process::Command::new(exe);
    command
        .arg("--internal-telemetry-flush")
        .env("AGIT_HUB_URL", hub)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    if let Ok(mut child) = command.spawn() {
        let _ = std::thread::Builder::new()
            .name("telemetry-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

/// The gate remains held through the request, so disable can wait for the bounded request then purge.
pub fn flush() -> Result<()> {
    flush_inner(None)
}

pub(crate) fn flush_installation(attempt: uuid::Uuid) -> Result<()> {
    flush_inner(Some(attempt))
}

fn flush_inner(installation_attempt: Option<uuid::Uuid>) -> Result<()> {
    if state::override_reason().is_some() || state::positive("AGIT_TELEMETRY_DEBUG") {
        return Ok(());
    }
    let hub = crate::infra::config::hub_url();
    let _ = super::acquisition::enqueue_pending_link(&hub);
    let Some(destination) = Destination::for_hub(&hub) else {
        return Ok(());
    };
    let dir = state::directory()?;
    let _guard = state::gate(&dir, installation_attempt.is_some())?;
    let preferences = state::read_at(&dir)?;
    if !state::enabled(&preferences) {
        return Ok(());
    }
    let mut queue = load(&dir)?;
    let now = chrono::Utc::now().timestamp_millis();
    prune(&mut queue, preferences.generation, now);
    let terminal = installation_attempt.and_then(|attempt| {
        queue.entries.iter().find(|entry| {
            entry.route == destination.route
                && entry.event.event == EventName::InstallStage
                && entry.event.properties.get("attempt_id") == Some(&json!(attempt))
                && entry.event.properties.get("stage") == Some(&json!("finished"))
        })
    });
    let terminal_installation = terminal
        .and_then(|entry| entry.event.properties.get("installation_id"))
        .cloned();
    let terminal_drain = terminal.is_some() && queue.failures == 0;
    if queue.next_send > now && !terminal_drain {
        return state::write_json(&dir.join("queue.json"), &queue);
    }
    let mut eligible = queue
        .entries
        .iter()
        .filter(|entry| {
            entry.route == destination.route
                && (entry.event.event != EventName::Integration
                    || now - entry.event.timestamp.timestamp_millis() >= 60_000)
        })
        .collect::<Vec<_>>();
    if terminal_drain {
        eligible.sort_by_key(|entry| {
            let same_attempt = installation_attempt.is_some_and(|attempt| {
                entry.event.properties.get("attempt_id") == Some(&json!(attempt))
            });
            let receipt = matches!(
                entry.event.event,
                EventName::InstallSucceeded | EventName::InstallAttributed
            ) && terminal_installation.is_some()
                && entry.event.properties.get("installation_id") == terminal_installation.as_ref();
            !(same_attempt || receipt)
        });
    }
    let batch = eligible
        .into_iter()
        .take(BATCH_SIZE)
        .map(|entry| entry.event.clone())
        .collect::<Vec<_>>();
    if batch.is_empty() {
        return state::write_json(&dir.join("queue.json"), &queue);
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_millis(750)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let response = agent
        .post(&destination.url)
        .send_json(json!({"api_key": destination.key, "batch": batch}));
    let accepted = response
        .as_ref()
        .is_ok_and(|response| response.status().is_success());
    let permanent = response.as_ref().is_ok_and(|response| {
        response.status().is_client_error() && !matches!(response.status().as_u16(), 408 | 429)
    });
    if accepted || permanent {
        queue
            .entries
            .retain(|entry| !batch.iter().any(|event| event.uuid == entry.event.uuid));
        queue.failures = 0;
        queue.next_send = now + 30_000;
    } else {
        queue.failures = queue.failures.saturating_add(1).min(7);
        queue.next_send = now + (30_000i64 * (1 << queue.failures)).min(3_600_000);
    }
    state::write_json(&dir.join("queue.json"), &queue)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(now: i64) -> Event {
        Event {
            event: EventName::Finished,
            uuid: uuid::Uuid::new_v4(),
            distinct_id: "synthetic".into(),
            timestamp: chrono::DateTime::from_timestamp_millis(now).unwrap(),
            properties: json!({"command":"status"}).as_object().unwrap().clone(),
        }
    }
    #[test]
    fn queue_bounds_remove_expired_generations_and_keep_stable_event_ids() {
        let now = chrono::Utc::now().timestamp_millis();
        let mut queue = Queue::default();
        for index in 0..=MAX_EVENTS {
            queue.entries.push(Entry {
                route: "synthetic".into(),
                generation: 1,
                event: event(now - index as i64),
            });
        }
        let last = queue.entries.last().unwrap().event.uuid;
        queue.entries.push(Entry {
            route: "synthetic".into(),
            generation: 0,
            event: event(now),
        });
        queue.entries.push(Entry {
            route: "synthetic".into(),
            generation: 1,
            event: event(now - TTL_MS - 1),
        });
        prune(&mut queue, 1, now);
        assert_eq!(queue.entries.len(), MAX_EVENTS);
        assert!(queue.entries.iter().all(|entry| entry.generation == 1
            && now - entry.event.timestamp.timestamp_millis() <= TTL_MS));
        assert_eq!(queue.entries.last().unwrap().event.uuid, last);
        let padding_bytes = MAX_EVENT_BYTES / 2;
        let byte_limit_entries = MAX_QUEUE_BYTES as usize / padding_bytes + 1;
        queue
            .entries
            .drain(..queue.entries.len() - byte_limit_entries);
        for entry in &mut queue.entries {
            entry
                .event
                .properties
                .insert("synthetic_padding".into(), json!("x".repeat(padding_bytes)));
            assert!(serde_json::to_vec(&entry.event).unwrap().len() <= MAX_EVENT_BYTES);
        }
        assert!(serde_json::to_vec(&queue).unwrap().len() as u64 > MAX_QUEUE_BYTES);
        prune(&mut queue, 1, now);
        assert!(serde_json::to_vec(&queue).unwrap().len() as u64 <= MAX_QUEUE_BYTES);
        assert_eq!(queue.entries.last().unwrap().event.uuid, last);
    }
    #[test]
    fn routing_rejects_remote_plaintext_credentials_and_query_strings() {
        for host in [
            "http://example.test",
            "https://user:secret@example.test",
            "https://example.test?token=secret",
            "file:///tmp/events",
        ] {
            assert!(
                Destination::checked(
                    "https://agent-git.com",
                    host.into(),
                    "synthetic".into(),
                    "production"
                )
                .is_none()
            );
        }
        assert!(
            Destination::checked(
                "https://agent-git.com",
                "https://example.test".into(),
                "synthetic".into(),
                "production"
            )
            .is_some()
        );
        let one = Destination::checked(
            "https://agent-git.com",
            "http://127.0.0.1:9".into(),
            "synthetic".into(),
            "production",
        )
        .unwrap();
        let two = Destination::checked(
            "https://selfhost.test",
            "http://127.0.0.1:9".into(),
            "synthetic".into(),
            "self_hosted",
        )
        .unwrap();
        assert_ne!(one.route, two.route);
    }
}
