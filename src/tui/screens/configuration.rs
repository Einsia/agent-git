//! The zero-argument `agit config` editor.
//!
//! Each row keeps the effective value separate from the value persisted in `config.json`.
//! Environment overrides therefore remain visible while a stored value is edited or removed.

use crate::commands::config::{self, Entry, Source};
use crate::tui::widgets;
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

/// Edit global configuration until the user quits.
pub fn edit() -> crate::Result<()> {
    let mut entries = config::collect()?;
    let mut state = ListState::default();
    state.select((!entries.is_empty()).then_some(0));
    let mut editing = false;
    let mut buffer = String::new();
    let mut notice: Option<String> = None;

    widgets::refresh_rc_status();
    let mut guard = crate::tui::term::Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    loop {
        terminal.draw(|frame| {
            draw(
                frame,
                &entries,
                &mut state,
                editing,
                &buffer,
                notice.as_deref(),
            )
        })?;
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        notice = None;

        if editing {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Esc => editing = false,
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Enter => {
                    let Some(index) = state.selected() else {
                        continue;
                    };
                    let value = buffer.trim();
                    if value.is_empty() {
                        notice =
                            Some("a stored value cannot be empty; press u to unset it.".into());
                        continue;
                    }
                    match config::apply(entries[index].key, Some(value)) {
                        Ok(()) => {
                            let key = entries[index].key;
                            entries = config::collect()?;
                            editing = false;
                            notice = Some(format!("saved {key}"));
                        }
                        Err(error) => notice = Some(format!("{error:#}")),
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    buffer.push(ch);
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Down | KeyCode::Char('j') => move_selection(&mut state, entries.len(), 1),
            KeyCode::Up | KeyCode::Char('k') => move_selection(&mut state, entries.len(), -1),
            KeyCode::Enter | KeyCode::Char('e') => {
                let Some(index) = state.selected() else {
                    continue;
                };
                buffer = editable_value(&entries[index]);
                editing = true;
            }
            KeyCode::Char('u') => {
                let Some(index) = state.selected() else {
                    continue;
                };
                let key = entries[index].key;
                match config::apply(key, None) {
                    Ok(()) => {
                        entries = config::collect()?;
                        notice = Some(format!("unset {key}"));
                    }
                    Err(error) => notice = Some(format!("{error:#}")),
                }
            }
            _ => {}
        }
    }
    drop(terminal);
    guard.suspend()?;
    Ok(())
}

fn editable_value(entry: &Entry) -> String {
    if entry.source == Source::Environment {
        entry.stored.clone().unwrap_or_default()
    } else {
        entry
            .stored
            .clone()
            .or_else(|| entry.effective.clone())
            .unwrap_or_default()
    }
}

fn move_selection(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        state.select(None);
        return;
    }
    let current = state.selected().unwrap_or(0);
    let next = if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(len - 1)
    };
    state.select(Some(next));
}

fn source_label(source: Source) -> &'static str {
    match source {
        Source::Environment => "env",
        Source::Stored => "stored",
        Source::Default => "default",
        Source::Unset => "unset",
    }
}

/// Keep the draft and its cursor visible while the row gives the value at least half its width.
/// The source is already present in the detail pane; retaining it in an editing row can consume
/// every remaining column before the draft begins.
fn editing_row(key: &str, buffer: &str, width: usize) -> String {
    let value_floor = width / 2;
    let key_budget = width.saturating_sub(value_floor).saturating_sub(3);
    let key = widgets::truncate_cols(key, key_budget);
    let prefix = if key.is_empty() {
        String::new()
    } else {
        format!("{key} = ")
    };
    let value_budget = width.saturating_sub(widgets::cols(&prefix));
    format!("{prefix}{}", draft_tail(buffer, value_budget))
}

/// Show the end of an append-only draft because that is where the editing cursor lives.
fn draft_tail(buffer: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if widgets::cols(buffer).saturating_add(1) <= width {
        return format!("{buffer}_");
    }
    if width == 1 {
        return "_".into();
    }

    let content_budget = width - 2; // leading ellipsis and trailing cursor
    let mut start = buffer.len();
    let mut used = 0;
    for (index, ch) in buffer.char_indices().rev() {
        let char_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + char_width > content_budget {
            break;
        }
        start = index;
        used += char_width;
    }
    format!("…{}_", &buffer[start..])
}

