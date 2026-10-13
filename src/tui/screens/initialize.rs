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
    pub auto_push: Option<bool>,
    /// `None` means seed was not requested. `Some` is the exact set confirmed in the asset screen.
    pub seed_assets: Option<Vec<(PathBuf, PathBuf)>>,
}

#[derive(Debug, Clone)]
struct Form {
    name: String,
    bind: bool,
    seed: bool,
    auto_push: Option<bool>,
    user_auto_push: bool,
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
            auto_push: None,
            user_auto_push: false,
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
    AutoPush,
    Create,
}

impl Field {
    fn next(self) -> Field {
        match self {
            Field::Name => Field::Bind,
            Field::Bind => Field::Seed,
            Field::Seed => Field::AutoPush,
            Field::AutoPush => Field::Create,
            Field::Create => Field::Name,
        }
    }

    fn previous(self) -> Field {
        match self {
            Field::Name => Field::Create,
            Field::Bind => Field::Name,
            Field::Seed => Field::Bind,
            Field::Create => Field::AutoPush,
            Field::AutoPush => Field::Seed,
        }
    }

    fn index(self) -> usize {
        match self {
            Field::Name => 0,
            Field::Bind => 1,
            Field::Seed => 2,
            Field::AutoPush => 3,
            Field::Create => 4,
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
    crate::telemetry::measure_pick(crate::telemetry::Operation::TuiInitialize, || {
        pick_telemetry_inner(cwd)
    })
}

fn pick_telemetry_inner(cwd: &Path) -> crate::Result<Option<Picked>> {
    let suggestion = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let owner = crate::infra::credentials::current_user().unwrap_or_else(|| "local".into());
    let assets = crate::commands::init::find_seed_assets(cwd);
    let mut form = Form {
        user_auto_push: crate::infra::config::auto_push_default()?,
        ..Default::default()
    };
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
                                auto_push: form.auto_push,
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
                        auto_push: form.auto_push,
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
                Field::Seed => toggle_file_import(form, asset_count),
                Field::AutoPush => form.auto_push = cycle_auto_push(form.auto_push),
                _ => {}
            },
            KeyCode::Enter => match form.field {
                Field::Name => form.editing = true,
                Field::Bind => form.bind = !form.bind,
                Field::Seed => toggle_file_import(form, asset_count),
                Field::AutoPush => form.auto_push = cycle_auto_push(form.auto_push),
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

fn toggle_file_import(form: &mut Form, asset_count: usize) {
    if asset_count == 0 {
        form.seed = false;
        form.notice = Some(
            "No instructions or skills were found in this folder. See Details for supported locations."
                .into(),
        );
    } else {
        form.seed = !form.seed;
    }
}

fn cycle_auto_push(choice: Option<bool>) -> Option<bool> {
    match choice {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
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
    let folder_choice = if form.bind {
        crate::ui::tilde(cwd)
    } else {
        "do not link".to_string()
    };
    let import_choice = if asset_count == 0 {
        "no supported files found".to_string()
    } else if form.seed {
        format!("review {asset_count} instructions/skills")
    } else {
        format!("skip {asset_count} instructions/skills")
    };
    let items = vec![
        ListItem::new(Line::styled(
            "Review these choices before creating the repo.",
            theme::muted(),
        )),
        ListItem::new(format!("repo name        {name}")),
        ListItem::new(format!(
            "link folder      {}  {folder_choice}",
            checkbox(form.bind),
        )),
        ListItem::new(format!(
            "import files     {}  {import_choice}",
            checkbox(form.seed && asset_count > 0)
        )),
        ListItem::new(format!(
            "auto push        {}",
            match form.auto_push {
                Some(true) => "on  [x] push settled turns",
                Some(false) => "off [ ] keep turns local",
                None if form.user_auto_push => "on  [-] user preference",
                None => "off [-] user preference",
            }
        )),
        ListItem::new(
            if owner == "local" && form.auto_push.unwrap_or(form.user_auto_push) {
                "create repo      sign in, then create"
            } else {
                "create repo      continue"
            },
        ),
    ];
    let mut state = ListState::default();
    state.select(Some(form.field.index() + 1));
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
        detail.push_str(&format!("owner       {owner}\n"));
        detail.push_str(&format!(
            "agent repo  {owner}/{}\n",
            if form.name.trim().is_empty() {
                "<name>"
            } else {
                form.name.trim()
            }
        ));
        detail.push_str(&format!(
            "folder link {}\n",
            if form.bind {
                crate::ui::tilde(cwd)
            } else {
                "disabled".to_string()
            }
        ));
        detail.push_str(&format!(
            "file import {}\n\n",
            if asset_count == 0 {
                "no supported files found".to_string()
            } else if form.seed {
                format!("review {asset_count} files next")
            } else {
                format!("skip {asset_count} found files")
            }
        ));
        detail.push_str(&field_detail(form, asset_count));
        frame.render_widget(
            Paragraph::new(detail)
                .block(widgets::pane("details"))
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

fn field_detail(form: &Form, asset_count: usize) -> String {
    match form.field {
        Field::Name => "Creates an Agent repo for conversation history and shared instructions. This does not create or rename the project's code repo. The folder name is only a suggestion; type the repo name you want.".into(),
        Field::Bind if form.bind => "Links this folder to the Agent repo as the destination for new sessions. Existing sessions still need an explicit target. Files in the folder are not changed.".into(),
        Field::Bind => "Does not link this folder. You will need to name the Agent repo explicitly in later agit commands.".into(),
        Field::Seed if asset_count == 0 => "No importable files were found here. agit checks AGENTS.md, CLAUDE.md, and .claude/skills/<name>/SKILL.md inside this folder.".into(),
        Field::Seed if form.seed => "After Create, review the found instructions and skills one by one. Only the files you select will be copied into the Agent repo.".into(),
        Field::Seed => "Enable this to review instructions and skills from this folder before copying selected files into the Agent repo.".into(),
        Field::AutoPush => "Choose whether settled turns are pushed automatically or kept local. Inherit uses your user preference. Automatic push requires signing in and still checks content before publishing.".into(),
        Field::Create => "Creates the Agent repo and its main line of shared instructions and skills using the choices above. This does not start an agent session.".into(),
    }
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
            title: "agit init · import files".into(),
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
            .block(widgets::pane(
                "instructions and skills · none selected by default",
            ))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        panes.list,
        state,
    );
    widgets::render_footer(
        frame,
        panes.footer,
        "↑↓ file   space toggle   a all/none   enter confirm   esc back   q quit",
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
    fn the_frame_explains_the_repo_folder_link_and_file_import_choices() {
        use ratatui::backend::TestBackend;
        let cwd = Path::new("/Projects/agent-git");
        let form = Form {
            name: "work-memory".into(),
            seed: true,
            field: Field::Seed,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(160, 16)).unwrap();
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
            "Review these choices before creating the repo.",
            "work-memory",
            "nana/work-memory",
            "/Projects/agent-git",
            "link folder",
            "import files",
            "review 3 instructions/skills",
            "After Create, review the found instructions and skills one by one.",
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
            field: Field::Bind,
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
            text.contains("folder link disabled"),
            "the details must match the unlinked execution path: {text}"
        );
        assert!(text.contains("Does not link this folder."));
    }

    #[test]
    fn inherited_push_values_remain_visible_in_a_standard_terminal() {
        use ratatui::backend::TestBackend;
        for enabled in [false, true] {
            let form = Form {
                name: "work-memory".into(),
                field: Field::AutoPush,
                user_auto_push: enabled,
                ..Default::default()
            };
            let mut terminal = Terminal::new(TestBackend::new(120, 16)).unwrap();
            terminal
                .draw(|frame| draw_form(frame, &form, "project", "nana", Path::new("/project"), 0))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            let value = if enabled { "on " } else { "off" };
            assert!(
                text.contains(&format!("auto push        {value} [-]")),
                "{text}"
            );
        }
    }

    #[test]
    fn file_import_cannot_be_enabled_when_nothing_is_found() {
        let mut form = Form {
            seed: true,
            field: Field::Seed,
            ..Default::default()
        };

        toggle_file_import(&mut form, 0);

        assert!(!form.seed);
        assert_eq!(
            form.notice.as_deref(),
            Some(
                "No instructions or skills were found in this folder. See Details for supported locations."
            )
        );
        assert!(field_detail(&form, 0).contains(".claude/skills/<name>/SKILL.md"));
    }

    #[test]
    fn the_review_screen_names_the_files_explicitly() {
        use ratatui::backend::TestBackend;
        let assets = [(PathBuf::from("AGENTS.md"), PathBuf::from("/p/AGENTS.md"))];
        let selected = [false];
        let mut state = ListState::default();
        state.select(Some(0));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();

        terminal
            .draw(|frame| draw_assets(frame, &assets, &selected, &mut state))
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
        assert!(text.contains("agit init · import files"));
        assert!(text.contains("instructions and skills · none selected by default"));
    }
}
