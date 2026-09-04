//! The zero-argument `agit init` wizard.
//!
//! A directory name is a suggestion only. The user must type a repository name, then decides
//! whether to bind this directory and whether to inspect adoptable project assets. Asset choices
//! start empty and are confirmed individually because instruction and skill files may contain
//! private memory.
//!
//! The wizard returns ordinary [`crate::commands::init::Args`] material. It leaves the alternate
//! screen before repo creation, binding, copying, and command output begin.

use crate::tui::widgets;
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::path::{Path, PathBuf};

/// The explicit answers passed back to the init command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub name: String,
    pub bind: bool,
    /// `None` means seed was not requested. `Some` is the exact set confirmed in the asset screen.
    pub seed_assets: Option<Vec<(PathBuf, PathBuf)>>,
}

#[derive(Debug, Clone)]
struct Form {
    name: String,
    bind: bool,
    seed: bool,
    field: Field,
    editing: bool,
    notice: Option<String>,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            name: String::new(),
            bind: true,
            seed: false,
            field: Field::Name,
            editing: false,
            notice: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Bind,
    Seed,
    Create,
}

impl Field {
    fn next(self) -> Field {
        match self {
            Field::Name => Field::Bind,
            Field::Bind => Field::Seed,
            Field::Seed => Field::Create,
            Field::Create => Field::Name,
        }
    }

    fn previous(self) -> Field {
        match self {
            Field::Name => Field::Create,
            Field::Bind => Field::Name,
            Field::Seed => Field::Bind,
            Field::Create => Field::Seed,
        }
    }

    fn index(self) -> usize {
        match self {
            Field::Name => 0,
            Field::Bind => 1,
            Field::Seed => 2,
            Field::Create => 3,
        }
    }
}

enum FormOutcome {
    Submit,
    Quit,
}

enum AssetOutcome {
    Pick(Vec<(PathBuf, PathBuf)>),
    Back,
    Quit,
}

/// Run the form and optional asset checklist, returning after the normal screen is restored.
pub fn pick(cwd: &Path) -> crate::Result<Option<Picked>> {
    let suggestion = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let owner = crate::infra::credentials::current_user().unwrap_or_else(|| "local".into());
    let assets = crate::commands::init::find_seed_assets(cwd);
    let mut form = Form::default();
    let mut selected = vec![false; assets.len()];

    widgets::refresh_rc_status();
    let picked = {
        let mut guard = crate::tui::term::Guard::enter()?;
        let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
        let result = loop {
            match form_loop(
                &mut terminal,
                &mut form,
                &suggestion,
                &owner,
                cwd,
                assets.len(),
            )? {
                FormOutcome::Quit => break None,
                FormOutcome::Submit if form.seed && !assets.is_empty() => {
                    match asset_loop(&mut terminal, &assets, &mut selected)? {
                        AssetOutcome::Pick(picked) => {
                            break Some(Picked {
                                name: form.name.trim().to_string(),
                                bind: form.bind,
                                seed_assets: Some(picked),
                            });
                        }
                        AssetOutcome::Back => continue,
                        AssetOutcome::Quit => break None,
                    }
                }
                FormOutcome::Submit => {
                    break Some(Picked {
                        name: form.name.trim().to_string(),
                        bind: form.bind,
                        seed_assets: form.seed.then(Vec::new),
                    });
                }
            }
        };
        drop(terminal);
        guard.suspend()?;
        result
    };
    Ok(picked)
}

