//! `agit repo delete` confirms by typing the full name, and a non-interactive environment has no
//! input to give. This pins that with no terminal the command names the terminal it is missing,
//! rather than sharing one `cancelled.` with "the name was typed wrong" — a caller inside a
//! script or an editor has only that sentence to know it must move to a terminal.

use std::process::Command;

#[test]
fn deleting_without_a_terminal_names_the_missing_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(["repo", "delete", "drh/qa"])
        .env("AGIT_HOME", tmp.path().join("home"))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(agit::ExitCode::Interactive.as_i32()),
        "a missing terminal must use the interactive-required exit code"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refused without a TTY"),
        "stderr must name the missing terminal: {stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("cancelled."),
        "a missing terminal is not a cancellation"
    );
}
