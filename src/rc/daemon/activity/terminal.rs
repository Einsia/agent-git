//! Terminal Claude Code sessions registered for native control, in team activity reports.
//!
//! Such a session runs outside the daemon, so the daemon decides from its folder where it may be
//! listed. It is listed to a Hub only when its folder resolves to exactly one bound project,
//! that project's device-local repository is confirmed to publish to that Hub, and agit uploads
//! the session's turns there. Anything less certain lists it nowhere: a folder in no bound
//! project or in more than one, a spelling of the folder that lies in other bound projects than
//! its canonical path, a destination that cannot be read, a claim or a remote-control row that
//! names something else, and a project policy that is paused, uploads nothing or excludes the
//! session.
//!
//! A session in a folder no Workspace project binds, under either spelling, may instead be
//! listed by the opaque id of the Hub repository its folder's CLI binding uploads it to, on the
//! same terms: exactly one folder binding covers it, agit uploads the session there now
//! ([`crate::commands::project::sole_upload`]), no remote-control row places it in a project, and
//! the binding uploads as the account that owns this device's enrollment on that Hub, under which
//! the report goes out. The reporter sends such a session only to a Hub that says it accepts one.
//!
//! It is reported under an id the daemon minted for its registration, never Claude Code's own
//! session id, and only while that registration is current.

use crate::commands::project::Capture;
use crate::hub::activity::{Place, ReportedSession, is_identifier};
use crate::hub::identity::RemoteIdentity;
use crate::rc::native_claude::TerminalActivity;
use std::path::{Path, PathBuf};

const RUNTIME: &str = "claude-code";

/// The Hub repository a bound project's device-local repository is confirmed to publish to.
#[derive(Debug, Clone)]
pub(super) struct Destination {
    pub identity: RemoteIdentity,
    /// `owner/repo` of the Hub repository and of the device-local repository, either of which a
    /// claim on a session in the project may name.
    pub repositories: [String; 2],
}

/// A remote-control row that records the conversation.
#[derive(Debug, Clone)]
pub(super) struct Row {
    pub project: Option<String>,
    /// The owner explicitly observed the conversation, so the daemon collects its turns.
    pub observed: bool,
}

/// What decides whether a terminal session in a bound project is uploaded to that project's
/// destination, read on the device.
#[derive(Debug, Clone)]
pub(super) struct Publication {
    /// `None` when the project has no confirmed destination or it cannot be read.
    pub destination: Option<Destination>,
    pub capture: Capture,
    pub rows: Vec<Row>,
    /// `owner/repo` of the agit claim on the conversation.
    pub claim: Option<String>,
}

/// What decides whether a terminal session in a folder no Workspace project binds is uploaded,
/// read on the device.
#[derive(Debug, Clone)]
pub(super) struct Folder {
    /// The repository the folder's one CLI binding uploads the session to, when that is certain.
    pub upload: Option<RemoteIdentity>,
    /// The id of the account the binding uploads as, as its policy records it.
    pub account: Option<String>,
    /// The id of the account that owns this device's enrollment on the upload's Hub.
    pub owner: Option<String>,
    pub rows: Vec<Row>,
}

/// The repository a session in an unbound-by-Workspace folder may be listed by: the one its
/// folder uploads it to, unless a remote-control row places the session in a project, the
/// binding uploads as another account than the one the report goes out under, or either account
/// is unknown, or the repository's id is not one the Hub takes.
fn folder_upload(folder: Folder) -> Option<RemoteIdentity> {
    let upload = folder.upload?;
    (folder.account.is_some()
        && folder.account == folder.owner
        && folder.rows.iter().all(|row| row.project.is_none())
        && is_identifier(&upload.agent_id))
    .then_some(upload)
}

/// The projects bound under a folder that `cwd` lies in.
fn containing(cwd: &Path, roots: &[(String, PathBuf)]) -> Vec<String> {
    roots
        .iter()
        .filter(|(_, root)| cwd.starts_with(root))
        .map(|(project, _)| project.clone())
        .collect()
}

/// The one bound project `cwd` lies in. `roots` lists each binding, so a folder bound in two
/// workspaces, or inside another bound folder, resolves to none; so does a folder whose
/// `spelled` form lies in other projects than its canonical path, as one reached through a
/// symlink does.
fn bound_project(cwd: &Path, spelled: &Path, roots: &[(String, PathBuf)]) -> Option<String> {
    let canonical = containing(cwd, roots);
    let [project] = canonical.as_slice() else {
        return None;
    };
    (containing(spelled, roots) == canonical).then(|| project.clone())
}

