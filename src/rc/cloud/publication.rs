//! Owned devices reconcile publication independently of browser connections and harness execution.

use super::store;
use anyhow::{Context, ensure};
use std::{path::PathBuf, time::Duration};

pub(super) async fn run(hub: String, directory: PathBuf, log: Option<crate::rc::diagnostics::Log>) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cursor = None;
    loop {
        interval.tick().await;
        let (origin, credential_dir, after) = (hub.clone(), directory.clone(), cursor.clone());
        let result = tokio::task::spawn_blocking(move || -> crate::Result<_> {
            let enrollment = store::load_in(&credential_dir, &origin)?
                .context("device enrollment is missing")?;
            ensure!(enrollment.inbound_enabled, "cloud access is disabled");
            let device = enrollment.credential.device;
            let client =
                crate::hub::Client::for_stored_hub_with_timeout(&origin, Duration::from_secs(5));
            let page = client.project_publication_plan(&device.id, after.as_deref())?;
            ensure!(
                crate::rc::daemon::publication::same_device(&page.device, &device)
                    && page.entries.len() <= 32,
                "publication plan device or page changed"
            );
            if let Some(next) = &page.next_cursor {
                ensure!(
                    page.entries
                        .last()
                        .is_some_and(|entry| &entry.web_project_id == next)
                        && after.as_ref().is_none_or(|previous| next > previous),
                    "invalid publication plan cursor"
                );
            }
            Ok(page)
        })
        .await;
        let page = match result {
            Ok(Ok(page)) => page,
            _ => {
                cursor = None;
                record(&log, "cloud.project_publication_plan_unavailable", 0);
                continue;
            }
        };
        cursor = page.next_cursor;
        let mut failures = 0;
        for target in page.entries {
            if let Err(error) = crate::rc::daemon::project_publication::reconcile_owned(
                hub.clone(),
                directory.clone(),
                page.device.clone(),
                target,
            )
            .await
            {
                failures += 1;
                if let Some(log) = &log {
                    log.record("cloud.project_publication_entry_failed", serde_json::json!({
                        "stage": error.downcast_ref::<crate::rc::daemon::project_publication::Stage>()
                            .map(|stage| stage.name()).unwrap_or("unknown"),
                        "http_status": error.downcast_ref::<crate::hub::client::ApiError>()
                            .map(|response| response.status),
                    }));
                }
            }
        }
        if failures > 0 {
            record(&log, "cloud.project_publication_retry", failures);
        }
    }
}

fn record(log: &Option<crate::rc::diagnostics::Log>, event: &str, pending: usize) {
    if let Some(log) = log {
        log.record(event, serde_json::json!({"pending": pending}));
    }
}
