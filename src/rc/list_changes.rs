//! `session.list.changed`: the device tells a controller that the session list of one project
//! changed, so a page reads its list when there is something new rather than on a timer.
//!
//! The event names the project and nothing else. Who hears it is decided per connection when it is
//! sent, by the question `session.list` asks of every row it returns: a connection hears about a
//! change only if its own list shows one of the rows that changed, or, for a folder bound or
//! unbound, if it may read the project. A member therefore never wakes for rows only the owner
//! sees, and in a tick that also changed a row the member sees, that row alone is what reaches them.
//!
//! There is no counter in the event. A counter that advanced for every change would skip over the
//! changes a reader may not see, and the size of each skip would count them.

use crate::protocol::{Frame, method};
use crate::rc::cloud::resources::Session;
use agit_peer::access::{Policy, Principal};
use std::collections::BTreeMap;
use std::sync::Arc;

/// One change to a project's list, described the way `session.list` filters its rows.
#[derive(Clone)]
pub(crate) enum Row {
    /// A row came, went or changed: shown to whoever may read this session.
    Session(Session),
    /// The folder was bound or unbound, so any row of the project may have come or gone.
    Project,
}

/// Who may hear one project's `session.list.changed`: the rows that changed in one tick.
pub(crate) struct Audience {
    project: Option<String>,
    rows: Vec<Row>,
}

/// Frames are logged with `{:?}`; the audience shows how many rows changed, never which ones.
impl std::fmt::Debug for Audience {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audience")
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

impl Audience {
    /// Whether `session.list`, asked by `principal` under `policy`, would show one of the rows
    /// that changed. This is the same per-row question the list's own filter asks, so a reader
    /// hears about exactly the changes their list can show.
    pub(crate) fn heard_by(&self, policy: &Policy, principal: &Principal) -> bool {
        self.rows.iter().any(|row| match row {
            Row::Session(session) => session.access(policy, principal).can_read(),
            Row::Project => self
                .project
                .as_deref()
                .is_some_and(|project| policy.access(principal, None, Some(project)).can_read()),
        })
    }
}

/// The projects whose list changed since the last send, with what changed in each.
#[derive(Default)]
pub(crate) struct Pending(BTreeMap<Option<String>, Vec<Row>>);

impl Pending {
    pub(crate) fn session(&mut self, session: Session) {
        self.0
            .entry(session.project.clone())
            .or_default()
            .push(Row::Session(session));
    }

    pub(crate) fn project(&mut self, project: &str) {
        self.0
            .entry(Some(project.to_owned()))
            .or_default()
            .push(Row::Project);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// One frame per project that changed since the last call, each carrying its audience. A
    /// connection that hears it gets one event per project per send, however many rows changed.
    pub(crate) fn take(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.0)
            .into_iter()
            .map(|(project, rows)| {
                let mut frame = Frame::notification(
                    method::SESSION_LIST_CHANGED,
                    serde_json::json!({
                        "workspace_id": crate::rc::endpoint::WORKSPACE,
                        "project_id": project,
                    }),
                );
                frame.list_audience = Some(Arc::new(Audience { project, rows }));
                frame
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::access::{Access, Resource, Rule};

    fn principal(account: &str) -> Principal {
        Principal {
            issuer: "https://cloud.example".into(),
            account_id: account.into(),
        }
    }

    fn session(id: &str, project: Option<&str>) -> Session {
        Session {
            id: id.into(),
            native_id: id.into(),
            source: None,
            runtime: "codex".into(),
            cwd: "/work".into(),
            project: project.map(str::to_owned),
        }
    }

    /// A member hears a project's change only through a row their own list shows: a tick that
    /// changed only a row they can't read sends them nothing, and the same tick with one of their
    /// rows added sends one event, which that row carries. A filter applied after merging a
    /// project's changes into one event would let the unreadable row wake them; one that checked
    /// the project rather than the row would let any project reader hear about a row denied to
    /// them.
    #[test]
    fn a_member_hears_only_changes_to_rows_their_list_shows() {
        let member = principal("member");
        let policy = Policy::new(
            1,
            vec![
                Rule {
                    principal: member.clone(),
                    resource: Resource::Project("p".into()),
                    access: Access::Read,
                },
                Rule {
                    principal: member.clone(),
                    resource: Resource::Session("private".into()),
                    access: Access::Deny,
                },
            ],
        )
        .unwrap();
        let mut pending = Pending::default();
        pending.session(session("private", Some("p")));
        let frames = pending.take();
        assert_eq!(frames.len(), 1);
        let audience = frames[0].list_audience.as_ref().unwrap();
        assert!(!audience.heard_by(&policy, &member));

        pending.session(session("private", Some("p")));
        pending.session(session("shared", Some("p")));
        let frames = pending.take();
        assert_eq!(
            frames.len(),
            1,
            "one event per project however many rows changed"
        );
        assert!(
            frames[0]
                .list_audience
                .as_ref()
                .unwrap()
                .heard_by(&policy, &member)
        );
        assert_eq!(
            frames[0].params,
            Some(serde_json::json!({"workspace_id": "local-owner", "project_id": "p"})),
            "the event names the project and nothing else"
        );
    }
}
