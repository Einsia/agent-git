//! Select a settled session for read-only history and sharing workflows.

use crate::domain::{repo::Repo, session};
use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug)]
pub struct Row {
    pub slug: String,
    pub branch: String,
    pub path: PathBuf,
    runtime: String,
    modified: SystemTime,
}

impl Row {
    pub fn target(&self) -> String {
        format!("{}@{}", self.slug, self.branch)
    }
}

/// Lists read metadata in batches per repository; transcripts are opened only after selection.
fn collect(cwd: &Path) -> crate::Result<Vec<Row>> {
    let preferred = crate::commands::context::repo_for(cwd)
        .ok()
        .or_else(|| crate::domain::workspace::read(cwd).map(|workspace| workspace.repo))
        .map(|slug| crate::commands::context::qualify(&slug));
    let mut rows = Vec::new();
    for (owner, name, path) in crate::commands::clone::list_local()? {
        let Some(repo) = Repo::open(&path) else {
            continue;
        };
        let slug = format!("{owner}/{name}");
        for stored in session::list(&repo) {
            let Some(branch) = stored.branch else {
                continue;
            };
            rows.push(Row {
                slug: slug.clone(),
                branch,
                path: path.clone(),
                runtime: stored.runtime,
                modified: stored.mtime,
            });
        }
    }
    rank(&mut rows, preferred.as_deref());
    Ok(rows)
}

fn rank(rows: &mut [Row], preferred: Option<&str>) {
    rows.sort_by(|a, b| {
        (Some(b.slug.as_str()) == preferred)
            .cmp(&(Some(a.slug.as_str()) == preferred))
            .then_with(|| b.modified.cmp(&a.modified))
            .then_with(|| a.slug.cmp(&b.slug))
            .then_with(|| a.branch.cmp(&b.branch))
    });
}

pub fn pick(cwd: &Path, title: &str) -> crate::Result<Option<Row>> {
    let rows = collect(cwd)?;
    if rows.is_empty() {
        println!("no settled sessions on this machine yet.");
        crate::ui::hint(
            "import a conversation with `agit import`, or fetch a repo with `agit clone`",
        );
        return Ok(None);
    }
    widgets::refresh_rc_status();
    let mut guard = crate::tui::term::Guard::enter()?;
    let result = run_loop(&rows, title);
    guard.suspend()?;
    result
}

fn run_loop(rows: &[Row], title: &str) -> crate::Result<Option<Row>> {
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default();
    state.select(Some(0));
    let mut filter = Filter::default();
    loop {
        let view: Vec<&Row> = rows
            .iter()
            .filter(|row| filter.matches(&format!("{} {}", row.target(), row.runtime)))
            .collect();
        state.select(if view.is_empty() {
            None
        } else {
            Some(state.selected().unwrap_or(0).min(view.len() - 1))
        });
        terminal.draw(|frame| draw(frame, &view, &mut state, &filter, title))?;
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(None);
        }
        if filter.is_active() {
            match key.code {
                KeyCode::Esc => filter.close(),
                KeyCode::Enter => filter.blur(),
                KeyCode::Backspace => filter.pop(),
                KeyCode::Char(ch) => filter.push(ch),
                _ => {}
            }
            state.select((!view.is_empty()).then_some(0));
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Char('/') => filter.open(),
            KeyCode::Down | KeyCode::Char('j') => state.select(Some(
                (state.selected().unwrap_or(0) + 1).min(view.len().saturating_sub(1)),
            )),
            KeyCode::Up | KeyCode::Char('k') => {
                state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)))
            }
            KeyCode::Home | KeyCode::Char('g') => state.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => state.select(Some(view.len().saturating_sub(1))),
            KeyCode::Enter => {
                if let Some(row) = state.selected().and_then(|index| view.get(index)) {
                    return Ok(Some((*row).clone()));
                }
            }
            _ => {}
        }
    }
}

fn draw(frame: &mut Frame, rows: &[&Row], state: &mut ListState, filter: &Filter, title: &str) {
    let panes = widgets::layout(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: title.into(),
            identity: crate::infra::credentials::current_user(),
            ..Default::default()
        },
    );
    let hint = filter.hint();
    let list_area = widgets::list_area_with_notice(frame, panes, hint.as_deref());
    let width = list_area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| {
            ListItem::new(vec![
                Line::raw(widgets::truncate_cols(&row.target(), width)),
                Line::styled(
                    widgets::truncate_cols(
                        &format!("  {} · {}", row.runtime, crate::ui::ago(row.modified)),
                        width,
                    ),
                    theme::muted(),
                ),
            ])
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(widgets::pane("choose a session"))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        list_area,
        state,
    );
    if let Some(detail) = panes.detail {
        let text = state
            .selected()
            .and_then(|index| rows.get(index))
            .map(|row| {
                format!(
                    "{}\n\nRuntime: {}\nLast saved: {}\n\nReads the saved conversation on this branch.\n\nSelection applies to this command only.",
                    row.target(), row.runtime, crate::ui::ago(row.modified)
                )
            })
            .unwrap_or_else(|| "No sessions match this filter.".into());
        frame.render_widget(
            Paragraph::new(text)
                .block(widgets::pane("session"))
                .wrap(Wrap { trim: false }),
            detail,
        );
    }
    widgets::render_footer(
        frame,
        panes.footer,
        if filter.is_active() {
            "type to filter   enter apply   esc clear"
        } else {
            "↑↓ move   enter choose   / filter   q cancel"
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevant_repo_ranks_first_without_merging_other_namespaces() {
        let row = |slug: &str, seconds| Row {
            slug: slug.into(),
            branch: "work".into(),
            path: PathBuf::new(),
            runtime: "codex".into(),
            modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
        };
        let mut rows = vec![row("org/repo", 10), row("me/repo", 1), row("me/repo", 5)];
        rank(&mut rows, Some("me/repo"));
        assert_eq!(
            rows[0].modified,
            rows[1].modified + std::time::Duration::from_secs(4)
        );
        assert_eq!(rows[0].target(), "me/repo@work");
        assert_eq!(rows[2].target(), "org/repo@work");
    }
}
