//! The standalone `agit import` screen: choose one unmanaged runtime session and its destination.
//!
//! The screen makes no adoption decision of its own. It returns ordinary import arguments, leaves
//! the alternate screen, and lets [`crate::commands::import`] perform every permission, lineage,
//! branch and settlement check on the normal command path.
//!
//! # Bounded previews
//!
//! The shared naming collector spends a fixed probe budget rejecting empty startup transcripts.
//! Codex then hands the opening prompt over in its index. Claude does not, so the selected row is
//! parsed on demand and cached. Moving the cursor may parse one more candidate; filtering never
//! refetches data, and candidates beyond the probe budget cost nothing until selected.

use super::{repos, sessions};
use crate::domain::link;
use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The explicit arguments the screen gives back to `agit import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub runtime: String,
    pub session_id: String,
    /// `None` is the `--link-only` path; otherwise the repo and new branch are both explicit.
    pub destination: Option<(String, String)>,
    pub link_only: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    runtime: String,
    session_id: String,
    path: PathBuf,
    gist: Option<String>,
    last_active: SystemTime,
    live: bool,
}

impl Candidate {
    fn key(&self) -> (String, String) {
        (self.runtime.clone(), self.session_id.clone())
    }

    fn haystack(&self, preview: Option<&str>) -> String {
        format!(
            "{} {} {}",
            self.runtime,
            self.session_id,
            preview.or(self.gist.as_deref()).unwrap_or_default()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Repos,
    Sessions,
    Destination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayoutMode {
    ThreeColumn,
    TwoColumn,
    SingleStage,
}

#[derive(Debug, Clone, Copy)]
struct Areas {
    status: Rect,
    stage: Option<Rect>,
    repos: Rect,
    sessions: Rect,
    destination: Rect,
    footer: Rect,
    mode: LayoutMode,
}

const MIN_THREE_PANE_WIDTH: u16 = 120;

fn areas(area: Rect, has_notice: bool) -> Areas {
    if area.width >= MIN_THREE_PANE_WIDTH {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
        let body = Layout::horizontal([
            Constraint::Percentage(28),
            Constraint::Percentage(44),
            Constraint::Percentage(28),
        ])
        .split(rows[1]);
        Areas {
            status: rows[0],
            stage: None,
            repos: body[0],
            sessions: body[1],
            destination: body[2],
            footer: rows[2],
            mode: LayoutMode::ThreeColumn,
        }
    } else if area.width >= widgets::MIN_TWO_PANE_WIDTH {
        let destination_height = if has_notice { 10 } else { 8 };
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(destination_height),
            Constraint::Length(1),
        ])
        .split(area);
        let body = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(rows[1]);
        Areas {
            status: rows[0],
            stage: None,
            repos: body[0],
            sessions: body[1],
            destination: rows[2],
            footer: rows[3],
            mode: LayoutMode::TwoColumn,
        }
    } else {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
        Areas {
            status: rows[0],
            stage: Some(rows[1]),
            repos: rows[2],
            sessions: rows[2],
            destination: rows[2],
            footer: rows[3],
            mode: LayoutMode::SingleStage,
        }
    }
}

fn next_stage(focus: Focus, repo_selected: bool, session_selected: bool, link_only: bool) -> Focus {
    match focus {
        Focus::Repos if repo_selected || link_only => Focus::Sessions,
        Focus::Repos => Focus::Repos,
        Focus::Sessions if session_selected => Focus::Destination,
        Focus::Sessions => Focus::Sessions,
        Focus::Destination => Focus::Destination,
    }
}

fn previous_stage(focus: Focus) -> Focus {
    match focus {
        Focus::Destination => Focus::Sessions,
        Focus::Sessions => Focus::Repos,
        Focus::Repos => Focus::Repos,
    }
}

fn clamp_selection(state: &mut ListState, len: usize) {
    if len == 0 {
        state.select(None);
    } else if state.selected().unwrap_or(0) >= len {
        state.select(Some(len - 1));
    } else if state.selected().is_none() {
        state.select(Some(0));
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

/// Gather unmanaged sessions in this directory without opening their transcripts.
fn collect(cwd: &Path) -> Vec<Candidate> {
    let store = crate::domain::store::Store::open_or_init().ok();
    let links = store.as_ref().map(link::list).unwrap_or_default();
    let link_refs = links.iter().collect::<Vec<_>>();
    let now = SystemTime::now();
    sessions::probe_sessions_for_naming(cwd, &link_refs)
        .into_iter()
        .filter(|item| item.worth_naming)
        .map(|item| {
            let session = item.session;
            Candidate {
                runtime: session.runtime.to_string(),
                session_id: session.id,
                path: session.path,
                gist: session.gist,
                last_active: session.mtime,
                live: sessions::is_live(session.mtime, now),
            }
        })
        .collect()
}

/// Open the picker, returning only after the normal screen owns the terminal again.
pub fn pick(cwd: &Path) -> crate::Result<Option<Picked>> {
    let candidates = collect(cwd);
    if candidates.is_empty() {
        println!(
            "{}",
            crate::ui::dim(&format!(
                "no unadopted sessions under {}.",
                crate::ui::tilde(cwd)
            ))
        );
        crate::ui::hint(
            "session ran in another directory? give the id directly: agit import <session-id> -n <name>",
        );
        return Ok(None);
    }

    let repos = repos::collect(crate::commands::new::DEFAULT_FROM);
    let preferred = crate::commands::context::repo_for(cwd).ok();
    let repo_index = preferred
        .as_deref()
        .and_then(|slug| repos.iter().position(|repo| repo.slug() == slug))
        .unwrap_or(0);
    widgets::refresh_rc_status();
    let picked = {
        let mut guard = crate::tui::term::Guard::enter()?;
        let outcome = run_loop(&candidates, &repos, repo_index);
        guard.suspend()?;
        outcome?
    };
    Ok(picked)
}

fn preview(candidate: &Candidate) -> String {
    candidate
        .gist
        .as_deref()
        .filter(|gist| !gist.trim().is_empty())
        .map(|gist| crate::ui::truncate(gist, 72))
        .unwrap_or_else(|| {
            crate::commands::import::gist_for(
                &candidate.runtime,
                &candidate.session_id,
                &candidate.path,
            )
        })
}

fn validate(
    candidate: &Candidate,
    repo: Option<&repos::Row>,
    branch: &str,
    link_only: bool,
    signed_in: bool,
) -> Result<Picked, String> {
    if candidate.live {
        return Err(
            "this session still looks active. exit it in its own terminal before adopting it."
                .into(),
        );
    }
    if link_only {
        return Ok(Picked {
            runtime: candidate.runtime.clone(),
            session_id: candidate.session_id.clone(),
            destination: None,
            link_only: true,
        });
    }
    if !signed_in {
        return Err(
            "recording the first version needs a sign-in. choose link-only, or quit and run `agit login`."
                .into(),
        );
    }
    let repo = repo.ok_or_else(|| {
        "there is no local agit repo to adopt into. choose link-only, or quit and run `agit init <name>`."
            .to_string()
    })?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err("type a branch name first.".into());
    }
    crate::domain::repo::valid_branch_name(branch).map_err(|error| format!("{error:#}"))?;
    let slug = repo.slug();
    crate::commands::target::branch_only(&format!("{slug}@{branch}"))
        .map_err(|error| format!("{error:#}"))?;
    if repo.branches.iter().any(|existing| existing == branch) {
        return Err(format!(
            "`{branch}` already exists in {slug} — choose a new session branch."
        ));
    }
    Ok(Picked {
        runtime: candidate.runtime.clone(),
        session_id: candidate.session_id.clone(),
        destination: Some((slug, branch.to_string())),
        link_only: false,
    })
}

fn run_loop(
    candidates: &[Candidate],
    repos: &[repos::Row],
    repo_index: usize,
) -> crate::Result<Option<Picked>> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut session_state = ListState::default();
    session_state.select(Some(0));
    let mut repo_state = ListState::default();
    repo_state.select((!repos.is_empty()).then_some(repo_index % repos.len().max(1)));
    let mut session_filter = Filter::default();
    let mut repo_filter = Filter::default();
    let mut previews: HashMap<(String, String), String> = HashMap::new();
    let mut branch = String::new();
    let mut link_only = false;
    let mut notice: Option<String> = None;
    let signed_in = crate::infra::credentials::current_user().is_some();
    let mut focus = if repos.len() == 1 {
        Focus::Sessions
    } else {
        Focus::Repos
    };

    loop {
        let session_view: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| {
                let cached = previews.get(&candidate.key()).map(String::as_str);
                session_filter.matches(&candidate.haystack(cached))
            })
            .collect();
        let repo_view: Vec<&repos::Row> = repos
            .iter()
            .filter(|repo| repo_filter.matches(&repo.haystack()))
            .collect();
        clamp_selection(&mut session_state, session_view.len());
        clamp_selection(&mut repo_state, repo_view.len());
        if let Some(candidate) = session_state
            .selected()
            .and_then(|index| session_view.get(index))
            .copied()
        {
            previews
                .entry(candidate.key())
                .or_insert_with(|| preview(candidate));
        }
        let repo = repo_state
            .selected()
            .and_then(|index| repo_view.get(index))
            .copied();
        term.draw(|frame| {
            draw(
                frame,
                &repo_view,
                &mut repo_state,
                &session_view,
                &mut session_state,
                repo,
                &branch,
                focus,
                link_only,
                &repo_filter,
                &session_filter,
                &previews,
                notice.as_deref(),
            )
        })?;

        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        let active_filter = match focus {
            Focus::Repos if !link_only => Some(&mut repo_filter),
            Focus::Sessions => Some(&mut session_filter),
            Focus::Repos | Focus::Destination => None,
        };
        if let Some(filter) = active_filter.filter(|filter| filter.is_active()) {
            match key.code {
                KeyCode::Esc => filter.close(),
                KeyCode::Enter => filter.blur(),
                KeyCode::Backspace => filter.pop(),
                KeyCode::Char(ch) => filter.push(ch),
                _ => {}
            }
            continue;
        }
        notice = None;
        if focus == Focus::Destination {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Char('q') if link_only => return Ok(None),
                KeyCode::Esc | KeyCode::BackTab => {
                    focus = previous_stage(focus);
                }
                KeyCode::Backspace if !link_only => {
                    branch.pop();
                }
                KeyCode::Enter => {
                    let candidate = session_state
                        .selected()
                        .and_then(|index| session_view.get(index))
                        .copied();
                    let Some(candidate) = candidate else {
                        continue;
                    };
                    match validate(candidate, repo, &branch, link_only, signed_in) {
                        Ok(picked) => return Ok(Some(picked)),
                        Err(error) => notice = Some(error),
                    }
                }
                KeyCode::Char(ch)
                    if !link_only
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    branch.push(ch);
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') => return Ok(None),
            KeyCode::Esc => {
                let previous = previous_stage(focus);
                if previous == focus && focus == Focus::Repos {
                    return Ok(None);
                }
                focus = previous;
            }
            KeyCode::BackTab => focus = previous_stage(focus),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(None);
            }
            KeyCode::Char('/') => match focus {
                Focus::Repos if !link_only => repo_filter.open(),
                Focus::Sessions => session_filter.open(),
                Focus::Repos | Focus::Destination => {}
            },
            KeyCode::Down | KeyCode::Char('j') => match focus {
                Focus::Repos if !link_only => move_selection(&mut repo_state, repo_view.len(), 1),
                Focus::Sessions => {
                    move_selection(&mut session_state, session_view.len(), 1);
                    branch.clear();
                }
                Focus::Repos | Focus::Destination => {}
            },
            KeyCode::Up | KeyCode::Char('k') => match focus {
                Focus::Repos if !link_only => move_selection(&mut repo_state, repo_view.len(), -1),
                Focus::Sessions => {
                    move_selection(&mut session_state, session_view.len(), -1);
                    branch.clear();
                }
                Focus::Repos | Focus::Destination => {}
            },
            KeyCode::Char('g') | KeyCode::Home => match focus {
                Focus::Repos if !link_only => {
                    repo_state.select((!repo_view.is_empty()).then_some(0))
                }
                Focus::Sessions => {
                    session_state.select((!session_view.is_empty()).then_some(0));
                    branch.clear();
                }
                Focus::Repos | Focus::Destination => {}
            },
            KeyCode::Char('G') | KeyCode::End => match focus {
                Focus::Repos if !link_only => repo_state.select(repo_view.len().checked_sub(1)),
                Focus::Sessions => {
                    session_state.select(session_view.len().checked_sub(1));
                    branch.clear();
                }
                Focus::Repos | Focus::Destination => {}
            },
            KeyCode::Tab => {
                focus = next_stage(
                    focus,
                    repo.is_some(),
                    session_state.selected().is_some(),
                    link_only,
                );
            }
            KeyCode::Char('l') | KeyCode::Char(' ') => {
                link_only = !link_only;
                if link_only && focus == Focus::Repos {
                    focus = Focus::Sessions;
                }
            }
            KeyCode::Enter if focus == Focus::Repos => {
                focus = next_stage(focus, repo.is_some(), false, link_only);
            }
            KeyCode::Enter if focus == Focus::Sessions => {
                let candidate = session_state
                    .selected()
                    .and_then(|index| session_view.get(index))
                    .copied();
                if candidate.is_none() {
                    continue;
                }
                focus = next_stage(focus, repo.is_some(), true, link_only);
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame,
    repo_view: &[&repos::Row],
    repo_state: &mut ListState,
    session_view: &[&Candidate],
    session_state: &mut ListState,
    repo: Option<&repos::Row>,
    branch: &str,
    focus: Focus,
    link_only: bool,
    repo_filter: &Filter,
    session_filter: &Filter,
    previews: &HashMap<(String, String), String>,
    notice: Option<&str>,
) {
    let layout = areas(frame.area(), notice.is_some());
    widgets::render_status(
        frame,
        layout.status,
        &widgets::Status {
            title: "agit import".into(),
            identity: crate::infra::credentials::current_user()
                .map(|user| format!("{user} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: widgets::Counters {
                unnamed: session_view.len(),
            },
        },
    );
    let selected = session_state
        .selected()
        .and_then(|index| session_view.get(index))
        .copied();
    let selected_preview = selected.and_then(|candidate| previews.get(&candidate.key()));
    let destination = detail_text(
        selected,
        repo,
        branch,
        focus == Focus::Destination,
        link_only,
        selected_preview.map(String::as_str),
        notice,
    );

    match layout.mode {
        LayoutMode::ThreeColumn => {
            render_repo_pane(
                frame,
                layout.repos,
                repo_view,
                repo_state,
                focus == Focus::Repos,
                link_only,
                repo_filter,
            );
            render_session_pane(
                frame,
                layout.sessions,
                session_view,
                session_state,
                focus == Focus::Sessions,
                session_filter,
                previews,
            );
            render_destination_pane(
                frame,
                layout.destination,
                destination,
                focus == Focus::Destination,
            );
        }
        LayoutMode::TwoColumn => {
            render_repo_pane(
                frame,
                layout.repos,
                repo_view,
                repo_state,
                focus == Focus::Repos,
                link_only,
                repo_filter,
            );
            render_session_pane(
                frame,
                layout.sessions,
                session_view,
                session_state,
                focus == Focus::Sessions,
                session_filter,
                previews,
            );
            render_destination_pane(
                frame,
                layout.destination,
                destination,
                focus == Focus::Destination,
            );
        }
        LayoutMode::SingleStage => {
            frame.render_widget(
                Paragraph::new(stage_text(focus, link_only)),
                layout
                    .stage
                    .expect("single-pane import layout has a stage row"),
            );
            match focus {
                Focus::Repos => render_repo_pane(
                    frame,
                    layout.repos,
                    repo_view,
                    repo_state,
                    true,
                    link_only,
                    repo_filter,
                ),
                Focus::Sessions => render_session_pane(
                    frame,
                    layout.sessions,
                    session_view,
                    session_state,
                    true,
                    session_filter,
                    previews,
                ),
                Focus::Destination => {
                    render_destination_pane(frame, layout.destination, destination, true)
                }
            }
        }
    }

    let footer = if focus == Focus::Destination {
        if link_only {
            "enter link   shift-tab/esc back   q quit"
        } else {
            "type branch   enter import   shift-tab/esc back"
        }
    } else if repo_filter.is_active() || session_filter.is_active() {
        "type to filter   enter keep   esc clear"
    } else if link_only && focus == Focus::Repos {
        "repo skipped   tab/enter next   l versioned   shift-tab stay   q quit"
    } else if link_only {
        "↑↓ move   tab/enter next   shift-tab/esc back   l versioned   / filter   q quit"
    } else {
        "↑↓ move   tab/enter next   shift-tab/esc back   l link-only   / filter   q quit"
    };
    widgets::render_footer(frame, layout.footer, footer);
}

fn stage_text(focus: Focus, link_only: bool) -> String {
    let marker = |stage| if focus == stage { "[*]" } else { "[ ]" };
    if link_only {
        format!(
            "{} 1 repo (skipped)  →  {} 2 session  →  {} 3 destination",
            marker(Focus::Repos),
            marker(Focus::Sessions),
            marker(Focus::Destination)
        )
    } else {
        format!(
            "{} 1 repo  →  {} 2 session  →  {} 3 destination",
            marker(Focus::Repos),
            marker(Focus::Sessions),
            marker(Focus::Destination)
        )
    }
}

fn render_repo_pane(
    frame: &mut Frame,
    area: Rect,
    view: &[&repos::Row],
    state: &mut ListState,
    active: bool,
    disabled: bool,
    filter: &Filter,
) {
    let width = area.width.saturating_sub(4) as usize;
    let items = view
        .iter()
        .map(|repo| {
            let access = if repo.read_only { " · read-only" } else { "" };
            ListItem::new(vec![
                widgets::clamp_line(Line::from(repo.slug()), width),
                widgets::clamp_line(
                    Line::from(Span::styled(
                        format!(
                            "  {} {} · from {}{}",
                            repo.sessions,
                            if repo.sessions == 1 {
                                "session"
                            } else {
                                "sessions"
                            },
                            repo.from_ref,
                            access
                        ),
                        theme::muted(),
                    )),
                    width,
                ),
            ])
        })
        .collect::<Vec<_>>();
    let mut title = pane_title("1 repos", view.len(), filter, active);
    if disabled {
        title.push_str(" · disabled");
    }
    let list = List::new(items)
        .block(widgets::pane(&title))
        .highlight_style(if active {
            theme::selected()
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol(if active { "▸ " } else { "• " });
    frame.render_stateful_widget(
        if disabled {
            list.style(theme::muted())
        } else {
            list
        },
        area,
        state,
    );
}

fn render_session_pane(
    frame: &mut Frame,
    area: Rect,
    view: &[&Candidate],
    state: &mut ListState,
    active: bool,
    filter: &Filter,
    previews: &HashMap<(String, String), String>,
) {
    let width = area.width.saturating_sub(4) as usize;
    let items = view
        .iter()
        .map(|candidate| {
            let preview = previews.get(&candidate.key()).map(String::as_str);
            ListItem::new(row_lines(candidate, preview, width))
        })
        .collect::<Vec<_>>();
    let title = pane_title("2 sessions", view.len(), filter, active);
    frame.render_stateful_widget(
        List::new(items)
            .block(widgets::pane(&title))
            .highlight_style(if active {
                theme::selected()
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            })
            .highlight_symbol(if active { "▸ " } else { "• " }),
        area,
        state,
    );
}

fn render_destination_pane(frame: &mut Frame, area: Rect, text: String, active: bool) {
    let title = if active {
        "3 destination*"
    } else {
        "3 destination"
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(widgets::pane(title)),
        area,
    );
}

fn row_lines(candidate: &Candidate, preview: Option<&str>, width: usize) -> Vec<Line<'static>> {
    let live = if candidate.live { " · active" } else { "" };
    let mut lines = vec![widgets::clamp_line(
        Line::from(format!(
            "{}  {}  {}{}",
            candidate.runtime,
            link::short(&candidate.session_id),
            crate::ui::ago(candidate.last_active),
            live
        )),
        width,
    )];
    if let Some(preview) = preview {
        lines.push(widgets::clamp_line(
            Line::from(Span::styled(format!("  {preview}"), theme::muted())),
            width,
        ));
    }
    lines
}

fn detail_text(
    candidate: Option<&Candidate>,
    repo: Option<&repos::Row>,
    branch: &str,
    editing: bool,
    link_only: bool,
    preview: Option<&str>,
    notice: Option<&str>,
) -> String {
    let Some(candidate) = candidate else {
        return "no session matches the filter.".into();
    };
    let mut out = String::new();
    if let Some(notice) = notice {
        out.push_str(notice);
        out.push_str("\n\n");
    }
    let repo_name = repo
        .map(repos::Row::slug)
        .unwrap_or_else(|| "no local agit repo".into());
    let access = if !link_only && repo.is_some_and(|repo| repo.read_only) {
        " · read-only"
    } else {
        ""
    };
    out.push_str(&format!(
        "repo       {}{}\n",
        if link_only { "skipped" } else { &repo_name },
        access
    ));
    let cursor = if editing && !link_only { "_" } else { "" };
    out.push_str(&format!(
        "branch     {}{cursor}\n",
        if link_only {
            "skipped"
        } else if branch.is_empty() {
            "<type name>"
        } else {
            branch
        }
    ));
    out.push_str(&format!("runtime    {}\n", candidate.runtime));
    out.push_str(&format!(
        "session    {}\n",
        link::short(&candidate.session_id)
    ));
    out.push_str(&format!(
        "active     {}\n",
        crate::ui::ago(candidate.last_active)
    ));
    out.push_str(&format!(
        "link only  {}\n",
        if link_only { "[x]" } else { "[ ]" }
    ));
    if link_only {
        out.push_str("           records only the session link\n");
    }
    if let Some(preview) = preview {
        out.push_str(&format!("\n{preview}\n"));
    }
    if candidate.live {
        out.push_str("\nthis session still looks active; adoption waits until it exits.\n");
    }
    out
}

fn pane_title(name: &str, count: usize, filter: &Filter, active: bool) -> String {
    let marker = if active { "*" } else { "" };
    if filter.query().is_empty() {
        format!("{name}{marker} ({count})")
    } else {
        format!("{name}{marker} ({count}) /{}", filter.query())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(live: bool) -> Candidate {
        Candidate {
            runtime: "codex".into(),
            session_id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
            path: "/tmp/session.jsonl".into(),
            gist: Some("fix the retry path".into()),
            last_active: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            live,
        }
    }

    fn repo(branches: &[&str]) -> repos::Row {
        repos::Row {
            owner: "nana".into(),
            name: "payments".into(),
            path: "/repo".into(),
            sessions: branches.len(),
            from_ref: "main".into(),
            from_line: Some(crate::domain::meta::Line::File),
            branches: branches.iter().map(|branch| (*branch).into()).collect(),
            read_only: false,
        }
    }

    fn rendered(width: u16, focus: Focus, notice: Option<&str>) -> String {
        use ratatui::backend::TestBackend;

        let rows = [candidate(false)];
        let view = vec![&rows[0]];
        let target = repo(&[]);
        let repo_view = vec![&target];
        let mut repo_state = ListState::default();
        repo_state.select(Some(0));
        let mut session_state = ListState::default();
        session_state.select(Some(0));
        let mut previews = HashMap::new();
        previews.insert(rows[0].key(), "fix the retry path".into());
        let mut term = Terminal::new(TestBackend::new(width, 20)).unwrap();
        term.draw(|frame| {
            draw(
                frame,
                &repo_view,
                &mut repo_state,
                &view,
                &mut session_state,
                Some(&target),
                "retry-fix",
                focus,
                false,
                &Filter::default(),
                &Filter::default(),
                &previews,
                notice,
            )
        })
        .unwrap();
        let buffer = term.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn link_only_needs_no_repo_or_branch_but_still_preserves_identity() {
        let picked = validate(&candidate(false), None, "", true, false).unwrap();
        assert_eq!(picked.runtime, "codex");
        assert_eq!(picked.session_id, "aaaaaaaa-0000-4000-8000-000000000001");
        assert!(picked.destination.is_none());
        assert!(picked.link_only);
    }

    #[test]
    fn a_versioned_import_requires_a_new_valid_branch() {
        let target = repo(&["taken"]);
        assert!(validate(&candidate(false), Some(&target), "", false, true).is_err());
        assert!(
            validate(
                &candidate(false),
                Some(&target),
                "agit-version",
                false,
                true
            )
            .is_err()
        );
        let duplicate =
            validate(&candidate(false), Some(&target), "taken", false, true).unwrap_err();
        assert!(duplicate.contains("already exists"), "{duplicate}");

        let picked = validate(&candidate(false), Some(&target), "fresh", false, true).unwrap();
        assert_eq!(
            picked.destination,
            Some(("nana/payments".into(), "fresh".into()))
        );
    }

    #[test]
    fn an_active_session_is_not_adopted_from_another_terminal() {
        assert!(validate(&candidate(true), None, "", true, false).is_err());
    }

    #[test]
    fn the_frame_exposes_session_destination_and_link_only_paths() {
        let text = rendered(120, Focus::Sessions, None);
        for expected in [
            "agit import",
            "codex",
            "fix the retry path",
            "nana/payments",
            "retry-fix",
            "link-only",
        ] {
            assert!(
                text.contains(expected),
                "missing `{expected}` from frame: {text}"
            );
        }
    }

    #[test]
    fn every_layout_marks_destination_focus_and_keeps_its_notice_visible() {
        for width in [MIN_THREE_PANE_WIDTH, MIN_THREE_PANE_WIDTH - 1, 79] {
            let text = rendered(width, Focus::Destination, Some("branch name is invalid"));
            assert!(text.contains("3 destination*"), "width {width}: {text}");
            assert!(
                text.contains("branch name is invalid"),
                "width {width}: {text}"
            );
        }
        let narrow = rendered(79, Focus::Destination, None);
        assert!(narrow.contains("[*] 3 destination"), "{narrow}");
    }

    #[test]
    fn import_layout_adapts_without_hiding_the_active_stage() {
        let three = areas(Rect::new(0, 0, MIN_THREE_PANE_WIDTH, 20), false);
        assert_eq!(three.mode, LayoutMode::ThreeColumn);
        assert_ne!(three.repos, three.sessions);
        assert_ne!(three.sessions, three.destination);
        assert!(three.stage.is_none());

        let two = areas(Rect::new(0, 0, MIN_THREE_PANE_WIDTH - 1, 20), false);
        assert_eq!(two.mode, LayoutMode::TwoColumn);
        assert_ne!(two.repos, two.sessions);
        assert!(two.destination.y > two.sessions.y);
        assert!(two.stage.is_none());

        let one = areas(Rect::new(0, 0, widgets::MIN_TWO_PANE_WIDTH - 1, 20), false);
        assert_eq!(one.mode, LayoutMode::SingleStage);
        assert_eq!(one.repos, one.sessions);
        assert_eq!(one.sessions, one.destination);
        assert!(one.stage.is_some());
    }

    #[test]
    fn stages_advance_and_retreat_without_changing_selection() {
        let mut state = ListState::default();
        state.select(Some(1));
        assert_eq!(
            next_stage(Focus::Repos, true, false, false),
            Focus::Sessions
        );
        assert_eq!(
            next_stage(Focus::Sessions, true, true, false),
            Focus::Destination
        );
        assert_eq!(previous_stage(Focus::Destination), Focus::Sessions);
        assert_eq!(previous_stage(Focus::Sessions), Focus::Repos);
        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn unavailable_or_skipped_repos_have_explicit_stage_boundaries() {
        assert_eq!(next_stage(Focus::Repos, false, false, false), Focus::Repos);
        assert_eq!(
            next_stage(Focus::Repos, false, false, true),
            Focus::Sessions
        );
        assert_eq!(
            next_stage(Focus::Sessions, true, false, false),
            Focus::Sessions
        );
        assert_eq!(previous_stage(Focus::Sessions), Focus::Repos);
    }
}
