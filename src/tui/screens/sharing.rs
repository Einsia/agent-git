//! Review share visibility and expiry before returning to the command's confirmation.

use crate::tui::widgets;
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

const EXPIRIES: &[&str] = &["7d", "24h", "30d", "never"];
const VIEW_LIMITS: &[Option<u32>] = &[None, Some(1), Some(10), Some(100)];

#[derive(Default)]
struct Draft {
    public: bool,
    expiry: usize,
    views: usize,
    password: bool,
}

impl Draft {
    fn change(&mut self, row: usize, backwards: bool) {
        let cycle = |value: &mut usize, count| {
            *value = (*value + if backwards { count - 1 } else { 1 }) % count;
        };
        match row {
            0 => self.public = !self.public,
            1 => cycle(&mut self.expiry, EXPIRIES.len()),
            2 => cycle(&mut self.views, VIEW_LIMITS.len()),
            3 => self.password = !self.password,
            _ => {}
        }
    }

    fn args(self, target: String) -> crate::commands::share::Args {
        crate::commands::share::Args {
            target: Some(target),
            full_log: false,
            public: self.public,
            expire: EXPIRIES[self.expiry].into(),
            views: VIEW_LIMITS[self.views],
            password: self.password,
            cmd: None,
        }
    }

    fn rows(&self) -> Vec<ListItem<'static>> {
        let visibility = if self.public {
            "Public"
        } else {
            "Encrypted link"
        };
        let views = VIEW_LIMITS[self.views]
            .map(|n| n.to_string())
            .unwrap_or_else(|| "Unlimited".into());
        let password = if self.password { "Required" } else { "None" };
        [
            format!("Visibility   {visibility}"),
            format!("Expires      {}", EXPIRIES[self.expiry]),
            format!("Max views    {views}"),
            format!("Passphrase   {password}"),
            "Continue to confirmation".into(),
        ]
        .into_iter()
        .map(ListItem::new)
        .collect()
    }
}

pub fn pick(cwd: &std::path::Path) -> crate::Result<Option<crate::commands::share::Args>> {
    let Some(session) = super::history::pick(cwd, "agit share")? else {
        return Ok(None);
    };
    let target = session.target();
    let mut draft = Draft::default();
    let mut state = ListState::default();
    state.select(Some(0));
    let mut guard = crate::tui::term::Guard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let result = loop {
        terminal.draw(|frame| draw(frame, &target, &draft, &mut state))?;
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            break None;
        }
        let selected = state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => break None,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                state.select(Some((selected + 1) % 5))
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                state.select(Some((selected + 4) % 5))
            }
            KeyCode::Enter if selected == 4 => break Some(draft.args(target)),
            KeyCode::Left => draft.change(selected, true),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => draft.change(selected, false),
            _ => {}
        }
    };
    drop(terminal);
    guard.suspend()?;
    Ok(result)
}

fn draw(frame: &mut Frame, target: &str, draft: &Draft, state: &mut ListState) {
    let panes = widgets::layout(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: "agit share".into(),
            identity: crate::infra::credentials::current_user(),
            ..Default::default()
        },
    );
    let area = widgets::list_area_with_notice(frame, panes, Some(target));
    frame.render_stateful_widget(
        List::new(draft.rows())
            .block(widgets::pane("share settings"))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        area,
        state,
    );
    if let Some(detail) = panes.detail {
        let visibility = if draft.public {
            "Public: anyone with the link can read it. The service stores the readable conversation."
        } else {
            "Encrypted link: anyone with the complete link can read it. The service cannot decrypt the conversation."
        };
        frame.render_widget(Paragraph::new(format!(
            "{target}\n\n{visibility}\n\nThe link expires after {}.\n\nA passphrase, if enabled, is entered after this screen.\n\nContinue to review and confirm the share.", EXPIRIES[draft.expiry]
        )).block(widgets::pane("review")).wrap(Wrap { trim: false }), detail);
    }
    widgets::render_footer(
        frame,
        panes.footer,
        "↑↓ move   ←→ change   enter select   q cancel",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_settings_keep_explicit_choices_and_private_defaults() {
        let mut draft = Draft::default();
        let defaults = Draft::default().args("me/repo@work".into());
        assert!(!defaults.public);
        assert_eq!(defaults.expire, "7d");
        draft.change(0, false);
        draft.change(1, true);
        draft.change(2, false);
        draft.change(3, false);
        let args = draft.args("org/repo@topic/work".into());
        assert_eq!(args.target.as_deref(), Some("org/repo@topic/work"));
        assert!(args.public);
        assert_eq!(args.expire, "never");
        assert_eq!(args.views, Some(1));
        assert!(args.password);
    }
}
