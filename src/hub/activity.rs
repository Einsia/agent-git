//! Team activity reports: the metadata a device's daemon sends for its organization dashboards.

use serde::{Deserialize, Serialize};

/// One live session as the Hub's dashboard sees it. The type has no field for a title, prompt,
/// path or branch, so a report cannot carry one, and it names exactly one of an executor project
/// or a Hub repository, so a session run outside both cannot be listed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ReportedSession {
    pub session_id: String,
    #[serde(flatten)]
    pub place: Place,
    pub runtime: String,
    pub status: &'static str,
    pub last_activity_at: String,
}

/// Where a reported session runs, as its body names it: `project_id` or `agent_id`, never both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Place {
    /// A Workspace project's executor project.
    ProjectId(String),
    /// The opaque id of the Hub repository a folder bound through the CLI is confirmed to
    /// upload to. Only a Hub that answered [`ReportAnswer::accepts_agent_id`] is sent one.
    /// Only terminal sessions registered for native control, on Unix, are named so.
    #[cfg_attr(not(unix), allow(dead_code))]
    AgentId(String),
}

/// Whether `value` passes the Hub's identifier rule, which refuses the whole report otherwise.
#[cfg(unix)]
pub(crate) fn is_identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b))
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
    /// Whether the Hub accepts a session named by `agent_id`. An answer without the field reads
    /// as `false`: a Hub that predates it refuses an unknown field, and with it the whole report.
    #[serde(default)]
    pub accepts_agent_id: bool,
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
