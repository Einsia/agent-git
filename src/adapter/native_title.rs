//! A runtime's own name for a session travels with the session's metadata.

/// Longer names are cut here; a title is a label, not a summary of the conversation.
const TITLE_CHARS: usize = 200;

/// The runtime's current name for a session, or `None` when it keeps none. A placeholder that
/// agit's own SessionStart hook assigned is not the session's name.
///
/// `home` is the runtime home the session is read from; `None` means this process's default.
/// Two homes can hold the same Codex thread id, so a name read from any other home than the
/// session's own can belong to a different conversation.
pub fn native_title(
    runtime: &str,
    session_id: &str,
    transcript: &str,
    home: Option<&std::path::Path>,
) -> Option<String> {
    let title = match runtime {
        "claude-code" => claude_title(transcript),
        "codex" => {
            let home = match home {
                Some(home) => home.to_path_buf(),
                None => super::codex::codex_home().ok()?,
            };
            super::codex_titles::name(&home.join("session_index.jsonl"), session_id)
        }
        _ => None,
    }?;
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title: String = title.chars().take(TITLE_CHARS).collect();
    (!title.is_empty() && !agit_placeholder(&title)).then_some(title)
}

/// Claude Code appends a `custom-title` record whenever the session is named or renamed, so the
/// last one is current. Older transcripts carry only `summary` records, read when no name exists.
fn claude_title(transcript: &str) -> Option<String> {
    let mut summary = None;
    for line in transcript.lines().rev() {
        if !line.contains("\"custom-title\"") && !line.contains("\"summary\"") {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let text = |key: &str| {
            record[key]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty() && !agit_placeholder(title))
                .map(str::to_owned)
        };
        match record["type"].as_str() {
            Some("custom-title") => {
                if let Some(title) = text("customTitle") {
                    return Some(title);
                }
            }
            Some("summary") if summary.is_none() => summary = text("summary"),
            _ => {}
        }
    }
    summary
}

/// `agit: unnamed`, or `agit <owner>/<repo>@<branch>`, as the SessionStart hook writes them.
fn agit_placeholder(title: &str) -> bool {
    title == "agit: unnamed"
        || title.strip_prefix("agit ").is_some_and(|binding| {
            !binding.contains(char::is_whitespace)
                && binding
                    .split_once('/')
                    .is_some_and(|(_, rest)| rest.contains('@'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last real name wins over earlier names, agit placeholders and summaries. Taking the
    /// first record would freeze the title at its first name, and accepting placeholders would
    /// publish `agit: unnamed` as the session's title.
    #[test]
    fn claude_title_takes_the_latest_real_name() {
        let record = |value: serde_json::Value| value.to_string();
        let transcript = [
            record(serde_json::json!({"type":"summary","summary":"Older summary"})),
            record(serde_json::json!({"type":"custom-title","customTitle":"First name"})),
            record(serde_json::json!({"type":"user","message":{"content":"\"custom-title\""}})),
            record(serde_json::json!({"type":"custom-title","customTitle":"Renamed  session\n"})),
            record(serde_json::json!({"type":"custom-title","customTitle":"agit: unnamed"})),
            record(serde_json::json!({"type":"custom-title","customTitle":"agit alice/app@work"})),
        ]
        .join("\n");
        assert_eq!(
            native_title("claude-code", "unused", &transcript, None).as_deref(),
            Some("Renamed session")
        );
        let summaries = record(serde_json::json!({"type":"summary","summary":"Only summary"}));
        assert_eq!(
            native_title("claude-code", "unused", &summaries, None).as_deref(),
            Some("Only summary")
        );
        let placeholder =
            record(serde_json::json!({"type":"custom-title","customTitle":"agit: unnamed"}));
        assert_eq!(
            native_title("claude-code", "unused", &placeholder, None),
            None
        );
        assert!(!agit_placeholder("agit roadmap review"));
    }

    /// Two homes can hold the same thread id under different names; the name comes from the
    /// home the session is read from. Reading the process default instead would record the
    /// other conversation's name, or none when that home lacks the thread.
    #[test]
    fn codex_title_comes_from_the_sessions_own_home() {
        let homes = ["Bound source name", "Default home name"].map(|name| {
            let home = tempfile::tempdir().unwrap();
            let record = serde_json::json!({"id":"thread-a", "thread_name": name});
            std::fs::write(
                home.path().join("session_index.jsonl"),
                format!("{record}\n"),
            )
            .unwrap();
            home
        });
        for (home, name) in homes.iter().zip(["Bound source name", "Default home name"]) {
            assert_eq!(
                native_title("codex", "thread-a", "", Some(home.path())).as_deref(),
                Some(name)
            );
        }
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            native_title("codex", "thread-a", "", Some(empty.path())),
            None
        );
    }
}
