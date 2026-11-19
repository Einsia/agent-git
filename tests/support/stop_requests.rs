//! Observe a direct Stop's durable result before inspecting its saved history.

use agit::domain::{
    stop_requests::{self, Phase},
    store::Store,
};
use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

pub fn finish(command: &mut Command, home: &Path, runtime: &str, session: &str) {
    let store = Store::at(home.join("store"));
    let queue = stop_requests::for_session(&store, runtime, session);
    let pending = queue.pending().unwrap();
    if pending.is_empty() {
        return;
    }
    let key = queue.directory().file_name().unwrap();
    command.args([
        std::ffi::OsStr::new("hooks"),
        std::ffi::OsStr::new("recover"),
        key,
    ]);
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if !queue.owned().unwrap() {
            let output = command.output().unwrap();
            assert!(
                output.status.success() || output.status.code() == Some(4),
                "{output:?}"
            );
            if !queue.owned().unwrap() {
                for id in queue.pending().unwrap() {
                    let request = queue.read::<serde_json::Value>(&id).unwrap();
                    assert!(
                        !matches!(
                            request.phase,
                            Phase::Accepted | Phase::Saving { .. } | Phase::Publishing { .. }
                        ),
                        "unsettled Stop outcome: {:?}",
                        request.phase
                    );
                }
                return;
            }
        }
        assert!(Instant::now() < deadline, "Stop worker did not finish");
        std::thread::sleep(Duration::from_millis(25));
    }
}
