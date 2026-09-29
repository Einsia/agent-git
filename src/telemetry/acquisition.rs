//! Scoped acquisition keys join verified installation to the first completed login.

use super::{Destination, Event, EventName, state, transport};
use serde_json::{Map, Value, json};

use state::RECEIPT_WINDOW_MS;

/// Which process recorded an installation's single receipt.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Installer,
    FirstCommand,
}

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
    record_install(
        &destination,
        &fact,
        state::read()?.generation,
        Origin::Installer,
    )?;
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
    if (0..=RECEIPT_WINDOW_MS).contains(&age) {
        record_install(
            &destination,
            &fact,
            preferences.generation,
            Origin::Installer,
        )?;
    }
    let _guard = state::gate(&dir, true)?;
    let mut current = state::read_at(&dir)?;
    if current.generation != preferences.generation {
        return Ok(());
    }
    let expired = !(0..=RECEIPT_WINDOW_MS).contains(&age);
    // An expired pending receipt proves the installation is not new; once it is gone, only the
    // marker keeps a first command from reporting the installation with a fresh timestamp.
    if expired && !current.install_reported && !current.first_command_checked {
        current.first_command_checked = true;
        state::write_json(&dir.join("preferences.json"), &current)?;
    }
    if current.install_reported || expired {
        match std::fs::remove_file(dir.join("pending-install.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// An installation whose installers recorded no receipt, such as `npx`, skipped install scripts
/// or a package manager that does not run them, reports it once from its first user command.
/// The caller admits only user-initiated invocations. Pending admission runs first and wins, so
/// an installer's timestamp and acquisition key survive. The marker is saved before the receipt
/// is queued, under the same gate hold, so state that cannot be written sends nothing and a
/// concurrent sender cannot take the gate between the two. The gate is never waited for: a
/// command that finds it busy leaves the marker unset and the next command tries again.
pub(crate) fn first_command(hub: &str, generation: u64) -> anyhow::Result<()> {
    let unchecked = |preferences: &state::Preferences| {
        state::enabled(preferences, hub)
            && preferences.generation == generation
            && preferences.device_id.is_some()
            && !preferences.install_reported
            && !preferences.first_command_checked
    };
    // Every later command returns here without taking the gate.
    if !unchecked(&state::read()?) {
        return Ok(());
    }
    let Some(destination) = Destination::for_hub(hub) else {
        return Ok(());
    };
    let dir = state::directory()?;
    let now = chrono::Utc::now();
    let _guard = state::gate(&dir, false)?;
    let mut preferences = state::read_at(&dir)?;
    if !unchecked(&preferences)
        || std::fs::symlink_metadata(dir.join("pending-install.json")).is_ok()
    {
        return Ok(());
    }
    // A debug run only previews, so the marker stays unset for the run that queues the receipt.
    if !state::debug(hub) {
        preferences.first_command_checked = true;
        state::write_json(&dir.join("preferences.json"), &preferences)?;
    }
    let recent = preferences.created_at.is_some_and(|created| {
        (0..=RECEIPT_WINDOW_MS).contains(&(now - created).num_milliseconds())
    });
    if !recent {
        return Ok(());
    }
    let mut fact = VerifiedInstall::current(&destination);
    fact.verified_at = now;
    fact.acquisition_id = None;
    fact.campaign = None;
    if fact.channel == "unknown" {
        fact.channel = preferences.channel;
    }
    record_locked(&dir, &destination, &fact, generation, Origin::FirstCommand)
}

fn record_install(
    destination: &Destination,
    fact: &VerifiedInstall,
    generation: u64,
    origin: Origin,
) -> anyhow::Result<()> {
    let dir = state::directory()?;
    let _guard = state::gate(&dir, true)?;
    record_locked(&dir, destination, fact, generation, origin)
}

/// The caller holds the state gate.
fn record_locked(
    dir: &std::path::Path,
    destination: &Destination,
    fact: &VerifiedInstall,
    generation: u64,
    origin: Origin,
) -> anyhow::Result<()> {
    let preferences = {
        let mut preferences = state::read_at(dir)?;
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
        "receipt_origin": match origin {Origin::Installer => "installer", Origin::FirstCommand => "first_command"},
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
        transport::enqueue_locked(dir, event.clone(), preferences.generation, destination)?;
        if preferences.acquisition_id.is_some() && !preferences.acquisition_reported {
            let mut attributed = event;
            attributed.event = EventName::InstallAttributed;
            attributed.uuid = uuid::Uuid::new_v4();
            attributed
                .properties
                .insert("event_id".into(), json!(attributed.uuid));
            transport::enqueue_locked(dir, attributed, preferences.generation, destination)?;
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
    save_login_with(hub, credential, || {
        crate::infra::credentials::save(hub, credential).map(|()| true)
    })
    .map(drop)
}

/// [`save_login`] with `save` in place of saving the credentials outright. `save` returns whether
/// it saved them, and only a login it saved counts as the installation's first. It can run while
/// this process holds the telemetry state lock, so it must not wait for any lock whose holder
/// can wait for that one.
pub fn save_login_with(
    hub: &str,
    credential: &crate::infra::credentials::HubCredential,
    save: impl FnOnce() -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
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
    if !save()? {
        return Ok(false);
    }
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
    Ok(true)
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
