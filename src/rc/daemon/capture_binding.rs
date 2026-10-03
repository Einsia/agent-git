//! Native capture adoption is independent of launching or controlling its writer.

use super::*;

pub(super) struct Request {
    pub logical: String,
    pub runtime: String,
    pub native: String,
    pub native_source: Option<crate::protocol::NativeSourceRef>,
    pub cwd: PathBuf,
    pub project: Option<(String, PathBuf)>,
    pub prior_entry: Option<roster::Entry>,
    pub prior_capture: Option<crate::rc::capture::RepositoryKind>,
    pub wire: Option<crate::rc::lineage::AgitSession>,
    pub authority: crate::rc::authority::Guard,
    pub settlement: tokio::sync::watch::Receiver<SettlementState>,
}

impl Request {
    pub async fn resolve(self) -> Result<Option<crate::rc::lineage::AgitSession>, RpcError> {
        let native = self.native.clone();
        let native = self
            .native_source
            .as_ref()
            .map(|source| source.session_ref(&native))
            .unwrap_or(native);
        let runtime = self.runtime.clone();
        let cwd = self.cwd.clone();
        let entry = self.prior_entry.clone();
        let saved = self.prior_capture.clone();
        let wire = self.wire.clone();
        let mut lineage = tokio::task::spawn_blocking(move || -> crate::Result<_> {
            let prior = match entry.as_ref() {
                Some(entry)
                    if entry.agit_session.is_some() || entry.expected_agent_id.is_some() =>
                {
                    Some(crate::rc::lineage::AgitSession::parse(
                        entry
                            .agit_session
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("incomplete capture route"))?,
                        entry
                            .expected_agent_id
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("incomplete capture identity"))?,
                    )?)
                }
                _ => None,
            };
            let resolved = crate::rc::capture::resolve(
                &runtime,
                &native,
                &cwd,
                prior.as_ref(),
                saved.as_ref(),
            )?;
            if let Some(wire) = wire {
                anyhow::ensure!(
                    resolved
                        .as_ref()
                        .is_some_and(|local| local.to_string() == wire.to_string()
                            && local.agent_id() == wire.agent_id()),
                    "requested capture conflicts with the local native claim"
                );
            }
            Ok(resolved)
        })
        .await
        .map_err(|_| RpcError::new(ErrorCode::Internal, "capture inspection failed"))?
        .map_err(|_| {
            let mut error = RpcError::new(
                ErrorCode::Forbidden,
                "native capture binding is invalid or conflicting",
            );
            error.data = Some(
                serde_json::json!({"publication":crate::protocol::SessionPublicationStatus {
                    readiness: crate::protocol::PublicationReadiness::SetupRequired,
                    progress: crate::protocol::PublicationProgress::Idle,
                    stage: Some(crate::protocol::PublicationStage::Capture),
                    reason: Some(crate::protocol::PublicationReason::BindingInvalid),
                }}),
            );
            error
        })?;
        if lineage.is_none()
            && let Some((project_id, project)) = self.project.clone()
        {
            self.authority.check()?;
            self.authority.check_project(&project_id, &project)?;
            let logical = self.logical.clone();
            let cwd = self.cwd.clone();
            let proposal = tokio::task::spawn_blocking(move || -> crate::Result<_> {
                anyhow::ensure!(
                    cwd.canonicalize()?.starts_with(project.canonicalize()?),
                    "native capture is outside its bound project"
                );
                let repository =
                    crate::rc::local_repository::ensure_repository(&project_id, &project)?;
                let mut proposal = crate::rc::lineage::AgitSession::new(
                    &repository.slug,
                    &repository.agent_id,
                    &format!("desktop-{}", logical.trim_start_matches("agit-")),
                )?;
                proposal.capture = Some(crate::rc::capture::RepositoryKind::DeviceLocal {
                    agent_id: repository.agent_id,
                });
                Ok(proposal)
            })
            .await
            .map_err(|_| RpcError::new(ErrorCode::Internal, "capture preparation failed"))?
            .map_err(|_| {
                RpcError::new(
                    ErrorCode::Internal,
                    "project capture repository is unavailable",
                )
            })?;
            let native = self.native.as_str();
            let mut args = crate::commands::rc::land_argv(
                &proposal.slug(),
                proposal.agent_id(),
                proposal.branch(),
                &self.runtime,
                native,
                &self.cwd.to_string_lossy(),
            );
            args.extend(["--local-owner".into(), "--adopt-unclaimed".into()]);
            if let Some(source) = &self.native_source {
                args.extend([
                    "--source-id".into(),
                    source.source_id.clone(),
                    "--source-generation".into(),
                    source.generation.to_string(),
                ]);
            }
            let exe = std::env::current_exe().map_err(|_| {
                RpcError::new(ErrorCode::Internal, "capture program is unavailable")
            })?;
            let mut command =
                tokio::process::Command::from(crate::infra::background::command(&exe));
            command
                .args(args)
                .env(
                    crate::rc::capture::CAPTURE_ENV,
                    serde_json::to_string(
                        proposal
                            .capture
                            .as_ref()
                            .expect("capture kind was assigned"),
                    )
                    .expect("capture kind serializes"),
                )
                .env_remove("AGIT_SESSION")
                .env_remove("AGIT_MERGE_TX")
                .env_remove(crate::hub::identity::EXPECTED_AGENT_ID_ENV)
                .env_remove(crate::rc::harness::SUPERVISED_HOOK_ENV)
                .env_remove(crate::commands::commit::archive::NATIVE_ENV)
                .env_remove(crate::commands::commit::archive::ROLE_ENV);
            // The subprocess owns branch and native-claim locks through adoption.
            let mut settlement = self.settlement.clone();
            let lease = *settlement.borrow();
            let output = crate::rc::supervisor::guarded_output(&mut settlement, lease, command)
                .await
                .ok_or_else(|| {
                    RpcError::new(ErrorCode::SessionBusy, "capture adoption was interrupted")
                })?;
            if !output.status.success() {
                return Err(RpcError::new(
                    ErrorCode::Forbidden,
                    "native capture could not be adopted without changing its existing claim",
                ));
            }
            self.authority.check()?;
            lineage = Some(proposal);
        }
        Ok(lineage)
    }
}
