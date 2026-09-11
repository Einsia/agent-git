//! Shared presentation scope for native candidate pickers; choosing a scope grants no authority.

use crate::tui::widgets;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Scope {
    pub runtime: Option<String>,
    pub all_projects: bool,
}

impl Scope {
    pub fn includes(&self, runtime: &str, here: bool) -> bool {
        (self.all_projects || here)
            && self
                .runtime
                .as_deref()
                .is_none_or(|selected| selected == runtime)
    }

    pub fn cycle_runtime(&mut self, runtimes: &[String]) {
        self.runtime = match self
            .runtime
            .as_ref()
            .and_then(|selected| runtimes.iter().position(|runtime| runtime == selected))
        {
            Some(index) => runtimes.get(index + 1).cloned(),
            None => runtimes.first().cloned(),
        };
    }

    pub fn label(&self) -> String {
        format!(
            "{} · {}",
            self.runtime.as_deref().unwrap_or("all runtimes"),
            if self.all_projects {
                "all projects"
            } else {
                "current project"
            }
        )
    }
}

pub(super) fn runtimes<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut values: Vec<_> = values
        .filter(|value| crate::adapter::RUNTIMES.contains(value))
        .map(str::to_owned)
        .collect();
    values.sort();
    values.dedup();
    values
}

/// A single available runtime needs no extra choice; the candidate still requires confirmation.
pub(super) fn preselect(runtimes: &[String], title: &str) -> crate::Result<Option<Scope>> {
    if runtimes.len() <= 1 {
        return Ok(Some(Scope::default()));
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;
    let mut state = ListState::default();
    state.select(Some(0));
    loop {
        terminal.draw(|frame| {
            let rows = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(frame.area());
            frame.render_widget(
                ratatui::widgets::Paragraph::new(format!("{title} · choose a runtime scope")),
                rows[0],
            );
            let items: Vec<_> = std::iter::once("all runtimes")
                .chain(runtimes.iter().map(String::as_str))
                .map(ListItem::new)
                .collect();
            frame.render_stateful_widget(
                List::new(items)
                    .block(widgets::pane("runtime"))
                    .highlight_symbol("› ")
                    .highlight_style(crate::ui::theme::selected()),
                rows[1],
                &mut state,
            );
            widgets::render_footer(
                frame,
                rows[2],
                "tab/↑↓ runtime   enter candidates   esc cancel",
            );
        })?;
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
            KeyCode::Tab | KeyCode::Down => state.select(Some(
                (state.selected().unwrap_or(0) + 1) % (runtimes.len() + 1),
            )),
            KeyCode::BackTab | KeyCode::Up => state.select(Some(
                (state.selected().unwrap_or(0) + runtimes.len()) % (runtimes.len() + 1),
            )),
            KeyCode::Enter => {
                return Ok(Some(Scope {
                    runtime: state
                        .selected()
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| runtimes.get(index))
                        .cloned(),
                    all_projects: false,
                }));
            }
            _ => {}
        }
    }
}

pub(super) fn project_label(cwd: Option<&str>) -> String {
    cwd.unwrap_or("unknown")
        .chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

const PREVIEW_BYTES: u64 = 32 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct Preview {
    pub gist: Option<String>,
    pub cwd: Option<String>,
}

/// Open only a regular native source; a special file must not block an advisory probe.
pub(super) fn opening_file(path: &std::path::Path) -> Option<std::fs::File> {
    if !std::fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return None;
        }
    }
    Some(file)
}

