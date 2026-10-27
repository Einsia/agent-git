//! Catalog reads release the daemon lock before touching native or cached state.

use super::*;
use crate::rc::runtime_catalog::{Catalog, ListRequest, SettingsRequest};

pub(super) fn handles(method_name: &str) -> bool {
    matches!(
        method_name,
        method::SESSION_CATALOG_LIST | method::SESSION_CATALOG_SETTINGS
    )
}

pub(super) struct Prepared {
    workspace_roots: policy::CanonicalRoots,
    roots: policy::CanonicalRoots,
    request: Request,
}

enum Request {
    List(ListRequest),
    Settings(SettingsRequest),
}

impl Daemon {
    pub(super) fn prepare_catalog(&self, frame: &Frame) -> Result<Prepared, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        if !self.mirror.has_workspace(&caller.workspace_id) {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                "workspace is not bound on this machine",
            ));
        }
        let workspace_roots = self.mirror.roots(&caller.workspace_id);
        let (request, named_workspace, project) = match frame.method() {
            method::SESSION_CATALOG_LIST => {
                let request: ListRequest = frame.params_as()?;
                request
                    .validate()
                    .map_err(|error| RpcError::new(ErrorCode::MalformedFrame, error.to_string()))?;
                let workspace = request.workspace_id.clone();
                let project = request.project_id.clone();
                (Request::List(request), workspace, project)
            }
            method::SESSION_CATALOG_SETTINGS => {
                let request: SettingsRequest = frame.params_as()?;
                let workspace = request.workspace_id.clone();
                (Request::Settings(request), workspace, None)
            }
            _ => return Err(RpcError::new(ErrorCode::Internal, "not a catalog read")),
        };
        if named_workspace.is_some_and(|id| id != caller.workspace_id) {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                "workspace is not bound on this machine",
            ));
        }
        let roots = match project {
            Some(id) => policy::CanonicalRoots::from_verified(vec![
                self.mirror
                    .project_path(&caller.workspace_id, &id)
                    .ok_or_else(|| {
                        RpcError::new(
                            ErrorCode::WorkspaceNotFound,
                            "project is not bound in this workspace",
                        )
                    })?,
            ]),
            None => workspace_roots.clone(),
        };
        Ok(Prepared {
            workspace_roots,
            roots,
            request,
        })
    }

    pub(super) fn finish_catalog(
        &self,
        frame: &Frame,
        roots: &policy::CanonicalRoots,
    ) -> Result<(), RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        if !self.mirror.has_workspace(&caller.workspace_id)
            || self.mirror.roots(&caller.workspace_id) != *roots
        {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                "workspace folders changed during discovery; refresh the catalog",
            ));
        }
        Ok(())
    }
}

impl Prepared {
    pub(super) async fn execute(
        self,
        daemon: Arc<Mutex<Daemon>>,
        frame: Frame,
        epoch: u64,
    ) -> Result<serde_json::Value, RpcError> {
        let roots = self.workspace_roots;
        let result = tokio::task::spawn_blocking(move || {
            frame.authority.check()?;
            let catalog = Catalog::open().map_err(|error| failed(error, false))?;
            let value = match self.request {
                Request::List(request) => serde_json::to_value(
                    catalog
                        .list(&self.roots, &request)
                        .map_err(|error| failed(error, false))?,
                )
                .map_err(internal)?,
                // The request names a source generation, so a changed source never matches it.
                Request::Settings(request) => catalog
                    .settings(&self.roots, &request)
                    .map_err(|error| failed(error, true))?,
            };
            Ok::<_, RpcError>((frame, value))
        })
        .await
        .map_err(|_| RpcError::new(ErrorCode::Internal, "catalog worker failed"))??;
        let state = daemon.lock().await;
        if !connection_epoch_is_current(&state.settlement, epoch) {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "connection changed during catalog read",
            ));
        }
        state.finish_catalog(&result.0, &roots)?;
        Ok(result.1)
    }
}

/// The reply to a catalog failure names no cause, so the daemon log does. A cause that persists is
/// written once rather than once per request.
fn log_cause(what: &str, error: &dyn std::fmt::Display) {
    static LAST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let line = format!("agitd: {what}: {error:#}");
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if last.as_deref() != Some(line.as_str()) {
        eprintln!("{line}");
        *last = Some(line);
    }
}

/// The catalog of this device's own conversations cannot be opened, and a rebuild in this run
/// failed too or was already spent, so nothing in it can be listed or resolved. Restarting
/// `agit rc` allows one more rebuild; one that keeps failing points at the device's disk.
fn unreadable(error: &anyhow::Error) -> RpcError {
    log_cause("the runtime catalog cannot be opened", error);
    classified(
        RpcError::new(
            ErrorCode::RuntimeUnavailable,
            "this device's conversation catalog cannot be opened",
        )
        .with_hint(
            "restart `agit rc` on this device, which rebuilds the catalog from the runtimes' own records; if it keeps failing, check the device's disk",
        ),
        false,
        "catalog_unreadable",
    )
}

fn internal(error: impl std::fmt::Display) -> RpcError {
    log_cause("the runtime catalog failed", &error);
    classified(
        RpcError::new(
            ErrorCode::Internal,
            "runtime catalog is unavailable; inspect local source status",
        ),
        false,
        "catalog_unavailable",
    )
}

