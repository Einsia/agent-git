//! Visible installer stages share the existing consent, route, and bounded transport.

use super::{Destination, Event, EventName, acquisition, state, transport};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stage {
    attempt_id: uuid::Uuid,
    stage: Phase,
    outcome: Outcome,
    elapsed_ms: u64,
    error_category: ErrorCategory,
}

#[derive(Clone, Copy, Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Started,
    BinaryCopy,
    Verification,
    Setup,
    Finished,
}

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Started,
    Ok,
    Error,
    Skipped,
    Partial,
}

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCategory {
    None,
    Filesystem,
    Binary,
    Integration,
}

pub fn record(raw: &str) -> anyhow::Result<()> {
    if raw.len() > 1024 || state::override_reason().is_some() {
        return Ok(());
    }
    let stage: Stage = serde_json::from_str(raw)?;
    anyhow::ensure!(
        stage.attempt_id.get_version_num() == 4,
        "Invalid attempt ID"
    );
    let hub = crate::infra::config::hub_url();
    let Some(destination) = Destination::for_hub(&hub) else {
        return Ok(());
    };
    if stage.stage == Phase::Started {
        state::onboarding()?;
    }
    let dir = state::directory()?;
    let preferences = {
        let _guard = state::gate(&dir, true)?;
        let mut preferences = state::read_at(&dir)?;
        if !state::enabled(&preferences)
            || !acquisition::bind_route(&mut preferences, &dir, &destination)?
        {
            return Ok(());
        }
        if preferences.acquisition_id.is_none() && preferences.first_acquisition_account.is_none() {
            preferences.acquisition_id = std::env::var("AGIT_ACQUISITION_ID")
                .ok()
                .and_then(|value| uuid::Uuid::parse_str(&value).ok())
                .filter(|value| value.get_version_num() == 4);
        }
        state::write_json(&dir.join("preferences.json"), &preferences)?;
        preferences
    };
    let Some(installation_id) = preferences.device_id else {
        return Ok(());
    };
    let runtimes = crate::infra::runtime_session::ENV_SESSIONS
        .iter()
        .filter(|(name, _)| super::present(name))
        .map(|(_, runtime)| *runtime)
        .collect::<std::collections::BTreeSet<_>>();
    let runtime = match runtimes.len() {
        0 => "unknown",
        1 => runtimes.iter().next().copied().unwrap_or("unknown"),
        _ => "multiple",
    };
    let id = uuid::Uuid::new_v4();
    let now = chrono::Utc::now();
    let elapsed = stage.elapsed_ms.min(86_400_000);
    let mut properties = json!({
        "schema_version": 1, "client_type": "cli", "cli_version": super::build_version(),
        "source": "installer", "channel": "create_agit", "runtime_env": runtime,
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "ci": super::present("CI") || super::present("GITHUB_ACTIONS") || super::present("GITLAB_CI"),
        "app_env": if destination.environment == "production" {"production"} else {"development"},
        "deployment_env": destination.environment, "event_id": id, "event_ts": now.timestamp_millis(),
        "attempt_id": stage.attempt_id, "stage": stage.stage, "outcome": stage.outcome,
        "elapsed_ms": elapsed, "error_category": stage.error_category,
        "coverage": "visible_installer_after_package_fetch",
        "$process_person_profile": false, "$geoip_disable": true
    }).as_object().unwrap().clone();
    acquisition::extend_properties(&preferences, &mut properties);
    let event = Event {
        event: EventName::InstallStage,
        uuid: id,
        distinct_id: format!("cli-installation:{installation_id}"),
        timestamp: now,
        properties,
    };
    if state::positive("AGIT_TELEMETRY_DEBUG") {
        eprintln!("{}", json!({"telemetry_preview":event}));
    } else {
        transport::enqueue(event, preferences.generation, &destination)?;
        if stage.stage == Phase::Finished {
            transport::flush_installation(stage.attempt_id)?;
        } else if stage.stage == Phase::Started {
            transport::spawn_worker(&hub);
        }
    }
    Ok(())
}
