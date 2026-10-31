//! Team activity reports: the metadata a device's daemon sends for its organization dashboards.

use serde::{Deserialize, Serialize};

/// One live session as the Hub's dashboard sees it. The type has no field for a title, prompt,
/// path or branch, so a report cannot carry one, and its executor project is required, so a
/// session run outside every project cannot be listed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ReportedSession {
    pub session_id: String,
    pub project_id: String,
    pub runtime: String,
    pub status: &'static str,
    pub last_activity_at: String,
}

/// A Hub's answer to a report.
#[derive(Debug, Deserialize)]
pub(crate) struct ReportAnswer {
    /// How long the Hub wants the daemon to wait before the next report.
    pub next_report_seconds: u64,
    /// Whether the device's owner shares activity with at least one organization on this Hub.
    /// An answer without the field reads as `false`, so a Hub that predates it is never sent
    /// a session.
    #[serde(default)]
    pub sharing: bool,
}

/// The request body of a report that lists `sessions`.
pub(crate) fn report_body(sessions: &[ReportedSession]) -> serde_json::Value {
    serde_json::json!({ "sessions": sessions })
}

impl super::Client {
    /// Report the device's live sessions; the answer says when to report next and whether the
    /// owner shares activity on this Hub.
    pub(crate) fn report_device_activity(
        &self,
        device: &str,
        sessions: &[ReportedSession],
    ) -> crate::Result<ReportAnswer> {
        self.put(
            &format!(
                "api/peer/devices/{}/activity",
                super::client::urlencode(device)
            ),
            &report_body(sessions),
        )
    }
}
