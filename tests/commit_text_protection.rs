//! Text saved into a commit leaves the privacy worker with every credential replaced.

use agit::domain::privacy::{projector::Mode, service::protect_for_commit};

/// A project-scoped model key, `sk-proj-` followed by mixed-case letters and digits, is replaced
/// by one placeholder in a user message as Codex records it, and the words around it stay.
#[test]
fn a_project_model_key_in_a_user_message_is_replaced() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this binary holds one test, so no other thread reads the environment; the privacy
    // worker it starts inherits these variables.
    unsafe {
        std::env::set_var("AGIT_HOME", home.path());
        std::env::set_var("AGIT_HUB_URL", "http://127.0.0.1:1");
        // A test binary can't host the worker; the agit executable does.
        std::env::set_var("AGIT_TEST_PRIVACY_WORKER", env!("CARGO_BIN_EXE_agit"));
    }
    let key = "sk-proj-pi6NTkE8N9fp0FtVo7zFfX80YifmpaUjErJZsZXDVTdfpkbc";
    let before = "(run-1) Here's the staging LLM key in case you need it: ";
    let after = ". Don't write it anywhere; just tell me which environment variable the chat \
                 service should read a real provider key from.";
    let message = format!("{before}{key}{after}");

    let protected = protect_for_commit(None, &message, Mode::ProtectText).unwrap();

    assert!(!protected.contains("pi6NTkE8N9fp"), "{protected}");
    let inner = protected
        .strip_prefix(before)
        .and_then(|rest| rest.strip_suffix(after))
        .unwrap_or_else(|| panic!("{protected}"));
    let tokens: Vec<_> = agit::domain::privacy::projector::tokens(inner).collect();
    assert_eq!(tokens.len(), 1, "{protected}");
    assert_eq!((tokens[0].0, tokens[0].1), (0, inner.len()), "{protected}");
}
