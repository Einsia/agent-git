//! Terminal notices related to the agent session.

use crate::infra::runtime_session::Current;

/// The human-facing name of a runtime.
pub fn runtime_label(runtime: &str) -> &'static str {
    match runtime {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        "cursor" => "Cursor",
        _ => "agent",
    }
}

/// Print the guard notice for `agit new`; the caller then refuses to continue.
pub fn warn_new(current: &Current, target: &str, branch: &str) {
    let turns = current
        .completed_turns
        .map(|n| format!("with {n} completed turns"))
        .unwrap_or_else(|| "with an active transcript".to_string());
    let destination = shell_arg(&format!("{target}@{branch}"));
    let target_arg = shell_arg(target);
    let branch_arg = shell_arg(branch);
    crate::ui::warning(&format!(
        "You are already inside an unmanaged {} session {}.",
        runtime_label(current.runtime),
        turns
    ));
    eprintln!("\n`agit new` starts an empty session and will not include this conversation.\n");
    eprintln!("To adopt the current conversation:");
    eprintln!("  agit import @ --into {destination}\n");
    eprintln!("To intentionally start fresh:");
    eprintln!("  agit new {target_arg} -b {branch_arg} --fresh");
}

/// Turn user input in a notice into one argument that is safe to copy into a shell.
pub fn shell_arg(value: &str) -> String {
    let placeholder = value.starts_with('<') && value.ends_with('>');
    if placeholder
        || value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '@' | '-' | '_' | '.'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtimes_have_stable_human_labels() {
        assert_eq!(runtime_label("codex"), "Codex");
        assert_eq!(runtime_label("claude-code"), "Claude Code");
    }

    #[test]
    fn warning_arguments_are_shell_safe_without_making_examples_noisy() {
        assert_eq!(shell_arg("me/paper@table-3"), "me/paper@table-3");
        assert_eq!(shell_arg("feature;drop"), "'feature;drop'");
        assert_eq!(shell_arg("<branch>"), "<branch>");
    }
}
