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

/// Claude Code appends a `custom-title` record whenever the session is named or renamed, and an
/// empty one when the name is cleared, so the last one is current. Without a name Claude Code
/// shows the title it generated (`ai-title`, last one current); older transcripts carry only
/// `summary` records. agit placeholders are skipped as if they had never been written.
fn claude_title(transcript: &str) -> Option<String> {
    let mut named = true;
    let mut generated = None;
    let mut summary = None;
    for line in transcript.lines().rev() {
        if !line.contains("\"custom-title\"")
            && !line.contains("\"ai-title\"")
            && !line.contains("\"summary\"")
        {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let text = |key: &str| record[key].as_str().map(str::trim);
        match record["type"].as_str() {
            Some("custom-title") if named => match text("customTitle") {
                Some(title) if agit_placeholder(title) => {}
                Some(title) if !title.is_empty() => return Some(title.to_owned()),
                _ => named = false,
            },
            Some("ai-title") if generated.is_none() => {
                generated = text("aiTitle").map(str::to_owned)
            }
            Some("summary") if summary.is_none() => summary = text("summary").map(str::to_owned),
            _ => {}
        }
    }
    generated
        .into_iter()
        .chain(summary)
        .find(|title| !title.is_empty() && !agit_placeholder(title))
}

/// The label Claude Code's own session list shows for a session without a title: its first
/// prompt on one line, cut to 200 characters, or "No prompt". Meta and compaction records, Claude
/// Code's own commands, interruptions and records that open with markup are not prompts; a custom
/// command shows with its arguments and a shell escape as `! <command>`. Pasted text names the
/// session by its words, without the wrapper tags.
#[cfg(feature = "cli")]
pub(crate) fn claude_untitled_label(
    transcript: &str,
    user_config: Option<&std::path::Path>,
) -> String {
    const LABEL_CHARS: usize = 200;
    let users = transcript
        .lines()
        .filter(|line| line.contains("\"user\""))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| {
            record["type"] == "user"
                && record["isMeta"] != true
                && record["isCompactSummary"] != true
                && record["isSidechain"] != true
        })
        .collect::<Vec<_>>();
    // Commands of the person's own live in the conversation's project, from the directory it
    // started in up to its repository's root as Claude Code loads them, and in their Claude Code
    // configuration.
    let mut roots: Vec<std::path::PathBuf> = users
        .iter()
        .find_map(|record| record["cwd"].as_str())
        .map(|cwd| project_dirs(std::path::Path::new(cwd)))
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(".claude"))
        .collect();
    roots.extend(user_config.map(std::path::Path::to_path_buf));
    let prompt = users
        .iter()
        .find_map(|record| match &record["message"]["content"] {
            serde_json::Value::String(text) => claude_prompt(text, &roots),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .find_map(|text| claude_prompt(text, &roots)),
            _ => None,
        });
    let Some(prompt) = prompt else {
        return "No prompt".to_owned();
    };
    let words = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.chars().count() <= LABEL_CHARS {
        return words;
    }
    let cut: String = words.chars().take(LABEL_CHARS).collect();
    format!("{}\u{2026}", cut.trim_end())
}

/// One text block of a user record as Claude Code's session list reads it, or `None` when the
/// block is not a prompt.
#[cfg(feature = "cli")]
fn claude_prompt(text: &str, roots: &[std::path::PathBuf]) -> Option<String> {
    use std::sync::LazyLock;
    static PASTE_WRAPPER: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"</?pasted_content\b[^>]*(?:>|$)").unwrap());
    static NOT_A_PROMPT: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"^(?:\s*<[a-z][\w-]*[\s>]|\[Request interrupted by user[^\]]*\])")
            .unwrap()
    });
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let end = text[start..].find(&format!("</{name}>"))?;
        Some(text[start..start + end].trim())
    };
    if let Some(command) = tag("command-name") {
        let args = tag("command-args").filter(|args| !args.is_empty())?;
        return custom_command(command.trim_start_matches('/'), roots)
            .then(|| format!("{command} {args}"));
    }
    if let Some(shell) = tag("bash-input") {
        return Some(format!("! {shell}"));
    }
    let text = PASTE_WRAPPER.replace_all(text, " ");
    (!text.trim().is_empty() && !NOT_A_PROMPT.is_match(&text)).then(|| text.into_owned())
}

/// Whether `/name` is a command the person or a plugin defined, which Claude Code's list names a
/// session by: a namespaced name (`plugin:command`), or one that resolves to a command file or a
/// skill in `roots`. Anything else is taken as one of Claude Code's own commands, which its list
/// passes over; a built-in this rule does not know costs only a label.
#[cfg(feature = "cli")]
fn custom_command(name: &str, roots: &[std::path::PathBuf]) -> bool {
    fn command_file(dir: &std::path::Path, file: &str, depth: usize) -> bool {
        dir.join(file).is_file()
            || depth > 0
                && std::fs::read_dir(dir).is_ok_and(|entries| {
                    entries
                        .flatten()
                        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                        .any(|entry| command_file(&entry.path(), file, depth - 1))
                })
    }
    name.contains(':')
        || roots.iter().any(|root| {
            command_file(&root.join("commands"), &format!("{name}.md"), 2)
                || root.join("skills").join(name).join("SKILL.md").is_file()
        })
}

