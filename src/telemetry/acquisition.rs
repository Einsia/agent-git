//! Scoped acquisition keys join verified installation to the first completed login.

use super::{Destination, Event, EventName, state, transport};
use serde_json::{Map, Value, json};

pub(crate) fn extend_properties(preferences: &state::Preferences, props: &mut Map<String, Value>) {
    if let Some(id) = preferences.device_id {
        props.insert("installation_id".into(), json!(id));
    }
    if let Some(id) = preferences.acquisition_id {
        props.insert("acquisition_id".into(), json!(id));
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct VerifiedInstall {
    verified_at: chrono::DateTime<chrono::Utc>,
    route: String,
    acquisition_id: Option<uuid::Uuid>,
    #[serde(default)]
    campaign: Option<super::campaign::Campaign>,
    channel: String,
    version: String,
    ci: bool,
}

impl VerifiedInstall {
    fn current(destination: &Destination) -> Self {
        Self {
            campaign: std::env::var("AGIT_CAMPAIGN_URL")
                .ok()
                .and_then(|raw| super::campaign::Campaign::from_url(&raw)),
            verified_at: chrono::Utc::now(),
            route: destination.route.clone(),
            acquisition_id: std::env::var("AGIT_ACQUISITION_ID")
                .ok()
                .and_then(|id| uuid::Uuid::parse_str(&id).ok())
                .filter(|id| id.get_version_num() == 4),
            channel: std::env::var("AGIT_INSTALL_CHANNEL")
                .ok()
                .filter(|channel| {
                    matches!(
                        channel.as_str(),
                        "npm_global" | "create_agit" | "source" | "archive"
                    )
                })
                .unwrap_or_else(|| "unknown".into()),
            version: super::build_version().into(),
            ci: ["CI", "GITHUB_ACTIONS", "GITLAB_CI"]
                .iter()
                .any(|name| std::env::var_os(name).is_some()),
        }
    }
}

/// Official Hub installations record receipts even when lifecycle output is hidden.
pub fn installed(defer_notice: bool) -> anyhow::Result<()> {
    let hub = crate::infra::config::hub_url();
    if state::override_reason(&hub).is_some() {
        return Ok(());
    }
    state::enforce(&hub)?;
    let Some(destination) = Destination::for_hub(&hub) else {
        return Ok(());
    };
    let fact = VerifiedInstall::current(&destination);
    if defer_notice && !state::required(&hub) {
        let dir = state::directory()?;
        let guard = state::gate(&dir, true)?;
        match state::read_at(&dir)?.optional_preference() {
            state::Preference::Disabled => return Ok(()),
            state::Preference::Unset => {
                let path = dir.join("pending-install.json");
                if state::read_json::<VerifiedInstall>(&path, 32768)?.is_none() {
                    state::write_json(&path, &fact)?;
                }
                return Ok(());
            }
            state::Preference::Enabled => drop(guard),
        }
    } else if !defer_notice {
        state::onboarding()?;
    }
    resume_pending(&hub)?;
    record_install(&destination, &fact, state::read()?.generation)?;
    transport::spawn_worker(&hub);
    Ok(())
}

/// Admitting a pending receipt does not make a local command online.
pub(crate) fn resume_pending(hub: &str) -> anyhow::Result<()> {
    let preferences = state::read()?;
    if !state::enabled(&preferences, hub) {
        return Ok(());
    }
    let Some(destination) = Destination::for_hub(hub) else {
        return Ok(());
    };
    let dir = state::directory()?;
    let Some(fact) = state::read_json::<VerifiedInstall>(&dir.join("pending-install.json"), 32768)?
    else {
        return Ok(());
    };
    if fact.route != destination.route {
        return Ok(());
    }
    let age = (chrono::Utc::now() - fact.verified_at).num_milliseconds();
    if (0..=86_400_000).contains(&age) {
        record_install(&destination, &fact, preferences.generation)?;
    }
    let _guard = state::gate(&dir, true)?;
    let current = state::read_at(&dir)?;
    if current.generation != preferences.generation {
        return Ok(());
    }
    if current.install_reported || !(0..=86_400_000).contains(&age) {
        match std::fs::remove_file(dir.join("pending-install.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn record_install(
    destination: &Destination,
    fact: &VerifiedInstall,
    generation: u64,
) -> anyhow::Result<()> {
    let dir = state::directory()?;
    let preferences = {
        let _guard = state::gate(&dir, true)?;
        let mut preferences = state::read_at(&dir)?;
        if !state::enabled(&preferences, &destination.hub) || preferences.generation != generation {
            return Ok(());
        }
        if preferences
            .acquisition_route
            .as_ref()
            .is_some_and(|route| route != &destination.route)
        {
            return Ok(());
        }
        preferences.acquisition_route = Some(destination.route.clone());
        if preferences.acquisition_id.is_none() && preferences.first_acquisition_account.is_none() {
            preferences.acquisition_id = fact.acquisition_id;
        }
        if let Some(campaign) = &fact.campaign {
            if preferences.campaign_first.is_none() {
                preferences.campaign_first = Some(campaign.clone());
            }
            preferences.campaign_latest = Some(campaign.clone());
        }
        if !preferences.install_reported {
            preferences.channel = fact.channel.clone();
        }
        state::write_json(&dir.join("preferences.json"), &preferences)?;
        preferences
    };
    let Some(id) = preferences.device_id else {
        return Ok(());
    };
    let mut properties = json!({
        "schema_version": 1, "client_type": "cli", "cli_version": fact.version,
        "source": "installer", "ci": fact.ci,
        "channel": preferences.channel, "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH, "installation_verified": true,
        "app_env": if destination.environment == "production" {"production"} else {"development"},
        "deployment_env": destination.environment, "event_id": id,
        "event_ts": fact.verified_at.timestamp_millis(), "$process_person_profile": false, "$geoip_disable": true
    }).as_object().unwrap().clone();
    extend_properties(&preferences, &mut properties);
    let event = Event {
        event: EventName::InstallSucceeded,
        uuid: id,
        distinct_id: format!("cli-installation:{id}"),
        timestamp: fact.verified_at,
        properties,
    };
    if state::debug(&destination.hub) {
        eprintln!("{}", json!({"telemetry_preview": event}));
    } else {
        transport::enqueue(event.clone(), preferences.generation, destination)?;
        if preferences.acquisition_id.is_some() && !preferences.acquisition_reported {
            let mut attributed = event;
            attributed.event = EventName::InstallAttributed;
            attributed.uuid = uuid::Uuid::new_v4();
            attributed
                .properties
                .insert("event_id".into(), json!(attributed.uuid));
            transport::enqueue(attributed, preferences.generation, destination)?;
        }
    }
    Ok(())
}

/// The caller holds the state gate so concurrent first exposures cannot select different routes.
pub(crate) fn bind_route(
    preferences: &mut state::Preferences,
    dir: &std::path::Path,
    destination: &Destination,
) -> anyhow::Result<bool> {
    if let Some(route) = &preferences.acquisition_route {
        return Ok(route == &destination.route);
    }
    preferences.acquisition_route = Some(destination.route.clone());
    state::write_json(&dir.join("preferences.json"), preferences)?;
    Ok(true)
}

/// Authorization URLs carry an opaque correlation key, never a credential-derived identifier.
pub fn authorization_url(raw: &str, hub: &str) -> String {
    let scoped = || -> anyhow::Result<state::Preferences> {
        anyhow::ensure!(
            state::read().is_ok_and(|p| state::enabled(&p, hub)),
            "statistics are off"
        );
        let destination =
            Destination::for_hub(hub).ok_or_else(|| anyhow::anyhow!("no analytics destination"))?;
        let dir = state::directory()?;
        let _guard = state::gate(&dir, true)?;
        let mut preferences = state::read_at(&dir)?;
        anyhow::ensure!(
            state::enabled(&preferences, hub) && preferences.first_acquisition_account.is_none(),
            "acquisition is inactive"
        );
        anyhow::ensure!(
            bind_route(&mut preferences, &dir, &destination)?,
            "different acquisition destination"
        );
        Ok(preferences)
    };
    let Ok(preferences) = scoped() else {
        return raw.into();
    };
    let Some(id) = preferences.device_id else {
        return raw.into();
    };
    let (base, fragment) = raw.split_once('#').unwrap_or((raw, ""));
    let Ok(uri) = base.parse::<http::Uri>() else {
        return raw.into();
    };
    if !matches!(uri.scheme_str(), Some("http" | "https")) {
        return raw.into();
    }
    let separator = if uri.query().is_some() { '&' } else { '?' };
    format!(
        "{base}{separator}installation_id={id}{}",
        if fragment.is_empty() {
            String::new()
        } else {
            format!("#{fragment}")
        }
    )
}

/// The first saved login owns the installation even when its event cannot enter the queue.
pub fn save_login(
    hub: &str,
    credential: &crate::infra::credentials::HubCredential,
) -> anyhow::Result<()> {
    let eligible = || -> anyhow::Result<_> {
        let destination =
            Destination::for_hub(hub).ok_or_else(|| anyhow::anyhow!("no analytics destination"))?;
        anyhow::ensure!(
            state::read().is_ok_and(|p| state::enabled(&p, hub)) && !state::debug(hub),
            "statistics are off"
        );
        let dir = state::directory()?;
        let guard = state::gate(&dir, true)?;
        let preferences = state::read_at(&dir)?;
        anyhow::ensure!(
            state::enabled(&preferences, hub) && !state::debug(hub),
            "statistics are off"
        );
        anyhow::ensure!(
            preferences
                .acquisition_route
                .as_ref()
                .is_none_or(|route| route == &destination.route),
            "different acquisition destination"
        );
        Ok((dir, guard, preferences, destination))
    };
    let state = if state::override_reason(hub).is_none() {
        eligible().ok()
    } else {
        None
    };
    crate::infra::credentials::save(hub, credential)?;
    if let Some((dir, guard, mut preferences, destination)) = state {
        if preferences.first_acquisition_account.is_none() && !preferences.acquisition_completed {
            let account_id = credential.account_id.clone().filter(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            });
            preferences.acquisition_route = Some(destination.route);
            preferences.first_acquisition_account = Some(state::AcquisitionAccount {
                account_id,
                event_id: uuid::Uuid::new_v4(),
                saved_at: chrono::Utc::now(),
                ci: ["CI", "GITHUB_ACTIONS", "GITLAB_CI"]
                    .iter()
                    .any(|name| std::env::var_os(name).is_some()),
            });
            let _ = state::write_json(&dir.join("preferences.json"), &preferences);
        }
        drop(guard);
        let _ = enqueue_pending_link(hub);
        transport::spawn_worker(hub);
    }
    Ok(())
}

pub(crate) fn enqueue_pending_link(hub: &str) -> anyhow::Result<()> {
    let preferences = state::read()?;
    if !state::enabled(&preferences, hub) || preferences.acquisition_completed || state::debug(hub)
    {
        return Ok(());
    }
    let Some(destination) = Destination::for_hub(hub) else {
        return Ok(());
    };
    if preferences.acquisition_route.as_ref() != Some(&destination.route) {
        return Ok(());
    }
    let Some(first) = &preferences.first_acquisition_account else {
        return Ok(());
    };
    let Some(account) = &first.account_id else {
        return Ok(());
    };
    let mut properties = json!({
        "schema_version": 1, "client_type": "cli", "cli_version": super::build_version(),
        "source": "direct", "ci": first.ci, "channel": preferences.channel,
        "user_id": account, "authentication_outcome": "authenticated", "identity_state": "identified",
        "app_env": if destination.environment == "production" {"production"} else {"development"},
        "deployment_env": destination.environment, "event_id": first.event_id,
        "event_ts": first.saved_at.timestamp_millis(), "$process_person_profile": true, "$geoip_disable": true
    }).as_object().unwrap().clone();
    extend_properties(&preferences, &mut properties);
    transport::enqueue(
        Event {
            event: EventName::AcquisitionLinked,
            uuid: first.event_id,
            distinct_id: if destination.environment == "production" {
                account.clone()
            } else {
                format!("{}:{account}", destination.environment)
            },
            timestamp: first.saved_at,
            properties,
        },
        preferences.generation,
        &destination,
    )
}
