use agit::domain::{meta, privacy_publication::MAX_INPUT_BYTES, repo::Repo};

/// The malformed body must be rejected from its object size before envelope parsing.
pub fn commit_oversized_event(repo: &Repo) {
    let event = "e".repeat(40);
    let path = repo.root().join(meta::event_path(&event).unwrap());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::File::create(path)
        .unwrap()
        .set_len(MAX_INPUT_BYTES as u64 + 1)
        .unwrap();
    std::fs::write(repo.root().join(meta::LOG_FILE), format!("{event}\n")).unwrap();
    std::fs::write(repo.root().join(meta::VIEW_FILE), format!("{event}\n")).unwrap();
    meta::write(
        repo.root(),
        &meta::Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            String::new(),
        ),
    )
    .unwrap();
    repo.git(&["add", "."]).unwrap();
    repo.git(&["commit", "-m", "Oversized source admission fixture"])
        .unwrap();
}