fn draw(
    frame: &mut Frame,
    entries: &[Entry],
    state: &mut ListState,
    editing: bool,
    buffer: &str,
    notice: Option<&str>,
) {
    let panes = widgets::layout(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: "agit config".into(),
            identity: crate::infra::credentials::current_user()
                .map(|user| format!("{user} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: Default::default(),
        },
    );
    let list_area = widgets::list_area_with_notice(frame, panes, notice);
    let selected = state.selected();
    let row_width = list_area.width.saturating_sub(4) as usize;
    let rows: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            if editing && selected == Some(index) {
                ListItem::new(editing_row(entry.key, buffer, row_width))
            } else {
                ListItem::new(widgets::clamp_line(
                    Line::from(format!(
                        "{:<19} [{:<7}] {}",
                        entry.key,
                        source_label(entry.source),
                        entry.effective.as_deref().unwrap_or("(unset)")
                    )),
                    row_width,
                ))
            }
        })
        .collect();
    frame.render_stateful_widget(
        List::new(rows)
            .block(widgets::pane("global configuration"))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        list_area,
        state,
    );

    if let Some(area) = panes.detail
        && let Some(entry) = selected.and_then(|index| entries.get(index))
    {
        let mut detail = String::new();
        if let Some(notice) = notice {
            detail.push_str(notice);
            detail.push_str("\n\n");
        }
        detail.push_str(&format!("key        {}\n", entry.key));
        if editing {
            detail.push_str(&format!("editing    {buffer}_\n"));
        }
        detail.push_str(&format!(
            "effective  {} ({})\n",
            entry.effective.as_deref().unwrap_or("(unset)"),
            source_label(entry.source)
        ));
        detail.push_str(&format!(
            "stored     {}\n",
            entry.stored.as_deref().unwrap_or("(unset)")
        ));
        if let Some(environment_name) = entry.environment_name {
            detail.push_str(&format!(
                "environment ${environment_name} = {}\n",
                entry.environment.as_deref().unwrap_or("not set")
            ));
        }
        detail.push('\n');
        detail.push_str(entry.description);
        if entry.source == Source::Environment {
            let environment_name = entry.environment_name.unwrap_or("environment");
            detail.push_str(&format!(
                "\n\n${environment_name} overrides the stored value. Editing or unsetting the file does not change this process."
            ));
        }
        frame.render_widget(
            Paragraph::new(detail)
                .block(widgets::pane("value and source"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
    widgets::render_footer(
        frame,
        panes.footer,
        if editing {
            "type value   enter save   esc cancel"
        } else {
            "↑↓ key   enter edit   u unset   q quit"
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn entry(source: Source) -> Entry {
        Entry {
            key: "hub.url",
            description: "default hub address (AGIT_HUB_URL takes priority)",
            effective: Some("https://env.example".into()),
            stored: Some("https://stored.example".into()),
            source,
            environment_name: Some("AGIT_HUB_URL"),
            environment: Some("https://env.example".into()),
        }
    }

    #[test]
    fn an_environment_override_does_not_become_the_editable_stored_value() {
        assert_eq!(
            editable_value(&entry(Source::Environment)),
            "https://stored.example"
        );
        let mut row = entry(Source::Environment);
        row.stored = None;
        assert_eq!(editable_value(&row), "");
    }

    #[test]
    fn a_narrow_editing_row_keeps_the_draft_tail_and_cursor_visible() {
        let row = editing_row(
            "runtime.default",
            "0123456789abcdefghijklmnopqrstuvwxyz",
            26,
        );
        assert!(widgets::cols(&row) <= 26, "row exceeds its pane: {row}");
        assert!(row.ends_with("xyz_"), "editing tail is hidden: {row}");
        assert!(
            row.contains('…'),
            "a clipped field needs an affordance: {row}"
        );

        assert_eq!(draft_tail("甲乙丙丁", 6), "…丙丁_");
        assert_eq!(draft_tail("anything", 1), "_");
    }

    #[test]
    fn the_frame_separates_effective_stored_and_environment_values() {
        let entries = vec![entry(Source::Environment)];
        let mut state = ListState::default();
        state.select(Some(0));
        let mut terminal = Terminal::new(TestBackend::new(120, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, &entries, &mut state, false, "", None))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "agit config",
            "https://env.example",
            "https://stored.example",
            "AGIT_HUB_URL overrides",
        ] {
            assert!(
                text.contains(expected),
                "missing `{expected}` from frame: {text}"
            );
        }
    }

    #[test]
    fn a_single_pane_frame_keeps_operation_errors_visible() {
        let entries = vec![entry(Source::Stored)];
        let mut state = ListState::default();
        state.select(Some(0));
        let mut terminal = Terminal::new(TestBackend::new(79, 10)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &entries,
                    &mut state,
                    false,
                    "",
                    Some("cannot save configuration"),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("cannot save configuration"),
            "single-pane feedback disappeared: {text}"
        );
    }

    #[test]
    fn the_detail_pane_shows_the_complete_editing_draft() {
        let entries = vec![entry(Source::Stored)];
        let mut state = ListState::default();
        state.select(Some(0));
        let draft = "https://a-long-value.example/configuration";
        let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
        terminal
            .draw(|frame| draw(frame, &entries, &mut state, true, draft, None))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("…"),
            "the narrow editing row was not windowed: {text}"
        );
        assert!(
            text.contains("https://a-long-value.example/configuration_"),
            "the detail pane hid the complete draft: {text}"
        );
    }
}
