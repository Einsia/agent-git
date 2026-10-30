//! The first upload and the first share from an installation extend the acquisition funnel
//! (`docs/telemetry.md`). Each is counted once per installation generation and joined to the
//! installation and its acquisition key; it carries no content, path, repository or session.

use super::{Destination, Event, EventName, acquisition, state, transport};
use serde_json::json;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    /// The Hub acknowledged a session reference this installation updated.
    FirstUpload,
    /// This installation created a read-only link or an invitation.
    FirstShare,
}

/// Records `milestone`, reached against `hub`, unless this installation generation already has.
/// The Hub decides the destination, as for any other event: nothing is recorded when statistics
/// are off for it, it has no analytics destination, or the installation has no identity. Nor is
/// anything recorded through a route other than the one the installation's acquisition key is
/// bound to: that key belongs to one Hub and statistics project, and the milestone stays
/// unreported for them. A failure never reaches the command that reached the milestone.
pub fn reached(milestone: Milestone, hub: &str) {
    let _ = record(milestone, hub);
}

fn record(milestone: Milestone, hub: &str) -> anyhow::Result<()> {
    if state::override_reason(hub).is_some() {
        return Ok(());
    }
    let Some(destination) = Destination::for_hub(hub) else {
        return Ok(());
    };
    let dir = state::directory()?;
    let guard = state::gate(&dir, false)?;
    let mut preferences = state::read_at(&dir)?;
    if !state::enabled(&preferences, &destination.hub)
        || reported(&preferences, milestone)
        || !acquisition::bind_route(&mut preferences, &dir, &destination)?
    {
        return Ok(());
    }
    let Some(installation) = preferences.device_id else {
        return Ok(());
    };
    let event = event(milestone, installation, &preferences, &destination);
    if state::debug(&destination.hub) {
        eprintln!("{}", json!({"telemetry_preview": event}));
        return Ok(());
    }
    transport::enqueue_locked(&dir, event, preferences.generation, &destination)?;
    drop(guard);
    transport::spawn_worker(hub);
    Ok(())
}

pub(crate) fn reported(preferences: &state::Preferences, milestone: Milestone) -> bool {
    match milestone {
        Milestone::FirstUpload => preferences.first_upload_reported,
        Milestone::FirstShare => preferences.first_share_reported,
    }
}

fn event(
    milestone: Milestone,
    installation: uuid::Uuid,
    preferences: &state::Preferences,
    destination: &Destination,
) -> Event {
    let now = chrono::Utc::now();
    let id = uuid::Uuid::new_v4();
    let ci = ["CI", "GITHUB_ACTIONS", "GITLAB_CI"]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    let mut properties = json!({
        "schema_version": 1, "client_type": "cli", "cli_version": super::build_version(),
        "ci": ci,
        "channel": preferences.channel, "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "app_env": if destination.environment == "production" {"production"} else {"development"},
        "deployment_env": destination.environment, "event_id": id,
        "event_ts": now.timestamp_millis(), "$process_person_profile": false, "$geoip_disable": true
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    acquisition::extend_properties(preferences, &mut properties);
    Event {
        event: match milestone {
            Milestone::FirstUpload => EventName::FirstUpload,
            Milestone::FirstShare => EventName::FirstShare,
        },
        uuid: id,
        distinct_id: format!("cli-installation:{installation}"),
        timestamp: now,
        properties,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each milestone is queued once per installation generation, keyed by the installation and
    /// nothing about what was uploaded or shared; turning statistics off and on starts a new
    /// installation that reports again under its own key. An installation whose acquisition key is
    /// bound to another Hub's or statistics project's route queues nothing there and keeps the
    /// milestone for its own route. A property outside the reviewed list, a second report in one
    /// generation, a report through another route, or none after rotation fails here.
    const HUB: &str = "http://127.0.0.1:9";

    #[test]
    fn a_milestone_is_counted_once_per_installation_and_carries_no_content() {
        const CHILD: &str = "AGIT_ACTIVATION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args([
                    "--exact",
                    "telemetry::activation::tests::a_milestone_is_counted_once_per_installation_and_carries_no_content",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("AGIT_HOME", home.path().join("agit"))
                .env("AGIT_HUB_URL", HUB)
                .env("AGIT_TELEMETRY_HOST", "http://127.0.0.1:9")
                .env("AGIT_TELEMETRY_KEY", "synthetic_project");
            for name in [
                "AGIT_TELEMETRY_DISABLED",
                "DO_NOT_TRACK",
                "AGIT_TELEMETRY_DEFER",
                "AGIT_TELEMETRY_DEBUG",
            ] {
                child.env_remove(name);
            }
            let output = child.output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let enable = || {
            state::choose(
                state::Preference::Enabled,
                state::DecisionSource::ExplicitEnable,
                false,
            )
            .unwrap()
            .device_id
            .unwrap()
        };
        let milestones = || {
            let queue: serde_json::Value =
                state::read_json(&state::directory().unwrap().join("queue.json"), 1 << 20)
                    .unwrap()
                    .unwrap_or_default();
            queue["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|entry| entry["event"].clone())
                .filter(|event| {
                    matches!(
                        event["event"].as_str(),
                        Some("cli_first_upload" | "cli_first_share")
                    )
                })
                .collect::<Vec<_>>()
        };
        let first = enable();
        let dir = state::directory().unwrap();
        let route = |route: Option<&str>| {
            let mut preferences = state::read_at(&dir).unwrap();
            preferences.acquisition_route = route.map(str::to_owned);
            state::write_json(&dir.join("preferences.json"), &preferences).unwrap();
        };
        route(Some("another-hub-or-project"));
        reached(Milestone::FirstUpload, HUB);
        assert!(milestones().is_empty());
        assert!(!state::read_at(&dir).unwrap().first_upload_reported);
        route(None);
        reached(Milestone::FirstUpload, HUB);
        reached(Milestone::FirstUpload, HUB);
        reached(Milestone::FirstShare, HUB);
        let events = milestones();
        let names: Vec<_> = events
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["cli_first_upload", "cli_first_share"]);
        let reviewed = [
            "schema_version",
            "client_type",
            "cli_version",
            "ci",
            "channel",
            "os",
            "arch",
            "app_env",
            "deployment_env",
            "event_id",
            "event_ts",
            "$process_person_profile",
            "$geoip_disable",
            "installation_id",
            "acquisition_id",
        ];
        for event in &events {
            assert_eq!(event["properties"]["installation_id"], json!(first));
            for key in event["properties"].as_object().unwrap().keys() {
                assert!(
                    reviewed.contains(&key.as_str()),
                    "unreviewed property {key}"
                );
            }
        }

        state::choose(
            state::Preference::Disabled,
            state::DecisionSource::ExplicitDisable,
            false,
        )
        .unwrap();
        reached(Milestone::FirstUpload, HUB);
        assert!(
            milestones().is_empty(),
            "nothing is queued while statistics are off"
        );
        let second = enable();
        reached(Milestone::FirstUpload, HUB);
        let events = milestones();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["properties"]["installation_id"], json!(second));
        assert_ne!(first, second);
    }
}