fn failed(error: anyhow::Error, request_names_source: bool) -> RpcError {
    known(&error, request_names_source).unwrap_or_else(|| internal(error))
}

/// Catalog conditions with their own meaning: a vanished conversation, a source the catalog is
/// still catching up with, and a transcript or index that is busy for a moment.
pub(super) fn known(error: &anyhow::Error, request_names_source: bool) -> Option<RpcError> {
    use crate::rc::runtime_catalog::{SourceChanged, Unavailable, Unreadable};
    let busy = |message: &str, reason: &str| {
        Some(classified(
            RpcError::new(ErrorCode::SessionBusy, message),
            true,
            reason,
        ))
    };
    if error
        .chain()
        .any(|cause| cause.is::<crate::adapter::codex_rollout::Restoring>())
    {
        return busy(
            "conversation history is being prepared; retry shortly",
            "transcript_restoring",
        );
    }
    if let Some(changed) = error.downcast_ref::<SourceChanged>() {
        if request_names_source {
            return Some(classified(
                RpcError::new(ErrorCode::SessionBusy, changed.to_string()),
                false,
                "source_changed",
            ));
        }
        return busy(&changed.to_string(), "catalog_refreshing");
    }
    if error.downcast_ref::<Unavailable>().is_some() {
        return Some(RpcError::new(
            ErrorCode::SessionNotFound,
            "conversation is unavailable in its registered source or bound project",
        ));
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(rusqlite::Error::sqlite_error_code)
            .is_some_and(|code| {
                matches!(
                    code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
            })
    }) {
        return busy("the runtime catalog is busy; retry shortly", "catalog_busy");
    }
    if error.downcast_ref::<Unreadable>().is_some() {
        return Some(unreadable(error));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog that is catching up with its source clears by itself, a request that names an
    /// old source never does, a vanished conversation is not an internal fault, and a catalog
    /// that cannot be opened is the device owner's to fix. Folding these into one code leaves
    /// the web either retrying forever, giving up on a transient, or asking the wrong person.
    #[test]
    fn catalog_conditions_keep_their_codes_and_say_whether_they_clear() {
        use crate::rc::runtime_catalog::Unavailable;
        let changed = || {
            anyhow::Error::from(crate::rc::runtime_catalog::SourceChanged(
                "conversation source changed",
            ))
        };
        let refreshing = known(&changed(), false).unwrap();
        assert_eq!(refreshing.code, ErrorCode::SessionBusy as i32);
        assert_eq!(refreshing.data.unwrap()["retryable"], true);
        let stale = known(&changed(), true).unwrap();
        assert_eq!(stale.code, ErrorCode::SessionBusy as i32);
        assert_eq!(
            stale.data,
            Some(serde_json::json!({"retryable": false, "reason": "source_changed"}))
        );
        let missing = anyhow::anyhow!("transcript is missing").context(Unavailable);
        assert_eq!(
            known(&missing, false).unwrap().code,
            ErrorCode::SessionNotFound as i32
        );
        let restoring =
            anyhow::Error::from(crate::adapter::codex_rollout::Restoring).context(Unavailable);
        assert_eq!(
            known(&restoring, false).unwrap().data.unwrap()["reason"],
            "transcript_restoring"
        );
        let unreadable = anyhow::anyhow!("file is not a database")
            .context(crate::rc::runtime_catalog::Unreadable);
        let unreadable = known(&unreadable, false).unwrap();
        assert_eq!(unreadable.code, ErrorCode::RuntimeUnavailable as i32);
        let data = unreadable.data.unwrap();
        assert_eq!(
            (&data["retryable"], &data["reason"]),
            (
                &serde_json::json!(false),
                &serde_json::json!("catalog_unreadable")
            )
        );
        assert!(
            data["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("agit rc"))
        );
        assert!(known(&anyhow::anyhow!("unexplained"), false).is_none());
    }

    #[tokio::test]
    async fn catalog_reads_revalidate_workspace_and_reject_wire_selected_roots() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
        let mut state = daemon.lock().await;
        state.mirror.bind("ws-a", "project", root.path()).unwrap();
        let mut frame = Frame::request(
            method::SESSION_CATALOG_LIST,
            serde_json::json!({"workspace_id":"ws-a","project_id":"project"}),
        );
        frame.caller = Some(crate::protocol::CallerClaim {
            account_id: None,
            username: None,
            role: "viewer".into(),
            workspace_id: "ws-a".into(),
        });
        let prepared = state.prepare_catalog(&frame).unwrap();
        assert_eq!(prepared.roots[0], root.path().canonicalize().unwrap());
        assert!(
            state
                .finish_catalog(&frame, &prepared.workspace_roots)
                .is_ok()
        );
        state.mirror.bind("ws-a", "project", other.path()).unwrap();
        assert!(
            state
                .finish_catalog(&frame, &prepared.workspace_roots)
                .is_err()
        );
        frame.params.as_mut().unwrap()["home"] = serde_json::json!(root.path());
        assert!(state.prepare_catalog(&frame).is_err());
        frame.params = Some(serde_json::json!({"workspace_id":"ws-b"}));
        assert!(state.prepare_catalog(&frame).is_err());
    }
}
