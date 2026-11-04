//! Whether agit is set up on this computer, and what a command says when it is not.
//!
//! An installed agit can be left without setup without anyone choosing that: npm 12 blocks an
//! install script it has not been told to allow, a policy can block or skip install scripts,
//! pnpm's install script configures nothing, and a copied binary has none. Such an agit captures
//! no conversation. Setting it up writes into the person's runtimes, so it stays the person's
//! choice: a terminal a person answers is asked once, with No as the default; every other caller
//! gets one line a day that tells whoever reads it to ask the person first.
//!
//! Detection reads file metadata, and, where no runtime has agit's Skill, the hook files
//! `agit setup --hooks` writes, only to learn whether they carry agit's hooks. The one file this
//! writes is [`RECORD`] in `AGIT_HOME`, holding when the line was last shown and whether the
//! person declined; nothing outside `AGIT_HOME`, and nothing in any runtime's configuration, is
//! written short of `agit setup`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::commands::CmdResult;
use crate::{ExitCode, ui};

/// The notice record, in `AGIT_HOME`. A successful `agit setup` removes it.
pub(crate) const RECORD: &str = "setup-notice.json";

/// The least time between two unattended notices.
const QUIET_PERIOD: chrono::Duration = chrono::Duration::hours(24);

/// The line for a caller that cannot answer. An agent reads it too, so it tells the reader to ask
/// the person instead of handing it a command to run.
pub(crate) const UNATTENDED_LINE: &str = "agit is not set up on this computer, so nothing is captured. Ask the person before running `agit setup`.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Presence {
    /// Some runtime has agit's Skill, or agit's capture hooks.
    SetUp,
    /// No runtime agit sets up is on this computer, so there is nothing to set up.
    NoRuntime,
    /// These runtimes are here and none of them has agit's Skill.
    NotSetUp(Vec<&'static str>),
    /// agit's Skill is set up for some runtime, but these runtimes are here with neither its Skill
    /// nor its capture hooks, such as one installed after the last setup.
    Partly(Vec<&'static str>),
}

/// [`UNATTENDED_LINE`] for runtimes here that the last setup did not cover.
fn unattended_partly(runtimes: &[&str]) -> String {
    format!(
        "agit is not set up for {} on this computer, so their conversations are not captured. Ask the person before running `agit setup`.",
        labels(runtimes)
    )
}

pub(crate) fn labels(runtimes: &[&str]) -> String {
    runtimes
        .iter()
        .map(|id| crate::adapter::runtime_label(id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether agit is set up here. agit counts as set up when any runtime has its Skill entrypoint,
/// which every full setup writes, or when `agit setup --hooks` left agit's hooks, which capture
/// adopted sessions without the Skill. A runtime counts as here when its configuration folder
/// exists, which it makes when it first runs: a program on PATH that never ran has no
/// conversation to capture yet, and counting it would make the answer depend on the machine
/// rather than on the home. Only the folder's existence is read, never what is in it. The hook
/// files are read only where no runtime has the Skill and some runtime is here, and only whether
/// they carry agit's hooks is kept. `agit setup` does not ask this; it installs for runtimes
/// whether or not they have run.
pub(crate) fn presence() -> Presence {
    // A Skill the person turned off is a choice made in setup, not a setup still to do.
    let off = super::skill_off::read();
    let mut skilled = !off.is_empty();
    let mut here = Vec::new();
    for id in crate::adapter::setup_runtimes() {
        let Some(skill) = super::skill_path(id) else {
            continue;
        };
        if skill.join("SKILL.md").symlink_metadata().is_ok() {
            skilled = true;
        } else if !off.contains(id)
            && runtime_folder(id, &skill).is_some_and(|folder| folder.symlink_metadata().is_ok())
        {
            here.push(id);
        }
    }
    if skilled {
        // After a full setup, a runtime here without agit is one it did not cover, such as one
        // installed since; one carrying agit's hooks alone was set up that way on purpose.
        here.retain(|id| !super::runtime_hooks_installed(id));
        return if here.is_empty() {
            Presence::SetUp
        } else {
            Presence::Partly(here)
        };
    }
    if here.is_empty() {
        Presence::NoRuntime
    } else if super::hooks_installed() {
        Presence::SetUp
    } else {
        Presence::NotSetUp(here)
    }
}

/// The runtimes installed on this computer, which bare `agit setup` sets up: those whose program is
/// on PATH, or whose configuration folder exists, as it does once the runtime has run. A runtime
/// installed later is set up by the next bare setup.
pub(crate) fn installed() -> Vec<&'static str> {
    crate::adapter::setup_runtimes()
        .filter(|id| {
            // `available` answers whether sessions can be resumed, which some adapters deny
            // whatever is installed, so the program on PATH is asked about on its own.
            crate::adapter::get(id).is_ok_and(|adapter| {
                adapter.available() || crate::adapter::which(adapter.cli()).is_some()
            }) || super::skill_path(id)
                .and_then(|skill| runtime_folder(id, &skill))
                .is_some_and(|folder| folder.symlink_metadata().is_ok())
        })
        .collect()
}

/// The folder a runtime keeps its configuration in, resolved as the runtime resolves it: the Skill
/// sits under it for every runtime but Claude Code, whose folder follows `CLAUDE_CONFIG_DIR`.
fn runtime_folder(id: &str, skill: &Path) -> Option<PathBuf> {
    if id == "claude-code" {
        let projects = crate::adapter::claude_code::projects_dir().ok()?;
        return projects.parent().map(Path::to_path_buf);
    }
    skill.parent().and_then(Path::parent).map(Path::to_path_buf)
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    #[serde(default)]
    pub(crate) declined_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) noticed_at: Option<DateTime<Utc>>,
}

fn record_path() -> Option<PathBuf> {
    crate::infra::config::agit_home()
        .ok()
        .map(|home| home.join(RECORD))
}

/// The record, or an empty one when it is absent or unreadable: an unreadable record can only
/// make the notice appear again, never write anything else.
pub(crate) fn record() -> Record {
    record_path()
        .and_then(|path| crate::domain::storage::read_bytes_capped(&path, 4096).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Written only into an `AGIT_HOME` that already exists, so the record is the one path the notice
/// ever creates. Without one, an unattended notice repeats until some command creates it.
fn save(record: &Record) {
    let Some(path) = record_path() else {
        return;
    };
    if !path.parent().is_some_and(Path::is_dir) {
        return;
    }
    if let Ok(text) = serde_json::to_vec(record) {
        let _ = std::fs::write(path, text);
    }
}

/// Forget the record once setup has run, so a later state starts from a clean slate.
pub(crate) fn forget() {
    if let Some(path) = record_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Nothing,
    /// Print [`UNATTENDED_LINE`] and stamp the record.
    Say,
    /// Show what setup would change and ask, default No.
    Ask,
}

/// What a command says about setup. `can_ask` is whether a person answers this terminal;
/// `silenced` is `AGIT_SKIP_SETUP` or quiet presentation.
pub(crate) fn decide(
    presence: &Presence,
    record: &Record,
    can_ask: bool,
    silenced: bool,
    now: DateTime<Utc>,
) -> Action {
    if silenced
        || record.declined_at.is_some()
        || !matches!(presence, Presence::NotSetUp(_) | Presence::Partly(_))
    {
        return Action::Nothing;
    }
    if can_ask {
        return Action::Ask;
    }
    match record.noticed_at {
        Some(at) if now.signed_duration_since(at) < QUIET_PERIOD && at <= now => Action::Nothing,
        _ => Action::Say,
    }
}

/// `AGIT_SKIP_SETUP` as the npm install scripts read it: set to anything but empty, `0` or `false`.
fn skip_setup() -> bool {
    std::env::var("AGIT_SKIP_SETUP")
        .is_ok_and(|value| !matches!(value.to_ascii_lowercase().as_str(), "" | "0" | "false"))
}

/// The notice the running command carries, and whether it may ask. [`arm`] sets it when the
/// command is parsed and [`when_ready`] takes it once the command has passed its argument and
/// precondition checks. A command that refuses what it was given, or that the person cancels,
/// never takes it: it says nothing about setup and writes no record, so the notice still reaches
/// the next run that does something.
static PENDING: Mutex<Option<bool>> = Mutex::new(None);

/// Carry the notice for this command, outside any JSON capture. `may_ask` is false for a command
/// that must never put the question to a person. Nothing is read or shown until [`when_ready`].
pub fn arm(may_ask: bool) {
    if let Ok(mut pending) = PENDING.lock() {
        *pending = Some(may_ask);
    }
}

/// Show the notice the command carries, at most once. A command calls this when it has passed its
/// checks: before it hands the terminal to a runtime, or once it has done what it was asked.
/// Without [`arm`] it does nothing, so the bare TUI and the commands it starts carry no notice.
pub(crate) fn when_ready() {
    let Some(may_ask) = PENDING.lock().ok().and_then(|mut pending| pending.take()) else {
        return;
    };
    notice(may_ask);
}

/// [`when_ready`] once a command has finished what it was asked; a refusal or failure passes
/// through without the notice.
pub(crate) fn after(result: CmdResult) -> CmdResult {
    if matches!(result, Ok(ExitCode::Ok)) {
        when_ready();
    }
    result
}

fn notice(may_ask: bool) {
    let silenced = skip_setup() || ui::quiet();
    let record = record();
    // Settled before looking at any runtime, so a declined or silenced agit stats nothing.
    if silenced || record.declined_at.is_some() {
        return;
    }
    let presence = presence();
    let now = Utc::now();
    // Under CI the answer is meant for a program, and `--yes` asks not to be asked; neither is a
    // person agreeing to capture hooks, so both get the unattended line.
    let person =
        may_ask && ui::prompt::may_prompt() && !crate::telemetry::state::positive("AGIT_YES");
    match decide(&presence, &record, person, silenced, now) {
        Action::Nothing => {}
        Action::Say => {
            match &presence {
                Presence::Partly(runtimes) => {
                    eprintln!("{}", ui::dim(&unattended_partly(runtimes)));
                }
                _ => eprintln!("{}", ui::dim(UNATTENDED_LINE)),
            }
            save(&Record {
                noticed_at: Some(now),
                ..record
            });
        }
        Action::Ask => ask(&presence, record, now),
    }
}

fn ask(presence: &Presence, record: Record, now: DateTime<Utc>) {
    match presence {
        Presence::Partly(runtimes) => eprintln!(
            "agit is set up on this computer, but not for {}, so their conversations are not captured.",
            labels(runtimes)
        ),
        _ => eprintln!(
            "agit is installed but not set up on this computer: no runtime has agit's Skill or capture hooks, so no conversation is captured."
        ),
    }
    eprintln!(
        "Setting it up writes, for the runtimes installed on this computer (`agit setup --all` writes for every runtime agit supports):"
    );
    for line in preview() {
        eprintln!("  {line}");
    }
    match ui::prompt::confirm("Set it up now?", false) {
        Ok(Some(true)) => {
            if let Err(error) = super::run(super::Args::everything()) {
                ui::warning(&format!("agit setup did not finish: {error:#}"));
            }
        }
        // No, or no answer: the person has seen the question once.
        _ => save(&Record {
            declined_at: Some(now),
            ..record
        }),
    }
}

/// One line per runtime a bare `agit setup` writes into, which are the runtimes installed here:
/// the folder, and what goes there.
fn preview() -> Vec<String> {
    installed()
        .into_iter()
        .map(|id| {
            let label = crate::adapter::runtime_label(id);
            let place = super::skill_path(id)
                .and_then(|skill| skill.parent().and_then(Path::parent).map(ui::tilde))
                .unwrap_or_default();
            let installed = crate::adapter::get(id).is_ok_and(|adapter| adapter.available());
            format!("{label} ({place}): {}", integrations(id, installed))
        })
        .collect()
}

/// What a bare `agit setup` writes for a runtime, as its `install_skill`, `install_hooks` and
/// `register_mcp` decide it: the Skill for every runtime, and hooks and MCP where each of them
/// installs them. The preview lists exactly the runtimes "y" writes for, so it neither hides a
/// file "y" writes nor names a tool the person does not have.
fn integrations(runtime: &str, installed: bool) -> &'static str {
    match runtime {
        "claude-code" | "workbuddy" => "Skill, hooks, MCP",
        "codex" => "Skill, MCP, and hooks if this Codex supports them",
        "cursor" | "opencode" => "Skill, MCP",
        "hermes" if installed => "Skill, hooks, MCP",
        "openclaw" if installed => "Skill, hooks",
        _ => "Skill",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a person who answers a terminal is ever asked, and nothing but an ask can lead to
    /// setup: an implementation that asks, or sets up, from an agent's or a script's run fails
    /// the unattended rows. A decline, `AGIT_SKIP_SETUP` or `AGIT_QUIET`, an agit that is set
    /// up, and a computer with no runtime all keep it silent; an unattended notice repeats only
    /// after a quiet day, and a stamp from the future does not mute it.
    #[test]
    fn only_a_person_at_a_terminal_is_asked_and_others_hear_once_a_day() {
        let now = Utc::now();
        let missing = Presence::NotSetUp(vec!["claude-code"]);
        let fresh = Record::default();
        let declined = Record {
            declined_at: Some(now),
            ..Record::default()
        };
        let noticed = |hours| Record {
            noticed_at: Some(now - chrono::Duration::hours(hours)),
            ..Record::default()
        };
        let cases = [
            (
                "person, not set up",
                &missing,
                fresh.clone(),
                true,
                false,
                Action::Ask,
            ),
            (
                "agent or script",
                &missing,
                fresh.clone(),
                false,
                false,
                Action::Say,
            ),
            (
                "unattended again soon",
                &missing,
                noticed(1),
                false,
                false,
                Action::Nothing,
            ),
            (
                "unattended next day",
                &missing,
                noticed(25),
                false,
                false,
                Action::Say,
            ),
            (
                "stamp from the future",
                &missing,
                noticed(-2),
                false,
                false,
                Action::Say,
            ),
            (
                "person after a decline",
                &missing,
                declined.clone(),
                true,
                false,
                Action::Nothing,
            ),
            (
                "agent after a decline",
                &missing,
                declined,
                false,
                false,
                Action::Nothing,
            ),
            (
                "silenced person",
                &missing,
                fresh.clone(),
                true,
                true,
                Action::Nothing,
            ),
            (
                "silenced agent",
                &missing,
                fresh.clone(),
                false,
                true,
                Action::Nothing,
            ),
            (
                "set up",
                &Presence::SetUp,
                fresh.clone(),
                true,
                false,
                Action::Nothing,
            ),
            (
                "no runtime",
                &Presence::NoRuntime,
                fresh,
                true,
                false,
                Action::Nothing,
            ),
        ];
        for (name, presence, record, can_ask, silenced, expected) in cases {
            assert_eq!(
                decide(presence, &record, can_ask, silenced, now),
                expected,
                "{name}"
            );
        }
    }
}
