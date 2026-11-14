//! Owned devices reconcile publication independently of browser connections and harness execution.

use super::store;
use anyhow::{Context, ensure};
use std::{collections::HashMap, path::PathBuf, time::Duration};
use tokio::time::Instant;

/// How long a plan entry that failed waits before it is tried again, doubling on each further
/// failure of the same class up to [`LAST_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(60);
const LAST_RETRY: Duration = Duration::from_secs(30 * 60);

/// Plan entries that failed, by project and the repository its binding names, with the class of
/// the last failure and when the entry is next tried. A binding that changes is a new entry and
/// is tried at once; so is one that fails for another reason than before. Without it, an entry
/// that cannot succeed is retried, with its Hub requests, on every pass.
#[derive(Default)]
struct Retries {
    held: HashMap<(String, String), (&'static str, u32, Instant)>,
}

impl Retries {
    fn due(&mut self, project: &str, repository: &str, now: Instant) -> bool {
        self.held
            .retain(|(held, bound), _| held != project || bound == repository);
        self.held
            .get(&(project.to_owned(), repository.to_owned()))
            .is_none_or(|(_, _, next)| *next <= now)
    }

    fn failed(&mut self, project: &str, repository: &str, class: &'static str, now: Instant) {
        let key = (project.to_owned(), repository.to_owned());
        let failures = match self.held.get(&key) {
            Some((last, failures, _)) if *last == class => failures.saturating_add(1),
            _ => 1,
        };
        let wait = FIRST_RETRY
            .saturating_mul(2u32.saturating_pow(failures - 1))
            .min(LAST_RETRY);
        self.held.insert(key, (class, failures, now + wait));
    }

    fn succeeded(&mut self, project: &str, repository: &str) {
        self.held
            .remove(&(project.to_owned(), repository.to_owned()));
    }
}

pub(super) async fn run(hub: String, directory: PathBuf, log: Option<crate::rc::diagnostics::Log>) {
    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cursor = None;
    let mut retries = Retries::default();
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
            let (project, repository) =
                (target.web_project_id.clone(), target.repository_id.clone());
            if !retries.due(&project, &repository, Instant::now()) {
                continue;
            }
            match crate::rc::daemon::project_publication::reconcile_owned(
                hub.clone(),
                directory.clone(),
                page.device.clone(),
                target,
            )
            .await
            {
                Ok(()) => retries.succeeded(&project, &repository),
                Err(error) => {
                    failures += 1;
                    let class = crate::rc::daemon::project_publication::Failure::of(&error).name();
                    retries.failed(&project, &repository, class, Instant::now());
                    if let Some(log) = &log {
                        log.record("cloud.project_publication_entry_failed", serde_json::json!({
                            "stage": error.downcast_ref::<crate::rc::daemon::project_publication::Stage>()
                                .map(|stage| stage.name()).unwrap_or("unknown"),
                            "failure": class,
                            "http_status": error.downcast_ref::<crate::hub::client::ApiError>()
                                .map(|response| response.status),
                            // Which publication check refused it, by its fixed name.
                            "check": crate::commands::push::PublicationCheck::of(&error)
                                .map(crate::commands::push::PublicationCheck::name),
                        }));
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan entry that keeps failing the same way is tried after a minute, then twice as long
    /// each time up to half an hour, so a binding that cannot succeed stops costing a pass of Hub
    /// requests every few seconds. A changed binding or a new kind of failure starts over, and a
    /// success forgets the entry, so a fix the person makes is picked up at once.
    #[test]
    fn a_failing_plan_entry_waits_longer_each_time_and_a_change_is_tried_at_once() {
        let start = Instant::now();
        let mut retries = Retries::default();
        assert!(retries.due("p", "r1", start));
        let mut waits = Vec::new();
        let mut at = start;
        for _ in 0..7 {
            retries.failed("p", "r1", "other", at);
            let next = retries.held[&("p".to_owned(), "r1".to_owned())].2;
            assert!(!retries.due("p", "r1", next - Duration::from_secs(1)));
            assert!(retries.due("p", "r1", next));
            waits.push((next - at).as_secs() / 60);
            at = next;
        }
        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30]);

        retries.failed("p", "r1", "not_writable", at);
        assert_eq!(retries.held[&("p".to_owned(), "r1".to_owned())].1, 1);
        assert!(
            retries.due("p", "r2", at),
            "a changed binding is tried at once"
        );
        assert!(
            !retries
                .held
                .contains_key(&("p".to_owned(), "r1".to_owned()))
        );
        retries.failed("p", "r2", "other", at);
        retries.succeeded("p", "r2");
        assert!(retries.due("p", "r2", at));
    }
}
