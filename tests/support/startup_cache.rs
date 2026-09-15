//! Local command contracts use a current cache to isolate incidental startup maintenance.

pub fn seed(home: &std::path::Path) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("cli-update.json"),
        serde_json::json!({
            "checked_at": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
            "latest": env!("CARGO_PKG_VERSION"),
        })
        .to_string(),
    )
    .unwrap();
}
