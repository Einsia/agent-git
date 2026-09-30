//! Profile the device history reader without taking control of a conversation.
#[cfg(feature = "rc")]
fn main() -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    agit::rc::select_local_authority();
    let session = std::env::args().nth(1).expect("session ID is required");
    for sample in 1..=3 {
        let mut timings = agit::rc::local_history::Timings::default();
        let started = std::time::Instant::now();
        let result = agit::rc::local_history::read_timed(
            serde_json::json!({"session_id": session, "view": "conversation"}),
            &mut timings,
        )?;
        let items = &result["items"];
        println!(
            "{}",
            serde_json::json!({"sample": sample, "elapsed_ms": started.elapsed().as_secs_f64()*1000.0,
            "phases": timings, "items": items.as_array().map(Vec::len),
            "digest": format!("{:x}", Sha256::digest(serde_json::to_vec(items)?))})
        );
    }
    Ok(())
}
#[cfg(not(feature = "rc"))]
fn main() {
    panic!("the rc feature is required");
}