/// The directories whose `.claude` holds a conversation's project commands: the one it started
/// in and each above it up to the repository's root, the directory that holds `.git`. Outside a
/// repository only the starting directory counts.
#[cfg(feature = "cli")]
pub(crate) fn project_dirs(start: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    for dir in start.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() {
            return dirs;
        }
    }
    vec![start.to_path_buf()]
}

/// The Claude Code configuration directory whose `commands` and `skills` are the user's own.
#[cfg(feature = "cli")]
pub(crate) fn claude_user_config() -> Option<std::path::PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| crate::infra::config::user_home().map(|home| home.join(".claude")))
}

/// `agit: unnamed`, or `agit <owner>/<repo>@<branch>`, as agit's SessionStart hook once named
/// Claude Code sessions.
pub(crate) fn agit_placeholder(title: &str) -> bool {
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
        let cleared = [
            record(serde_json::json!({"type":"ai-title","aiTitle":"Generated title"})),
            record(serde_json::json!({"type":"custom-title","customTitle":"Old name"})),
            record(serde_json::json!({"type":"custom-title","customTitle":""})),
        ]
        .join("\n");
        assert_eq!(
            native_title("claude-code", "unused", &cleared, None).as_deref(),
            Some("Generated title")
        );
        assert!(!agit_placeholder("agit roadmap review"));
    }

    /// The label is the one Claude Code's list shows for an untitled session: records and blocks
    /// that are not prompts are passed over, pasted text keeps its words without the wrapper,
    /// a long prompt is cut with an ellipsis, and a session without a prompt says so.
    #[cfg(feature = "cli")]
    #[test]
    fn an_untitled_claude_session_is_labeled_by_its_first_prompt() {
        // The repository's root defines the `/triage` skill, and the conversation starts in one
        // of its subdirectories, as in a monorepo; the user's configuration defines nothing.
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".git")).unwrap();
        let skill = project.path().join(".claude/skills/triage");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "Triage the issue").unwrap();
        let start = project.path().join("packages/frontend");
        std::fs::create_dir_all(&start).unwrap();
        let user_config = tempfile::tempdir().unwrap();
        let cwd = start.to_str().unwrap().to_owned();
        let label = |records: &[serde_json::Value]| {
            let transcript = records
                .iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>();
            claude_untitled_label(&transcript, Some(user_config.path()))
        };
        let user = |content: serde_json::Value| serde_json::json!({"type":"user","cwd":cwd,"message":{"content":content}});
        assert_eq!(
            label(&[
                serde_json::json!({"type":"user","isMeta":true,"message":{"content":"Caveat"}}),
                user(serde_json::json!(
                    "<command-name>/model</command-name><command-args>haiku</command-args>"
                )),
                user(serde_json::json!(
                    "<local-command-stdout>Set model</local-command-stdout>"
                )),
                user(serde_json::json!("[Request interrupted by user]")),
                user(
                    serde_json::json!([{"type":"tool_result","content":"ok"},{"type":"text","text":"<pasted_content id=\"1\">\nFix the\nlease pool</pasted_content> please"}])
                ),
                user(serde_json::json!("Later prompt")),
            ]),
            "Fix the lease pool please"
        );
        // A command that resolves to nothing the person or a plugin defined is one of Claude
        // Code's own, with arguments or not, and is passed over for the question after it; a
        // custom command with arguments names the session.
        assert_eq!(
            label(&[
                user(serde_json::json!(
                    "<command-name>/effort</command-name><command-args>high</command-args>"
                )),
                user(serde_json::json!(
                    "<command-name>/plan</command-name><command-args>the migration</command-args>"
                )),
                user(serde_json::json!(
                    "<command-name>/rc</command-name><command-args>start</command-args>"
                )),
                user(serde_json::json!("Why does the login redirect loop?")),
            ]),
            "Why does the login redirect loop?"
        );
        assert_eq!(
            label(&[user(serde_json::json!(
                "<command-name>/triage</command-name><command-args>#124</command-args>"
            ))]),
            "/triage #124"
        );
        assert_eq!(
            label(&[user(serde_json::json!("<bash-input>ls</bash-input>"))]),
            "! ls"
        );
        let long = "word ".repeat(60);
        assert_eq!(
            label(&[user(serde_json::json!(long))]),
            format!("{}\u{2026}", long[..200].trim_end())
        );
        assert_eq!(label(&[]), "No prompt");
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