/// The Hub a session in `project` may be listed to under `publication`.
fn hub_for(project: &str, publication: Publication) -> Option<String> {
    let destination = publication.destination?;
    if publication
        .claim
        .is_some_and(|claim| !destination.repositories.contains(&claim))
        || publication
            .rows
            .iter()
            .any(|row| row.project.as_deref() != Some(project))
    {
        return None;
    }
    let uploaded = match publication.capture {
        Capture::Withheld => false,
        Capture::Uploads(identity) => identity == destination.identity,
        Capture::Unbound => publication.rows.iter().any(|row| row.observed),
    };
    uploaded.then_some(destination.identity.hub)
}

/// Pair each terminal session with the Hub it may be listed to, reading what decides it through
/// `publication` once its folder resolves to a Workspace project, or through `folder` when no
/// Workspace project binds its folder at all. A session with no such Hub is dropped.
pub(super) fn attribute(
    terminals: Vec<TerminalActivity>,
    roots: &[(String, PathBuf)],
    mut publication: impl FnMut(&TerminalActivity, &str) -> Publication,
    mut folder: impl FnMut(&TerminalActivity) -> Folder,
) -> Vec<(String, ReportedSession)> {
    terminals
        .into_iter()
        .filter_map(|terminal| {
            let (hub, place) = if containing(&terminal.cwd, roots).is_empty()
                && containing(&terminal.spelled, roots).is_empty()
            {
                let upload = folder_upload(folder(&terminal))?;
                (upload.hub, Place::AgentId(upload.agent_id))
            } else {
                let project = bound_project(&terminal.cwd, &terminal.spelled, roots)?;
                let hub = hub_for(&project, publication(&terminal, &project))?;
                (hub, Place::ProjectId(project))
            };
            Some((
                hub,
                ReportedSession {
                    session_id: terminal.report_id,
                    place,
                    runtime: RUNTIME.to_owned(),
                    status: terminal.status,
                    last_activity_at: terminal.last_activity_at,
                },
            ))
        })
        .collect()
}

/// The remote-control rows that record `session`.
pub(super) fn rows(roster: &crate::rc::roster::Roster, session: &str) -> Vec<Row> {
    roster
        .sessions
        .iter()
        .filter(|(_, entry)| {
            entry.runtime == RUNTIME
                && (entry.thread_id == session || entry.prior_threads.iter().any(|t| t == session))
        })
        .map(|(logical, entry)| Row {
            project: entry.project_id.clone(),
            observed: roster.observed.contains(logical),
        })
        .collect()
}

/// What decides where `terminal` in `project` publishes, read from this device's state.
pub(super) fn read(terminal: &TerminalActivity, project: &str, rows: Vec<Row>) -> Publication {
    let destination = crate::rc::local_repository::publication_destination(project)
        .ok()
        .flatten()
        .map(|(local, destination)| Destination {
            identity: destination.identity,
            repositories: [destination.repository, local],
        });
    // A claim that cannot be read, or names no repository, names none of the project's.
    let claim = crate::infra::config::store_root()
        .and_then(|root| {
            let store = crate::domain::store::Store::at(root);
            crate::domain::link::get_checked(&store, RUNTIME, &terminal.session)
        })
        .map_or(Some(String::new()), |link| {
            link.map(|link| match (link.owner, link.agent) {
                (Some(owner), Some(agent)) => format!("{owner}/{agent}"),
                _ => String::new(),
            })
        });
    Publication {
        destination,
        capture: crate::commands::project::capture_state(RUNTIME, &terminal.session, &terminal.cwd),
        rows,
        claim,
    }
}

/// What decides whether `terminal`, in a folder no Workspace project binds, is uploaded, read from
/// this device's state.
pub(super) fn folder(terminal: &TerminalActivity, rows: Vec<Row>) -> Folder {
    let Some((upload, account)) = crate::commands::project::sole_upload(
        RUNTIME,
        &terminal.session,
        &terminal.cwd,
        &terminal.spelled,
    ) else {
        return Folder {
            upload: None,
            account: None,
            owner: None,
            rows,
        };
    };
    let owner = crate::rc::cloud::store::load(&upload.hub)
        .ok()
        .flatten()
        .map(|enrollment| enrollment.credential.device.owner.account_id);
    Folder {
        upload: Some(upload),
        account,
        owner,
        rows,
    }
}
