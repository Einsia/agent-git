//! Remote requests update bounded counters; only the background worker accesses telemetry files.

use super::{Context, EventName, RUN, emit, maybe_upload, schema};
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

static COUNTERS: OnceLock<Option<Arc<Counters>>> = OnceLock::new();
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

struct Counters(BTreeMap<&'static str, AtomicU64>);

impl Counters {
    fn new() -> Self {
        Self(
            schema::captured_rc_methods()
                .map(|method| (method, AtomicU64::new(0)))
                .collect(),
        )
    }

    fn record(&self, method: &str) {
        if let Some(count) = self.0.get(method) {
            let _ = count.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(1))
            });
        }
    }

    fn drain(&self, mut persist: impl FnMut(&'static str, u64)) -> bool {
        let mut any = false;
        for (method, counter) in &self.0 {
            let count = counter.swap(0, Ordering::Relaxed);
            if count > 0 {
                any = true;
                persist(method, count);
            }
        }
        any
    }

    fn persist(&self, context: &Context) -> bool {
        self.drain(|method, count| {
            emit(
                context,
                EventName::Integration,
                json!({"integration":"rc_request", "rpc_method":method,
                    "outcome":"received", "actor_kind":"remote_operator",
                    "integration_count":count})
                .as_object()
                .unwrap()
                .clone(),
            );
        })
    }
}

fn start(context: Context) -> Option<Arc<Counters>> {
    let counters = Arc::new(Counters::new());
    let worker = counters.clone();
    std::thread::Builder::new()
        .name("agit-rc-telemetry".into())
        .spawn(move || {
            loop {
                std::thread::sleep(FLUSH_INTERVAL);
                // The captured identity and consent generation fence every delayed write.
                if worker.persist(&context) {
                    maybe_upload(&context.hub);
                }
            }
        })
        .ok()?;
    Some(counters)
}

pub(super) fn record(method: &str) {
    let Some(method) = schema::rc_method(method) else {
        return;
    };
    let counters = if let Some(counters) = COUNTERS.get() {
        counters
    } else {
        let context = RUN
            .try_lock()
            .ok()
            .and_then(|run| run.as_ref().and_then(|run| run.context.clone()));
        let Some(context) = context else { return };
        COUNTERS.get_or_init(|| start(context))
    };
    if let Some(counters) = counters {
        counters.record(method);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{state, transport::Destination};
    use std::sync::mpsc;

    #[test]
    fn request_counters_remain_available_while_persistence_is_blocked() {
        let counters = Arc::new(Counters::new());
        let capacity = counters.0.len();
        counters.record("terminal.input");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = counters.clone();
        let persister = std::thread::spawn(move || {
            worker.drain(|method, count| {
                assert_eq!((method, count), ("terminal.input", 1));
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let recorder = counters.clone();
        let (recorded_tx, recorded_rx) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            for _ in 0..10_000 {
                recorder.record("terminal.input");
            }
            recorder.record("untrusted-method-does-not-allocate-a-counter");
            recorded_tx.send(()).unwrap();
        });
        let recorded = recorded_rx.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).unwrap();
        producer.join().unwrap();
        persister.join().unwrap();
        recorded.expect("recording cannot wait for persistence");
        assert_eq!(counters.0.len(), capacity);
        assert!(counters.drain(|method, count| {
            assert_eq!((method, count), ("terminal.input", 10_000));
        }));
        assert!(!counters.drain(|_, _| panic!("counts must drain exactly once")));
        counters.0["terminal.input"].store(u64::MAX, Ordering::Relaxed);
        counters.record("terminal.input");
        counters.drain(|_, count| assert_eq!(count, u64::MAX));
    }

    #[test]
    fn delayed_counts_merge_and_cannot_survive_disable_or_a_new_consent_generation() {
        const CHILD: &str = "AGIT_TEST_RC_TELEMETRY_CHILD";
        const COMPLETE: &str = "delayed RC telemetry verified";
        if std::env::var_os(CHILD).is_none() {
            // Concurrent test forks can inherit the gate and make a later nonblocking enqueue fail.
            let home = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "telemetry::rc::tests::delayed_counts_merge_and_cannot_survive_disable_or_a_new_consent_generation",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("AGIT_HOME", home.path())
                .env_remove("AGIT_TELEMETRY_DISABLED")
                .env_remove("AGIT_TELEMETRY_DEFER")
                .env_remove("DO_NOT_TRACK")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains(COMPLETE));
            return;
        }
        let preferences = state::choose(
            state::Preference::Enabled,
            state::DecisionSource::ExplicitEnable,
            false,
        )
        .unwrap();
        let context = Context {
            properties: json!({"invocation_id":"synthetic-invocation"})
                .as_object()
                .unwrap()
                .clone(),
            distinct_id: "synthetic-anonymous".into(),
            generation: preferences.generation,
            destination: Destination {
                hub: "http://127.0.0.1:9".into(),
                url: "http://127.0.0.1:9/batch/".into(),
                key: "synthetic".into(),
                route: "synthetic".into(),
                environment: "development",
            },
            debug: false,
            protocol: true,
            hub: "http://127.0.0.1:9".into(),
            account: None,
            consent_device: preferences.device_id,
            identity_state: "signed_out",
        };
        let counters = Counters::new();
        let queue = state::directory().unwrap().join("queue.json");
        for _ in 0..20 {
            counters.record("terminal.input");
        }
        assert!(!queue.exists(), "request counting must not touch the queue");
        counters.persist(&context);
        for _ in 0..5 {
            counters.record("terminal.input");
        }
        counters.persist(&context);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&queue).unwrap()).unwrap();
        assert_eq!(saved["entries"].as_array().unwrap().len(), 1);
        assert_eq!(
            saved["entries"][0]["event"]["properties"]["integration_count"],
            25
        );

        counters.record("terminal.input");
        state::choose(
            state::Preference::Disabled,
            state::DecisionSource::ExplicitDisable,
            false,
        )
        .unwrap();
        counters.persist(&context);
        assert!(!queue.exists());
        counters.record("terminal.input");
        state::choose(
            state::Preference::Enabled,
            state::DecisionSource::ExplicitEnable,
            false,
        )
        .unwrap();
        counters.persist(&context);
        assert!(!queue.exists());
        assert!(!counters.drain(|_, _| panic!("stale counts must be discarded")));
        println!("{COMPLETE}");
    }
}