fn form_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    form: &mut Form,
    suggestion: &str,
    owner: &str,
    cwd: &Path,
    asset_count: usize,
) -> crate::Result<FormOutcome> {
    loop {
        terminal.draw(|frame| draw_form(frame, form, suggestion, owner, cwd, asset_count))?;
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        form.notice = None;
        if form.editing {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(FormOutcome::Quit);
                }
                KeyCode::Esc => form.editing = false,
                KeyCode::Backspace => {
                    form.name.pop();
                }
                KeyCode::Enter => {
                    form.editing = false;
                    form.field = Field::Bind;
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    form.name.push(ch);
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(FormOutcome::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(FormOutcome::Quit);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                form.field = form.field.next();
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                form.field = form.field.previous();
            }
            KeyCode::Char('e') if form.field == Field::Name => form.editing = true,
            KeyCode::Char(' ') => match form.field {
                Field::Bind => form.bind = !form.bind,
                Field::Seed => form.seed = !form.seed,
                _ => {}
            },
            KeyCode::Enter => match form.field {
                Field::Name => form.editing = true,
                Field::Bind => form.bind = !form.bind,
                Field::Seed => form.seed = !form.seed,
                Field::Create => match validate_name(&form.name) {
                    Ok(()) => return Ok(FormOutcome::Submit),
                    Err(error) => {
                        form.notice = Some(error);
                        form.field = Field::Name;
                    }
                },
            },
            _ => {}
        }
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("type the repo name; the directory name is only a suggestion.".into());
    }
    crate::domain::repo::valid_name(name).map_err(|error| format!("{error:#}"))
}

fn asset_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    assets: &[(PathBuf, PathBuf)],
    selected: &mut [bool],
) -> crate::Result<AssetOutcome> {
    let mut state = ListState::default();
    state.select(Some(0));
    loop {
        terminal.draw(|frame| draw_assets(frame, assets, selected, &mut state))?;
        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        match key.code {
            KeyCode::Char('q') => return Ok(AssetOutcome::Quit),
            KeyCode::Esc => return Ok(AssetOutcome::Back),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(AssetOutcome::Quit);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let index = state.selected().unwrap_or(0);
                state.select(Some((index + 1).min(assets.len().saturating_sub(1))));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let index = state.selected().unwrap_or(0);
                state.select(Some(index.saturating_sub(1)));
            }
            KeyCode::Char(' ') => {
                if let Some(index) = state.selected() {
                    selected[index] = !selected[index];
                }
            }
            KeyCode::Char('a') => {
                let take = selected.iter().any(|take| !take);
                selected.fill(take);
            }
            KeyCode::Enter => {
                let picked = assets
                    .iter()
                    .zip(selected.iter())
                    .filter(|(_, take)| **take)
                    .map(|(asset, _)| asset.clone())
                    .collect();
                return Ok(AssetOutcome::Pick(picked));
            }
            _ => {}
        }
    }
}