/// Read an advisory opening window only; neither a preview nor its directory selects a source.
pub(crate) fn preview(runtime: &str, path: &std::path::Path) -> Preview {
    use std::io::Read;
    let read = || -> Option<String> {
        let file = opening_file(path)?;
        let mut bytes = Vec::new();
        file.take(PREVIEW_BYTES).read_to_end(&mut bytes).ok()?;
        // A cut record never provides even advisory fields.
        let end = bytes.iter().rposition(|byte| *byte == b'\n')? + 1;
        String::from_utf8(bytes[..end].to_vec()).ok()
    };
    if !matches!(runtime, "claude-code" | "codex" | "cursor") {
        return Preview::default();
    }
    let Some(text) = read() else {
        return Preview::default();
    };
    let mut result = Preview::default();
    if runtime == "cursor" {
        let session = crate::adapter::cursor::parse_preview_at(path, &text);
        result.gist = session.gist(60);
        result.cwd = session.cwd;
        return result;
    }
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if result.cwd.is_none() {
            result.cwd = value
                .get("cwd")
                .or_else(|| value.get("payload")?.get("cwd"))
                .and_then(serde_json::Value::as_str)
                .filter(|cwd| std::path::Path::new(cwd).is_absolute())
                .map(str::to_owned);
        }
    }
    if let Ok(adapter) = crate::adapter::get(runtime)
        && let Ok(session) = adapter.parse(&text)
    {
        result.gist = session.gist(60);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_preview_is_bounded_and_cannot_borrow_fields_from_a_cut_record() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let cwd = directory.path().to_str().unwrap();
        let first = format!(
            "{}\n",
            serde_json::json!({"type":"user", "cwd":cwd,
            "message":{"role":"user", "content":"recognizable opening prompt"}})
        );
        let bytes = format!(
            "{first}{}\n{{\"cwd\":\"/unread-tail\"}}\n",
            "x".repeat(PREVIEW_BYTES as usize)
        );
        std::fs::write(&path, &bytes).unwrap();
        let observed = preview("claude-code", &path);
        assert_eq!(
            observed.gist.as_deref(),
            Some("recognizable opening prompt")
        );
        assert_eq!(observed.cwd.as_deref(), Some(cwd));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        std::fs::write(&path, first.trim_end()).unwrap();
        let cut = preview("claude-code", &path);
        assert!(cut.gist.is_none());
        assert!(cut.cwd.is_none());
        assert!(preview("claude-code", directory.path()).gist.is_none());
        assert!(preview("opencode", &path).gist.is_none());
    }

    #[test]
    fn selected_cursor_preview_uses_native_body_path_evidence_without_slug_decoding() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("project-with-hyphens");
        let path = directory
            .path()
            .join(crate::adapter::cursor::slug_for(&cwd))
            .join("agent-transcripts/session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = format!(
            "{}\n{}\n",
            serde_json::json!({"role":"user", "message":{"content":[{"type":"text", "text":"<user_query>recognize cursor</user_query>"}]}}),
            serde_json::json!({"role":"assistant", "message":{"content":[{"type":"tool_use", "name":"Read", "input":{"path":cwd.join("a.rs")}}]}})
        );
        std::fs::write(&path, &bytes).unwrap();
        let observed = preview("cursor", &path);
        assert_eq!(observed.cwd.as_deref(), cwd.to_str());
        assert_eq!(observed.gist.as_deref(), Some("recognize cursor"));
        let no_hint = bytes.lines().next().unwrap().to_owned() + "\n";
        std::fs::write(&path, &no_hint).unwrap();
        assert!(preview("cursor", &path).cwd.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), no_hint);
    }

    #[cfg(unix)]
    #[test]
    fn preview_rejects_special_files_and_symlink_routes() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("native.jsonl");
        std::fs::write(&file, "{}\n").unwrap();
        let alias = directory.path().join("alias.jsonl");
        symlink(&file, &alias).unwrap();
        assert!(preview("claude-code", &alias).gist.is_none());
        let fifo = directory.path().join("fifo.jsonl");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(preview("claude-code", &fifo).gist.is_none());
        assert!(std::fs::symlink_metadata(alias).unwrap().is_symlink());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "{}\n");
    }

    #[test]
    fn project_and_runtime_filters_intersect_and_cycle_without_selecting_a_session() {
        let runtimes = runtimes(["codex", "claude-code", "codex", ""].into_iter());
        assert_eq!(runtimes, ["claude-code", "codex"]);
        let mut scope = Scope::default();
        assert!(scope.includes("codex", true));
        assert!(!scope.includes("codex", false));
        scope.cycle_runtime(&runtimes);
        assert!(scope.includes("claude-code", true));
        assert!(!scope.includes("codex", true));
        scope.all_projects = true;
        assert!(scope.includes("claude-code", false));
        assert!(!scope.includes("codex", false));
        scope.cycle_runtime(&runtimes);
        assert!(scope.includes("codex", false));
        scope.cycle_runtime(&runtimes);
        assert!(scope.includes("claude-code", false));
        assert!(scope.includes("codex", false));
        scope.all_projects = false;
        assert!(!scope.includes("codex", false));
    }
}
