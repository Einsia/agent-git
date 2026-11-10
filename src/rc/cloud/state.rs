//! Each Hub's Cloud connection as this daemon sees it now, for `agit rc status` and
//! `agit rc start --here`. Only fixed classes are kept, never an error's text.

use crate::rc::control::{CloudLine, CloudPresence};
use std::{collections::BTreeMap, sync::Mutex};

/// A Hub's line, with the enrolled device the host serves it with and the device its presence
/// connected as.
struct State {
    line: CloudLine,
    enrolled_device: Option<String>,
    connected_device: Option<String>,
}

static STATES: Mutex<BTreeMap<String, State>> = Mutex::new(BTreeMap::new());

fn update(hub: &str, change: impl FnOnce(&mut State)) {
    let mut states = STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = states.entry(hub.to_owned()).or_insert_with(|| State {
        line: CloudLine {
            hub: hub.to_owned(),
            enrolled: false,
            pending: false,
            presence: CloudPresence::Connecting,
            error: None,
        },
        enrolled_device: None,
        connected_device: None,
    });
    change(state);
}

/// The Hubs the host serves now, each with whether an enrollment is still pending and the device
/// it is enrolled as with inbound on. A Hub no longer served is forgotten.
pub(crate) fn observe(origins: &[(String, bool, Option<String>)]) {
    STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|hub, _| origins.iter().any(|(origin, ..)| origin == hub));
    for (hub, pending, device) in origins {
        update(hub, |state| {
            state.line.pending = *pending;
            state.line.enrolled = device.is_some();
            state.enrolled_device.clone_from(device);
        });
    }
}

/// The presence socket opened as `device`.
pub(crate) fn connected(hub: &str, device: &str) {
    update(hub, |state| {
        state.line.presence = CloudPresence::Connected;
        state.line.error = None;
        state.connected_device = Some(device.to_owned());
    });
}

/// The presence socket closed; `failure` names why when it failed rather than ended.
pub(crate) fn disconnected(hub: &str, failure: Option<&'static str>) {
    update(hub, |state| match failure {
        Some(class) => {
            state.line.presence = CloudPresence::Failed;
            state.line.error = Some(class.to_owned());
        }
        None => state.line.presence = CloudPresence::Connecting,
    });
}

pub(crate) fn enrollment_failed(hub: &str, class: &'static str) {
    update(hub, |state| {
        if state.line.presence != CloudPresence::Connected {
            state.line.presence = CloudPresence::Failed;
        }
        state.line.error = Some(class.to_owned());
    });
}

/// Each Hub's line. A connection made as another device than the one enrolled now (an earlier
/// account's, before the switch reached the presence socket) reads as connecting, not online.
pub(crate) fn lines() -> Vec<CloudLine> {
    STATES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .map(|state| {
            let mut line = state.line.clone();
            if line.presence == CloudPresence::Connected
                && (state.connected_device.is_none()
                    || state.connected_device != state.enrolled_device)
            {
                line.presence = CloudPresence::Connecting;
            }
            line
        })
        .collect()
}

/// The fixed class a presence or enrollment failure is shown under.
pub(crate) fn failure_class(error: &anyhow::Error) -> &'static str {
    if let Some(failure) = error.downcast_ref::<agit_peer::client::HttpFailure>() {
        return match failure.status {
            401 | 403 => "not_known_to_this_hub",
            404 => "hub_without_cloud",
            429 | 500..=599 => "hub_unavailable",
            _ => "hub_refused",
        };
    }
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
    {
        return "timed_out";
    }
    if error.is::<agit_peer::client::PresenceSlept>() {
        return "machine_slept";
    }
    if error.is::<agit_peer::client::RelayRestarting>() {
        return "relay_restarting";
    }
    if error.downcast_ref::<std::io::Error>().is_some() {
        return "local_io";
    }
    "connection_failed"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state is one table per process and `observe` forgets every Hub it is not given, so
    /// the tests that drive it take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What the host saw is what `rc status` shows, per Hub, and a Hub no longer served drops
    /// out. A failure keeps its class until the next connection clears it.
    #[test]
    fn cloud_state_follows_each_hub() {
        let _serial = serial();
        let (a, b) = ("https://state-a.invalid", "https://state-b.invalid");
        observe(&[
            (a.into(), false, Some("dev-a".into())),
            (b.into(), true, None),
        ]);
        connected(a, "dev-a");
        enrollment_failed(b, "not_known_to_this_hub");
        let line = |hub: &str| lines().into_iter().find(|line| line.hub == hub);
        assert_eq!(line(a).unwrap().presence, CloudPresence::Connected);
        let failed = line(b).unwrap();
        assert_eq!(failed.presence, CloudPresence::Failed);
        assert_eq!(failed.error.as_deref(), Some("not_known_to_this_hub"));
        assert!(failed.pending && !failed.enrolled);
        disconnected(a, None);
        assert_eq!(line(a).unwrap().presence, CloudPresence::Connecting);
        connected(b, "dev-b");
        assert_eq!(line(b).unwrap().error, None);
        observe(&[(a.into(), false, Some("dev-a".into()))]);
        assert!(line(b).is_none());
    }

    /// After a switch of account on one Hub, the earlier account's connection is not this
    /// computer being online: the line reads connecting until the presence socket connects as
    /// the device enrolled now.
    #[test]
    fn a_connection_made_as_an_earlier_enrollment_is_not_online() {
        let _serial = serial();
        let hub = "https://state-switch.invalid";
        let line = || lines().into_iter().find(|line| line.hub == hub).unwrap();
        observe(&[(hub.into(), false, Some("account-a".into()))]);
        connected(hub, "account-a");
        assert_eq!(line().presence, CloudPresence::Connected);
        // The new account's request is pending: no device enrolled with inbound on.
        observe(&[(hub.into(), true, None)]);
        assert_eq!(line().presence, CloudPresence::Connecting);
        assert!(line().pending && !line().enrolled);
        observe(&[(hub.into(), false, Some("account-b".into()))]);
        assert_eq!(line().presence, CloudPresence::Connecting);
        connected(hub, "account-b");
        assert_eq!(line().presence, CloudPresence::Connected);
        observe(&[]);
    }
}