fn draw_form(
    frame: &mut Frame,
    form: &Form,
    suggestion: &str,
    owner: &str,
    cwd: &Path,
    asset_count: usize,
) {
    let panes = widgets::layout(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: "agit init".into(),
            identity: crate::infra::credentials::current_user()
                .map(|user| format!("{user} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: Default::default(),
        },
    );
    let list_area = widgets::list_area_with_notice(frame, panes, form.notice.as_deref());
    let name = if form.name.is_empty() {
        format!("<type name; suggestion: {suggestion}>")
    } else {
        format!("{}{}", form.name, if form.editing { "_" } else { "" })
    };
    let items = vec![
        ListItem::new(format!("name    {name}")),
        ListItem::new(format!(
            "bind    {}  {}",
            checkbox(form.bind),
            crate::ui::tilde(cwd)
        )),
        ListItem::new(format!(
            "seed    {}  inspect {asset_count} adoptable assets",
            checkbox(form.seed)
        )),
        ListItem::new("create  continue"),
    ];
    let mut state = ListState::default();
    state.select(Some(form.field.index()));
    frame.render_stateful_widget(
        List::new(items)
            .block(widgets::pane("new agent repo"))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        list_area,
        &mut state,
    );
    if let Some(area) = panes.detail {
        let mut detail = String::new();
        if let Some(notice) = &form.notice {
            detail.push_str(notice);
            detail.push_str("\n\n");
        }
        detail.push_str(&format!("owner  {owner}\n"));
        detail.push_str(&format!(
            "repo   {owner}/{}\n",
            if form.name.trim().is_empty() {
                "<name>"
            } else {
                form.name.trim()
            }
        ));
        detail.push_str(&format!(
            "bind   {}\n",
            if form.bind {
                crate::ui::tilde(cwd)
            } else {
                "do not bind this directory".to_string()
            }
        ));
        detail.push_str(&format!(
            "seed   {}\n\n",
            if form.seed {
                "review each asset next"
            } else {
                "do not inspect project assets"
            }
        ));
        detail.push_str(
            "The directory name is a suggestion only; typing the repo name makes the choice explicit.\n\nSeed choices begin empty because project instructions and skills may contain private memory.",
        );
        frame.render_widget(
            Paragraph::new(detail)
                .block(widgets::pane("summary"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
    widgets::render_footer(
        frame,
        panes.footer,
        if form.editing {
            "type name   enter done   esc stop editing"
        } else {
            "↑↓ field   enter edit/toggle/create   space toggle   q quit"
        },
    );
}

fn draw_assets(
    frame: &mut Frame,
    assets: &[(PathBuf, PathBuf)],
    selected: &[bool],
    state: &mut ListState,
) {
    let panes = widgets::layout_single(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: "agit init · seed".into(),
            identity: crate::infra::credentials::current_user()
                .map(|user| format!("{user} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: Default::default(),
        },
    );
    let items: Vec<ListItem> = assets
        .iter()
        .zip(selected.iter())
        .map(|((destination, source), take)| {
            ListItem::new(format!(
                "{} {} ← {}",
                checkbox(*take),
                destination.display(),
                source.display()
            ))
        })
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(widgets::pane("assets · none selected by default"))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        panes.list,
        state,
    );
    widgets::render_footer(
        frame,
        panes.footer,
        "↑↓ asset   space toggle   a all/none   enter confirm   esc back   q quit",
    );
}

fn checkbox(checked: bool) -> &'static str {
    if checked { "[x]" } else { "[ ]" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_suggestion_is_not_a_name_and_invalid_names_stay_invalid() {
        assert!(validate_name("").is_err());
        assert!(validate_name("bad/name").is_err());
        assert!(validate_name("agent-git").is_ok());
    }

    #[test]
    fn seed_choices_start_empty() {
        let assets = [
            (PathBuf::from("AGENTS.md"), PathBuf::from("/p/AGENTS.md")),
            (PathBuf::from("CLAUDE.md"), PathBuf::from("/p/CLAUDE.md")),
        ];
        let selected = vec![false; assets.len()];
        let picked: Vec<_> = assets
            .iter()
            .zip(selected.iter())
            .filter(|(_, take)| **take)
            .collect();
        assert!(picked.is_empty());
    }

    #[test]
    fn the_frame_shows_the_explicit_name_bind_and_seed_decisions() {
        use ratatui::backend::TestBackend;
        let cwd = Path::new("/Projects/agent-git");
        let form = Form {
            name: "work-memory".into(),
            seed: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 14)).unwrap();
        terminal
            .draw(|frame| draw_form(frame, &form, "agent-git", "nana", cwd, 3))
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
            "agit init",
            "work-memory",
            "nana/work-memory",
            "/Projects/agent-git",
            "inspect 3 adoptable asset",
        ] {
            assert!(
                text.contains(expected),
                "missing `{expected}` from frame: {text}"
            );
        }
    }

    #[test]
    fn the_summary_says_when_the_directory_will_not_be_bound() {
        use ratatui::backend::TestBackend;
        let cwd = Path::new("/Projects/agent-git");
        let form = Form {
            name: "work-memory".into(),
            bind: false,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(110, 14)).unwrap();
        terminal
            .draw(|frame| draw_form(frame, &form, "agent-git", "nana", cwd, 0))
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
            text.contains("bind   do not bind this directory"),
            "the summary must match the no-bind execution path: {text}"
        );
    }
}
