//! `agit setup` — install every integration in one pass. Idempotent, and installs everything by
//! default except the project `AGENTS.md` section, which only `--agents-md` writes; without it
//! everything still works, at the manual pace.
//!
//! * `--hooks`: settle automatically at the end of a turn (silent; a failure never blocks) +
//!   `AGIT_SESSION` injection + registering a newly opened unadopted session inside a bound
//!   workspace as pending adoption. Discovery answers only "which ones exist". Claude Code,
//!   hook-capable Codex and WorkBuddy receive `agit hooks` commands in their hook settings, one
//!   entry per hook whatever path agit runs from; Hermes and OpenClaw receive their native
//!   lifecycle integration. Other runtimes keep using the instruction-injected `agit commit`
//!   discipline.
//! * `--skill`: install one entrypoint plus command references read on demand into each runtime's
//!   native Skill directory:
//!   - claude-code → `~/.claude/skills/agit/`
//!   - codex → `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`)
//!   - opencode → `~/.config/opencode/skills/agit/`
//!   - cursor → `~/.cursor/skills/agit/`
//!   - hermes → `$HERMES_HOME/skills/agit/` (default `~/.hermes/skills/agit/`)
//!   - openclaw → `$OPENCLAW_STATE_DIR/skills/agit/` (default `~/.openclaw/skills/agit/`)
//!   - workbuddy → `skills/agit/` under its config directory (default `~/.workbuddy-ai/`)
//!     Every directory holds `SKILL.md`, `VERSION` and `references/commands/`; `--skill` does not
//!     expand the full manual into `AGENTS.md`. Use `--agents-md` for the short adoption rule.
//!     Installing it also removes an agit section from the home-level instruction files every
//!     conversation reads (`~/AGENTS.md`, `~/CLAUDE.md`, the Codex and OpenCode ones), where a
//!     setup run in the home folder once put it: there it reached conversations that have nothing
//!     to do with AgentGit.
//! * `--mcp`: register `agit mcp` (stdio MCP server: search/show/view/status/commit) into each
//!   runtime's native MCP configuration:
//!   - claude-code → `claude mcp add --scope user`, so every folder sees it; local copies the user
//!     entry fully replaces are removed (prints guidance instead of failing when it cannot install)
//!   - codex → `[mcp_servers.agit]` in `~/.codex/config.toml`
//!   - opencode → `mcp.agit` in `~/.config/opencode/opencode.json`
//!   - cursor → `mcpServers.agit` in `~/.cursor/mcp.json`
//!   - workbuddy → `mcpServers.agit` in its MCP file under the config directory
//!   - hermes → its native MCP configuration; OpenClaw needs none beside its lifecycle plugin
//! * `--agents-md`: append a marked, idempotent section to the `AGENTS.md` at the top of the current
//!   Git repository, and say where and what it is for. Outside a repository, or in the home
//!   folder, it writes nothing.
//! * `--completions <shell>`: shell completions.

use super::CmdResult;
use super::skill_bundle;
use crate::{ExitCode, ui};
pub(crate) mod claude_mcp;
mod legacy;
mod native;

use clap::Args as ClapArgs;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

#[derive(ClapArgs)]
pub struct Args {
    /// Install for one runtime only (default: every runtime noticed).
    #[arg(long, value_name = "runtime")]
    pub runtime: Option<String>,
    #[arg(long)]
    pub hooks: bool,
    #[arg(long)]
    pub skill: bool,
    /// Refresh only what is already installed: existing native or legacy Skill installations
    /// with `--skill`; with `--hooks`, repair the existing hooks that name this agit in a form a
    /// shell does not run, and install none.
    #[arg(long, conflicts_with_all = ["mcp", "agents_md", "completions", "auto_push"])]
    pub installed_only: bool,
    #[arg(long)]
    pub mcp: bool,
    #[arg(long = "agents-md")]
    pub agents_md: bool,
    #[arg(long, value_name = "shell", value_parser = ["bash", "zsh", "fish"])]
    pub completions: Option<String>,
    /// Set the user default for automatically publishing settled turns.
    #[arg(long, num_args = 0..=1, default_missing_value = "true", require_equals = true)]
    pub auto_push: Option<bool>,
}

/// The observable result of one setup run.
///
/// `items` counts what was written and what was already current; `failures` counts what did not
/// persist. With any failure the command must not report "All set" and must not return success —
/// that matters more than reporting how much was written, because a partial write must leave the
/// legacy Skill in place.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SetupReport {
    items: usize,
    failures: usize,
    changed: usize,
}

impl SetupReport {
    fn item() -> Self {
        Self {
            items: 1,
            failures: 0,
            changed: 0,
        }
    }

    fn changed_item() -> Self {
        Self {
            changed: 1,
            ..Self::item()
        }
    }

    fn failure() -> Self {
        Self {
            items: 0,
            failures: 1,
            changed: 0,
        }
    }

    fn merge(&mut self, other: Self) {
        self.items += other.items;
        self.failures += other.failures;
        self.changed += other.changed;
    }

    fn succeeded(self) -> bool {
        self.failures == 0
    }
}

/// Runtime filter: None / all installs everything; a match returns true. An unknown value is
/// rejected up in `run`, so whatever reaches here is valid — this decides a bool only, and the
/// error happens once, at the entry point.
fn wants(filter: Option<&str>, runtime: &str) -> bool {
    matches!(filter, None | Some("all")) || filter == Some(runtime)
}

pub fn run(args: Args) -> CmdResult {
    // A hook repair answers `--json` with one value saying what it repaired, so its notices stay
    // off stdout.
    let data = args.completions.is_some() || (repairs_hooks(&args) && super::json::requested());
    ui::with_info_on_stderr(data, || run_setup(args))
}

fn repairs_hooks(args: &Args) -> bool {
    args.hooks && args.installed_only
}

fn run_setup(mut args: Args) -> CmdResult {
    if let Some(runtime) = args.runtime.as_deref()
        && let Ok(runtime) = crate::adapter::normalize(runtime)
    {
        args.runtime = Some(runtime.into());
    }
    // Reject an unknown runtime on the spot: silently installing nothing is the most expensive
    // class of configuration bug — the user believes the integration is in place and finds out
    // only at the next session.
    if let Some(rt) = args.runtime.as_deref()
        && rt != "all"
        && !crate::adapter::setup_runtimes().any(|id| id == rt)
    {
        ui::error(&format!(
            "unknown runtime `{rt}` (expected one of: {})",
            std::iter::once("all")
                .chain(crate::adapter::setup_runtimes())
                .collect::<Vec<_>>()
                .join(" / ")
        ));
        return Ok(ExitCode::Usage);
    }
    if args.installed_only && !args.skill && !args.hooks {
        ui::error("`--installed-only` refreshes `--skill` or `--hooks`; name one of them");
        return Ok(ExitCode::Usage);
    }

    if args.completions.is_none() && !args.installed_only {
        if crate::telemetry::state::onboarding().is_err() {
            eprintln!("Usage statistics remain off because their settings could not be saved.");
        }
        crate::telemetry::activate(true);
    }

    // No flag = install everything (except completions — that one takes a shell argument).
    let all =
        !args.hooks && !args.skill && !args.mcp && !args.agents_md && args.completions.is_none();
    // The AGENTS.md section is a project's choice, so only `--agents-md` writes it: run from
    // wherever setup happens to start, it would land in an unrelated project or the home folder.
    let (hooks, skill, mcp, am) = (
        args.hooks || all,
        args.skill || all,
        args.mcp || all,
        args.agents_md,
    );

    if let Some(value) = args.auto_push {
        super::config::apply("push.auto", Some(if value { "true" } else { "false" }))?;
    } else if all
        && !super::json::requested()
        && ui::prompt::can_ask()
        && !crate::telemetry::state::positive("AGIT_YES")
    {
        let current = crate::infra::config::auto_push_default()?;
        if let Some(value) = ui::prompt::confirm(
            "Automatically push settled turns by default? Each repository can override this",
            current,
        )? {
            super::config::apply("push.auto", Some(if value { "true" } else { "false" }))?;
        }
    }

    let rt = args.runtime.as_deref();
    let mut report = SetupReport::default();
    if repairs_hooks(&args) {
        let repaired = repair_hooks(rt, &exe_str());
        report.merge(repaired);
        if super::json::requested() {
            println!(
                "{}",
                serde_json::json!({"hook_files_repaired": repaired.changed})
            );
        }
    } else if hooks {
        report.merge(install_hooks(rt));
    }
    if skill {
        report.merge(install_skill(rt, args.installed_only));
        report.merge(remove_home_sections());
    }
    if mcp {
        report.merge(register_mcp(rt));
    }
    if am {
        report.merge(append_agents_md());
    }
    if let Some(sh) = &args.completions {
        report.merge(print_completions(sh));
    }

    if report.failures > 0 {
        ui::error(&format!(
            "Setup incomplete: {} item(s) failed; run `agit doctor` to inspect the installation.",
            report.failures
        ));
        return Ok(ExitCode::Precondition);
    }
    if report.items > 0 {
        ui::success(&format!(
            "All set ({} items). Idempotent: re-running writes nothing twice.",
            report.items
        ));
    } else {
        ui::info(format_args!("Nothing to install."));
    }
    Ok(ExitCode::Ok)
}

// ── hooks ─────────────────────────────────────────────────────────────

/// The hooks installed into settings.json: event → agit's argv (exe excluded).
///
/// argv rather than one whole command string, so a test can push the same data through clap. A
/// command agit cannot parse makes every SessionStart fail with exit code 2, and the harness
/// swallows that with no symptom at all. Nothing else proves that "the command written out parses
/// on its own", which is why this table exists.
const HOOKS: &[(&str, &[&str])] = &[
    ("SessionStart", &["hooks", "ingest"]),
    ("Stop", &["hooks", "settle"]),
];

/// Hooks installed for Claude Code alone, each with its timeout in seconds. A PermissionRequest
/// hook waits for an answer a person gives in shared control while the terminal's dialog stays
/// up beside it, and Claude Code ends the hook as soon as the terminal answers; its timeout only
/// bounds how long a remote answer may still arrive. Claude Code passes user plugins over on its
/// classic events where an organization's security default is seated, so this ask reaches agit
/// only through settings.
#[cfg(unix)]
const CLAUDE_HOOKS: &[(&str, &[&str], u64)] =
    &[("PermissionRequest", &["hooks", "native-permission"], 86_400)];
#[cfg(not(unix))]
const CLAUDE_HOOKS: &[(&str, &[&str], u64)] = &[];

/// Hooks agit no longer installs and must retire. **Matched by argv, without the exe path** —
/// once the install location moves (npm's `/usr/local/bin` ↔ `setup.sh`'s `~/.local/bin`),
/// matching on the whole command string retires nothing, and one Stop settles twice.
///
/// `commands/hooks.rs` carries the reason `hooks settle` replaces `commit --from-hook`: the latter
/// does not read stdin and can only locate the branch through a possibly stale `AGIT_SESSION`.
const RETIRED_HOOKS: &[(&str, &str)] = &[("Stop", "commit --from-hook")];

/// The command string of one hook. Every runtime that takes a hook as one string (Claude Code,
/// Codex, WorkBuddy) hands it to a shell, so the executable is written as one shell word.
fn hook_command(exe: &str, argv: &[&str], runtime: Option<&str>) -> String {
    format!("{} {}", shell_word(exe), hook_arguments(argv, runtime))
}

/// `exe` as one word of a shell command line. Left bare, a path with a space
/// (`~/Library/Application Support/…`, a Windows user folder with a space) is split there: the
/// shell looks for a program named by the part before the space, and the hook fails on every
/// event without anything showing the user why.
fn shell_word(exe: &str) -> Cow<'_, str> {
    shell_word_for(exe, cfg!(windows))
}

/// On Windows a backslash separates directories and the hook shell may be `cmd`, which knows
/// only double quotes; a Windows path cannot contain one. Elsewhere POSIX single quotes keep
/// every character, with `'\''` for a quote inside.
fn shell_word_for(exe: &str, windows: bool) -> Cow<'_, str> {
    let plain =
        |c: char| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c) || (windows && c == '\\');
    if !exe.is_empty() && exe.chars().all(plain) {
        Cow::Borrowed(exe)
    } else if windows {
        Cow::Owned(format!("\"{exe}\""))
    } else {
        Cow::Owned(format!("'{}'", exe.replace('\'', "'\\''")))
    }
}

/// The executable a hook command runs before `arguments`, read back from the forms
/// [`shell_word`] writes: one bare word, one double-quoted word, or one single-quoted word.
/// `None` for anything else, such as a chain, a wrapper or a path whose space splits it, which
/// is not one program a shell would run. A double-quoted word counts only when the shell reads
/// it as written: a `$` or a backtick expands inside double quotes, so a hook that chooses its
/// path at run time is the user's, and claiming it would replace their choice with ours.
fn hook_program<'a>(command: &'a str, arguments: &str) -> Option<Cow<'a, str>> {
    let rest = command.trim().strip_suffix(arguments)?;
    let word = rest.trim_end();
    // A real separator must sit between the executable and the arguments; otherwise a matching
    // tail like `.../notagit-commit --from-hook` counts too.
    if word.is_empty() || word.len() == rest.len() {
        return None;
    }
    if let Some(inner) = word.strip_prefix('"') {
        let inner = inner.strip_suffix('"')?;
        // A backslash before an ordinary character stays literal in double quotes, which keeps a
        // Windows path as written; before another backslash the shell drops one of the two.
        let literal = !inner.contains(['"', '$', '`']) && !inner.contains("\\\\");
        return literal.then_some(Cow::Borrowed(inner));
    }
    if word.starts_with('\'') {
        let mut value = String::new();
        let mut rest = word;
        loop {
            let body = rest.strip_prefix('\'')?;
            let end = body.find('\'')?;
            value.push_str(&body[..end]);
            rest = &body[end + 1..];
            if rest.is_empty() {
                return Some(Cow::Owned(value));
            }
            rest = rest.strip_prefix("\\'")?;
            value.push('\'');
            if rest.is_empty() {
                return Some(Cow::Owned(value));
            }
        }
    }
    (!word.contains(|c: char| c.is_whitespace() || c == '"' || c == '\''))
        .then_some(Cow::Borrowed(word))
}

/// What identifies one agit hook regardless of where the executable lives: the argument string
/// after the executable.
fn hook_arguments(argv: &[&str], runtime: Option<&str>) -> String {
    let mut arguments = argv.join(" ");
    if let Some(runtime) = runtime {
        arguments.push_str(&format!(" --runtime {runtime}"));
    }
    arguments
}

/// Runtime hook configuration. Each runtime-owned file is merged idempotently, so one hook is
/// never written twice, including when agit now runs from another executable path.
fn install_hooks(runtime: Option<&str>) -> SetupReport {
    let mut report = SetupReport::default();
    let exe = exe_str();

    if wants(runtime, "claude-code") {
        match claude_settings() {
            Some(path) => report.merge(install_hook_file(
                &path,
                "Claude Code",
                &exe,
                None,
                CLAUDE_HOOKS,
            )),
            None => {
                ui::warning("cannot install Claude Code hooks: HOME is not set");
                report.merge(SetupReport::failure());
            }
        }
        #[cfg(unix)]
        report.merge(native::claude());
    }

    if wants(runtime, "codex") {
        let explicit = matches!(runtime, Some("codex" | "all"));
        match codex_hooks_capability() {
            Some(capability) => match codex_home_path().map(|h| h.join("hooks.json")) {
                Some(path) => {
                    let installed = install_hook_file(&path, "Codex", &exe, Some("codex"), &[]);
                    report.merge(installed);
                    if installed.succeeded() {
                        if capability.enabled {
                            ui::info(format_args!(
                                "  {} Codex may ask you to trust these hooks when their command changes",
                                ui::dim("·")
                            ));
                        } else {
                            ui::warning(
                                "Codex hooks are configured but its `hooks` feature is disabled",
                            );
                        }
                    }
                }
                None => {
                    ui::warning("cannot install Codex hooks: HOME and CODEX_HOME are not set");
                    report.merge(SetupReport::failure());
                }
            },
            None if explicit => {
                ui::warning("cannot install Codex hooks: this Codex does not report hook support");
                report.merge(SetupReport::failure());
            }
            None => ui::info(format_args!(
                "  {} Codex hooks unavailable; skipping",
                ui::dim("·")
            )),
        }
    }

    if wants(runtime, "workbuddy") {
        match crate::adapter::workbuddy::home() {
            Ok(root) => report.merge(install_hook_file(
                &root.join("settings.json"),
                "WorkBuddy",
                &exe,
                Some("workbuddy"),
                &[],
            )),
            Err(_) => report.merge(SetupReport::failure()),
        }
    }
    for id in ["hermes", "openclaw"] {
        if wants(runtime, id)
            && (runtime == Some(id)
                || crate::adapter::get(id).is_ok_and(|adapter| adapter.available()))
        {
            report.merge(if id == "hermes" {
                native::hermes(&exe, "hooks")
            } else {
                native::openclaw(&exe)
            });
        }
    }

    report
}

fn claude_settings() -> Option<PathBuf> {
    crate::adapter::claude_code::projects_dir()
        .ok()
        .and_then(|projects| {
            projects
                .parent()
                .map(|profile| profile.join("settings.json"))
        })
}

/// `setup --hooks --installed-only`: repair, in each runtime's hook file, the agit hooks that name
/// `exe` in a form a shell does not run as that program, such as a path with a space written
/// bare. It runs unattended (the menu bar app runs it once per new agit), so it installs nothing,
/// touches no native integration, and leaves alone every hook that runs another program, even
/// another agit: pointing those at this one would replace a choice the person made.
fn repair_hooks(runtime: Option<&str>, exe: &str) -> SetupReport {
    let files = [
        ("claude-code", "Claude Code", claude_settings(), None),
        (
            "codex",
            "Codex",
            codex_home_path().map(|home| home.join("hooks.json")),
            Some("codex"),
        ),
        (
            "workbuddy",
            "WorkBuddy",
            crate::adapter::workbuddy::home()
                .ok()
                .map(|root| root.join("settings.json")),
            Some("workbuddy"),
        ),
    ];
    let mut report = SetupReport::default();
    for (id, label, path, runtime_flag) in files {
        if wants(runtime, id)
            && let Some(path) = path
        {
            report.merge(repair_hook_file(&path, label, exe, runtime_flag));
        }
    }
    report
}

/// Repair one hook file in its own text: only the command strings of agit's hooks for `exe`
/// change, and every other byte stays as it was, so nothing the person or another tool wrote is
/// added, reordered, reformatted or removed. Parsing and writing the document back would sort its
/// keys and reflow it. The file as it was before the first repair is kept beside it.
fn repair_hook_file(
    path: &Path,
    label: &str,
    exe: &str,
    runtime_flag: Option<&str>,
) -> SetupReport {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SetupReport::default();
        }
        Err(_) => {
            ui::warning(&format!(
                "cannot repair {label} hooks: failed to read {}",
                path.display()
            ));
            return SetupReport::failure();
        }
    };
    let Ok(mut expected) = serde_json::from_str::<serde_json::Value>(&text) else {
        ui::warning(&format!(
            "cannot repair {label} hooks: {} is not valid JSON",
            path.display()
        ));
        return SetupReport::failure();
    };
    let mut present = false;
    let mut rewrites: Vec<(String, String)> = Vec::new();
    for (event, argv) in HOOKS {
        let command = hook_command(exe, argv, runtime_flag);
        let arguments = hook_arguments(argv, runtime_flag);
        let bare = format!("{exe} {arguments}");
        let entries = expected
            .get_mut("hooks")
            .and_then(|hooks| hooks.get_mut(event))
            .and_then(|groups| groups.as_array_mut())
            .into_iter()
            .flatten()
            .filter_map(|group| group.get_mut("hooks").and_then(|list| list.as_array_mut()))
            .flatten()
            .filter(|entry| entry.get("type").and_then(|value| value.as_str()) == Some("command"));
        for entry in entries {
            let Some(current) = entry.get("command").and_then(|value| value.as_str()) else {
                continue;
            };
            let names_exe = current.trim() == bare
                || hook_program(current, &arguments).is_some_and(|program| program == exe);
            if !names_exe {
                continue;
            }
            present = true;
            if current != command {
                rewrites.push((current.to_owned(), command.clone()));
                entry["command"] = serde_json::Value::String(command.clone());
            }
        }
    }
    if rewrites.is_empty() {
        return if present {
            SetupReport::item()
        } else {
            SetupReport::default()
        };
    }
    let repaired = rewrite_command_values(&text, &rewrites);
    // The same text as the value of another `command` key is rewritten too; the document would
    // then differ from the one only agit's hooks changed in, and nothing is written.
    if serde_json::from_str::<serde_json::Value>(&repaired)
        .ok()
        .as_ref()
        != Some(&expected)
    {
        ui::warning(&format!(
            "cannot repair {label} hooks: {} uses the same command outside agit's hooks",
            path.display()
        ));
        return SetupReport::failure();
    }
    if keep_original(path, &text).is_err() {
        ui::warning(&format!(
            "cannot repair {label} hooks: failed to keep a copy of {}",
            path.display()
        ));
        return SetupReport::failure();
    }
    if std::fs::write(path, repaired).is_err() {
        ui::warning(&format!("failed to write {}", path.display()));
        return SetupReport::failure();
    }
    ui::info(format_args!(
        "  {} {label} hooks repaired → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(path)
    ));
    SetupReport::changed_item()
}

/// Keeps `text`, the hook file at `path` as read, beside it with `.agit-backup` added to the name,
/// before agit rewrites it. An existing copy is never replaced, so the copy is the file as it was
/// before agit first changed it and may predate later edits. Without the copy, a rewrite the person
/// did not expect has nothing to compare against.
fn keep_original(path: &Path, text: &str) -> std::io::Result<()> {
    let mut backup = path.as_os_str().to_owned();
    backup.push(".agit-backup");
    let mut options = crate::infra::config::state_file_options();
    options.write(true).create_new(true);
    // The copy is for its owner only, and never more readable than the original: a hook file can
    // hold credentials in its environment block, and group bits copied onto a file whose group
    // differs from the original's would hand it to another group.
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(path) {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
        options.mode(metadata.permissions().mode() & 0o600);
    }
    match options
        .open(&backup)
        .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
    {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        kept => kept,
    }
}

/// `text` with each string that is the value of a `command` key and equals the left side of a
/// rewrite replaced by its right side; every other byte is copied as is. `text` must be valid
/// JSON, so outside strings it holds no quote, and the text between a key and its value is a
/// colon and whitespace.
fn rewrite_command_values(text: &str, rewrites: &[(String, String)]) -> String {
    let bytes = text.as_bytes();
    let mut strings = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'"' {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() && bytes[index] != b'"' {
            index += if bytes[index] == b'\\' { 2 } else { 1 };
        }
        index = (index + 1).min(bytes.len());
        strings.push(start..index);
    }
    let decode =
        |range: &std::ops::Range<usize>| serde_json::from_str::<String>(&text[range.clone()]).ok();
    let mut repaired = String::with_capacity(text.len());
    let mut copied = 0;
    for pair in strings.windows(2) {
        let (key, value) = (&pair[0], &pair[1]);
        if text[key.end..value.start].trim() != ":" || decode(key).as_deref() != Some("command") {
            continue;
        }
        let Some(current) = decode(value) else {
            continue;
        };
        let Some((_, replacement)) = rewrites.iter().find(|(old, _)| *old == current) else {
            continue;
        };
        repaired.push_str(&text[copied..value.start]);
        repaired.push_str(&serde_json::Value::String(replacement.clone()).to_string());
        copied = value.end;
    }
    repaired.push_str(&text[copied..]);
    repaired
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CodexHooksCapability {
    enabled: bool,
}

/// Ask the installed Codex whether it knows the hooks feature. Presence is the compatibility
/// boundary; the version string is not, because downstream builds may add or remove capabilities
/// without sharing the release numbering of the public binary.
fn codex_hooks_capability() -> Option<CodexHooksCapability> {
    let exe = crate::adapter::which("codex")?;
    let output = std::process::Command::new(exe)
        .args(["features", "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_codex_hooks_capability(&String::from_utf8_lossy(&output.stdout))
}

/// The runtimes whose hooks can capture project sessions automatically. Codex counts only when
/// it reports an enabled hooks feature; a Codex that cannot does not take Claude Code capture
/// down with it, and its sessions remain available to manual project sync.
pub(super) fn project_hook_runtimes() -> crate::Result<Vec<&'static str>> {
    let mut runtimes = Vec::new();
    if crate::adapter::get("claude-code")?.available() {
        runtimes.push("claude-code");
    }
    if crate::adapter::get("codex")?.available() {
        if codex_hooks_capability().is_some_and(|c| c.enabled) {
            runtimes.push("codex");
        } else {
            ui::warning(
                "Codex does not report an enabled hooks feature; its sessions in this project are published by `agit project sync` instead of automatically",
            );
        }
    }
    anyhow::ensure!(
        !runtimes.is_empty(),
        "install Claude Code, or Codex with its hooks feature enabled, before enabling automatic project capture; use manual project sync otherwise"
    );
    Ok(runtimes)
}

fn parse_codex_hooks_capability(output: &str) -> Option<CodexHooksCapability> {
    output.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        if fields.next()? != "hooks" {
            return None;
        }
        let enabled = fields.last()?.parse().ok()?;
        Some(CodexHooksCapability { enabled })
    })
}

/// Add agit's hooks to one runtime-owned JSON file without replacing malformed or non-object
/// user configuration. `runtime_flag` makes the payload parser independent of transcript paths.
/// The file is written back whole, so its keys come back sorted; the file as it was before agit
/// first changed it is kept beside it, and a rewrite that cannot keep it does not happen.
fn install_hook_file(
    path: &Path,
    label: &str,
    exe: &str,
    runtime_flag: Option<&str>,
    extra: &[(&str, &[&str], u64)],
) -> SetupReport {
    let original = match std::fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            ui::warning(&format!(
                "cannot install {label} hooks: failed to read {}",
                path.display()
            ));
            return SetupReport::failure();
        }
    };
    let mut doc = match original
        .as_deref()
        .map(serde_json::from_str::<serde_json::Value>)
    {
        None => serde_json::json!({}),
        Some(Ok(doc)) => doc,
        Some(Err(_)) => {
            ui::warning(&format!(
                "cannot install {label} hooks: {} is not valid JSON",
                path.display()
            ));
            return SetupReport::failure();
        }
    };
    let Some(root) = doc.as_object_mut() else {
        ui::warning(&format!(
            "cannot install {label} hooks: {} must contain a JSON object",
            path.display()
        ));
        return SetupReport::failure();
    };
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({}));
    if !hooks.is_object() {
        ui::warning(&format!(
            "cannot install {label} hooks: `hooks` in {} must be an object",
            path.display()
        ));
        return SetupReport::failure();
    }

    for event in HOOKS
        .iter()
        .map(|(event, _)| *event)
        .chain(RETIRED_HOOKS.iter().map(|(event, _)| *event))
        .chain(extra.iter().map(|(event, _, _)| *event))
    {
        if hooks.get(event).is_some_and(|value| !value.is_array()) {
            ui::warning(&format!(
                "cannot install {label} hooks: `{event}` in {} must be an array",
                path.display()
            ));
            return SetupReport::failure();
        }
    }

    let mut added = 0;
    for (event, argv) in RETIRED_HOOKS {
        added += retire_hook(hooks, event, argv);
    }
    for (event, argv) in HOOKS {
        added += upsert_hook(
            hooks,
            event,
            &hook_command(exe, argv, runtime_flag),
            &hook_arguments(argv, runtime_flag),
            None,
        );
    }
    for (event, argv, timeout) in extra {
        added += upsert_hook(
            hooks,
            event,
            &hook_command(exe, argv, runtime_flag),
            &hook_arguments(argv, runtime_flag),
            Some(*timeout),
        );
    }
    if added > 0 {
        if let Some(text) = &original
            && keep_original(path, text).is_err()
        {
            ui::warning(&format!(
                "cannot install {label} hooks: failed to keep a copy of {}",
                path.display()
            ));
            return SetupReport::failure();
        }
        if write_json(path, &doc).is_err() {
            ui::warning(&format!("failed to write {}", path.display()));
            return SetupReport::failure();
        }
        ui::info(format_args!(
            "  {} {label} hooks → {}",
            ui::ok(ui::theme::symbols().check),
            ui::tilde(path)
        ));
    } else {
        ui::info(format_args!(
            "  {} {label} hooks already in place",
            ui::dim("·")
        ));
    }
    SetupReport::item()
}

/// Retire a hook that is no longer used. Returns how many commands were removed.
///
/// Without this step, a machine that still carries the old command runs both on Stop: the old one
/// locates the branch through `AGIT_SESSION`, the new one through the payload, and after a session
/// switch the two point at different branches — one conversation settles into two histories,
/// which is harder to clean up than not settling at all.
///
/// # What is removed is the command, not the whole group
///
/// Under one event `settings.json` holds several **group**s, each with its own matcher and a
/// `hooks` array. The group agit writes holds a single command, so "delete the whole group" looks
/// right on the install-and-retire-your-own path — but as soon as the user folds agit's command
/// into their own group (or another tool merges `settings.json`), deleting the group takes **the
/// user's hook with it**, and does so silently.
///
/// So this removes only the matching entry from the `hooks` array; a group goes away with it only
/// once it has been emptied, leaving no idle shell behind in the file.
///
/// Matching is by **argv substring**, without the exe path: once the install location moves (npm's
/// `/usr/local/bin` ↔ `setup.sh`'s `~/.local/bin`), matching on the whole command string retires
/// nothing, and one Stop settles twice.
/// Whether this command is the one hook **we wrote ourselves**.
///
/// Retiring is a destructive write, so the test is narrow enough to admit only the shape
/// [`hook_command`] builds: an executable named `agit` followed by exactly this argument string.
///
/// Under a substring test, the user's own wrapper, our command spliced into a shell chain, even
/// another tool merely passing this text as an argument (`echo "commit --from-hook"`), all get
/// deleted **whole**. And a hook can only be deleted whole — there is no way to lift out just the
/// middle of it, so anything not recognized must be left alone.
///
/// When in doubt, keep it: a stale hook stays visible and the user can deal with it at the next
/// `agit setup`; deleting someone else's configuration is one of the hardest kinds of damage agit
/// can do on this machine to track down.
fn is_our_hook(command: &str, argv: &str) -> bool {
    // It must be a single word. In `some earlier command && /bin/agit commit --from-hook` ours
    // is one link of the chain, and deleting the entry takes the user's first half with it.
    let Some(exe) = hook_program(command, argv) else {
        return false;
    };
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(&exe);
    matches!(name, "agit" | "agit.exe")
}

fn retire_hook(hooks: &mut serde_json::Value, event: &str, argv: &str) -> usize {
    let Some(arr) = hooks
        .as_object_mut()
        .expect("hooks must be an object")
        .get_mut(event)
        .and_then(|v| v.as_array_mut())
    else {
        return 0;
    };
    let mut removed = 0;
    for group in arr.iter_mut() {
        let Some(list) = group.get_mut("hooks").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        let before = list.len();
        list.retain(|h| {
            !h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| is_our_hook(c, argv))
        });
        removed += before - list.len();
    }
    // Drop only the groups that were emptied. Every other group stays unchanged, including the
    // ones nothing was removed from.
    arr.retain(|group| {
        group
            .get("hooks")
            .and_then(|v| v.as_array())
            .map(|l| !l.is_empty())
            .unwrap_or(true)
    });
    removed
}

/// Install one agit hook under an event, keeping exactly one entry for it. Returns how many
/// entries were added, rewritten or removed.
///
/// An entry is agit's own when it is this exact command, or when [`is_our_hook`] recognizes it
/// as an `agit` executable followed by `arguments`. Installs from different executable paths
/// (npm's global bin, `setup.sh`'s `~/.local/bin`, a package-runner cache) therefore land on the
/// same entry: the first one found is pointed at the current executable and the others are
/// removed. Keeping them would run the same hook once per stale path, concurrently, on every
/// event. Other entries, including ones that merely mention the arguments, are left untouched.
fn upsert_hook(
    hooks: &mut serde_json::Value,
    event: &str,
    cmd: &str,
    arguments: &str,
    timeout: Option<u64>,
) -> usize {
    let arr = hooks
        .as_object_mut()
        .expect("hooks must be an object")
        .entry(event)
        .or_insert_with(|| serde_json::json!([]));
    let arr = arr.as_array_mut().expect("hook event must be an array");
    // The same executable joined bare: what this command was written as before its executable
    // was quoted. Its space split it, so it never ran; it is rewritten, not kept beside the new
    // one. Matching its exact text claims no entry another tool wrote.
    let unquoted = hook_program(cmd, arguments)
        .map(|exe| format!("{exe} {arguments}"))
        .filter(|unquoted| unquoted != cmd);
    let ours = |entry: &serde_json::Value| {
        entry.get("type").and_then(|value| value.as_str()) == Some("command")
            && entry
                .get("command")
                .and_then(|value| value.as_str())
                .is_some_and(|command| {
                    command == cmd
                        || is_our_hook(command, arguments)
                        || unquoted.as_deref() == Some(command.trim())
                })
    };
    let exact = |entry: &serde_json::Value| {
        entry
            .get("command")
            .and_then(|value| value.as_str())
            .is_some_and(|command| {
                command == cmd
                    || hook_program(command, arguments).is_some_and(|exe| runs_this_agit(&exe))
            })
    };
    // Prefer an entry that already carries the current command, so a settled file stays as is.
    let keep = arr
        .iter()
        .enumerate()
        .flat_map(|(group, value)| {
            value
                .get("hooks")
                .and_then(|entries| entries.as_array())
                .into_iter()
                .flatten()
                .enumerate()
                .filter(|(_, entry)| ours(entry))
                .map(move |(index, entry)| (group, index, exact(entry)))
        })
        .min_by_key(|(_, _, exact)| !exact)
        .map(|(group, index, _)| (group, index));
    let Some((keep_group, keep_index)) = keep else {
        let mut entry = serde_json::json!({"type": "command", "command": cmd});
        if let Some(timeout) = timeout {
            entry["timeout"] = timeout.into();
        }
        arr.push(serde_json::json!({ "hooks": [entry] }));
        return 1;
    };
    let mut changed = 0;
    let mut emptied = Vec::new();
    for (group_index, group) in arr.iter_mut().enumerate() {
        let Some(entries) = group
            .get_mut("hooks")
            .and_then(|value| value.as_array_mut())
        else {
            continue;
        };
        let before = entries.len();
        let mut index = 0;
        entries.retain(|entry| {
            let kept = (group_index, index) == (keep_group, keep_index) || !ours(entry);
            index += 1;
            kept
        });
        changed += before - entries.len();
        if before > 0 && entries.is_empty() {
            emptied.push(group_index);
        }
    }
    let kept = arr[keep_group]["hooks"]
        .as_array_mut()
        .and_then(|entries| entries.iter_mut().find(|entry| ours(entry)))
        .expect("the kept hook entry remains in its group");
    if !exact(kept) {
        kept["command"] = serde_json::Value::String(cmd.to_owned());
        changed += 1;
    }
    if let Some(timeout) = timeout
        && kept.get("timeout").and_then(|value| value.as_u64()) != Some(timeout)
    {
        kept["timeout"] = timeout.into();
        changed += 1;
    }
    // Only groups this call emptied go away; every other group stays as it was.
    let mut group_index = 0;
    arr.retain(|_| {
        let kept = !emptied.contains(&group_index);
        group_index += 1;
        kept
    });
    changed
}

// ── skill / instruction install ───────────────────────────────────────

/// The agit skill lands in each runtime's native Skill directory. Beyond how many files were
/// written, this also reports whether the install is complete.
fn install_skill(runtime: Option<&str>, installed_only: bool) -> SetupReport {
    let mut report = SetupReport::default();
    for name in crate::adapter::setup_runtimes() {
        if !wants(runtime, name) {
            continue;
        }
        let Some(dir) = skill_path(name) else {
            ui::warning(&format!("cannot install {name} Skill: HOME is not set"));
            report.merge(SetupReport::failure());
            continue;
        };
        if installed_only && !dir.exists() && !legacy_skill_installed(name) {
            continue;
        }
        let skill_report = install_skill_dir(&dir, name);
        report.merge(skill_report);
        // Older releases placed the full entrypoint in an AGENTS.md marker. Remove
        // only that self-owned block after the native Skill is present; user content
        // and the separate short --agents-md block are preserved. The closure is
        // deliberately gated on a complete bundle result so a failed install cannot
        // destroy the only working copy of the Skill.
        merge_legacy_cleanup(&mut report, skill_report, || {
            let mut cleanup = remove_legacy_inline_skill(name);
            // The home manual is a Cursor entrypoint; another runtime's bundle cannot replace it.
            if name == "cursor" {
                cleanup.merge(remove_legacy_home_skill());
            }
            cleanup
        });
    }
    report
}

fn legacy_skill_installed(runtime: &str) -> bool {
    legacy_inline_skill_path(runtime).is_some_and(|path| legacy_inline_skill_exists(&path, runtime))
        || (runtime == "cursor" && legacy_home_skill_exists())
}

pub(crate) fn legacy_home_skill_path() -> Option<PathBuf> {
    home().map(|home| home.join("AGENTS.md"))
}

pub(crate) fn legacy_home_skill_exists() -> bool {
    legacy_home_skill_path().is_some_and(|path| {
        legacy_inline_skill_exists(&path, "codex") || legacy_inline_skill_exists(&path, "cursor")
    })
}

pub(crate) fn legacy_inline_skill_exists(path: &Path, runtime: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let (begin, end) = if runtime == "cursor" {
        (
            skill_bundle::CURSOR_BEGIN_MARKER,
            skill_bundle::CURSOR_END_MARKER,
        )
    } else {
        (skill_bundle::BEGIN_MARKER, skill_bundle::END_MARKER)
    };
    legacy::without_versioned_blocks(&text, begin, end).is_some()
}

fn remove_legacy_home_skill() -> SetupReport {
    let Some(path) = legacy_home_skill_path() else {
        return SetupReport::default();
    };
    let mut report = remove_marked_block_if_versioned(
        &path,
        skill_bundle::BEGIN_MARKER,
        skill_bundle::END_MARKER,
        "legacy home AGENTS.md skill",
    );
    report.merge(remove_marked_block_if_versioned(
        &path,
        skill_bundle::CURSOR_BEGIN_MARKER,
        skill_bundle::CURSOR_END_MARKER,
        "legacy home AGENTS.md skill",
    ));
    report
}

/// Resolve the native global Skill directory for a runtime.
pub(crate) fn skill_path(runtime: &str) -> Option<PathBuf> {
    (crate::adapter::registration(runtime)?.skill_dir?)().ok()
}

#[cfg(test)]
fn skill_path_for_home(home: &Path, runtime: &str) -> PathBuf {
    match runtime {
        "claude-code" => home.join(".claude/skills/agit"),
        "codex" => home.join(".codex/skills/agit"),
        "opencode" => home.join(".config/opencode/skills/agit"),
        "cursor" => home.join(".cursor/skills/agit"),
        _ => unreachable!("unknown runtime: {runtime}"),
    }
}

fn configured_codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn codex_home_for(home: Option<&Path>, configured: Option<&Path>) -> Option<PathBuf> {
    configured
        .filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|path| path.join(".codex")))
}

fn codex_home_path() -> Option<PathBuf> {
    let configured = configured_codex_home();
    codex_home_for(home().as_deref(), configured.as_deref())
}

/// Path used by releases before native Skill directories were installed.
pub(crate) fn legacy_inline_skill_path(runtime: &str) -> Option<PathBuf> {
    let home = home()?;
    match runtime {
        // The old layout always used the default Codex home, even when a later
        // process sets CODEX_HOME for the native Skill location.
        "codex" => Some(home.join(".codex/AGENTS.md")),
        "opencode" => Some(home.join(".config/opencode/AGENTS.md")),
        "cursor" => std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("AGENTS.md")),
        _ => None,
    }
}

fn install_skill_dir(dir: &Path, runtime: &str) -> SetupReport {
    let mut report = SetupReport::default();
    report.merge(write_if_changed(
        &dir.join("SKILL.md"),
        skill_bundle::entrypoint(),
        &format!("skill {runtime} entrypoint"),
    ));
    report.merge(write_if_changed(
        &dir.join(skill_bundle::VERSION_FILE),
        &format!("{}\n", skill_bundle::version()),
        &format!("skill {runtime} version"),
    ));

    let refs = dir.join(skill_bundle::REFERENCES_DIR);
    let expected: std::collections::BTreeSet<&str> = skill_bundle::SUBSKILLS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    for (name, body) in skill_bundle::SUBSKILLS {
        report.merge(write_if_changed(
            &refs.join(format!("{name}.md")),
            body,
            &format!("skill {runtime} reference {name}"),
        ));
    }

    // references/commands is a directory agit owns: stale `*.md` files are removed, while any
    // other file the user may keep alongside them is left untouched.
    match std::fs::read_dir(&refs) {
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else {
                    ui::warning(&format!(
                        "failed to inspect skill references in {}",
                        refs.display()
                    ));
                    report.merge(SetupReport::failure());
                    continue;
                };
                let path = entry.path();
                let is_stale = path.extension().and_then(|x| x.to_str()) == Some("md")
                    && path
                        .file_stem()
                        .and_then(|x| x.to_str())
                        .is_some_and(|name| !expected.contains(name));
                if !is_stale {
                    continue;
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        report.merge(SetupReport::changed_item());
                    }
                    Err(error) => {
                        ui::warning(&format!(
                            "failed to remove stale skill reference {}: {error}",
                            path.display()
                        ));
                        report.merge(SetupReport::failure());
                    }
                }
            }
        }
        Err(error) => {
            ui::warning(&format!(
                "failed to inspect skill references {}: {error}",
                refs.display()
            ));
            report.merge(SetupReport::failure());
        }
    }
    if report.succeeded() && !skill_bundle_complete(dir) {
        ui::warning(&format!(
            "skill {runtime} bundle verification failed: {}",
            dir.display()
        ));
        report.merge(SetupReport::failure());
    }
    if report.succeeded() {
        let status = if report.changed == 0 {
            "is up to date"
        } else {
            "updated"
        };
        ui::info(format_args!(
            "  {} skill {runtime} {status} → {}",
            ui::ok(ui::theme::symbols().check),
            ui::tilde(dir)
        ));
    }
    report
}

fn merge_legacy_cleanup(
    report: &mut SetupReport,
    skill_report: SetupReport,
    cleanup: impl FnOnce() -> SetupReport,
) {
    if skill_report.succeeded() {
        report.merge(cleanup());
    }
}

/// Verify every file owned by the native bundle before deleting any legacy copy.
fn skill_bundle_complete(dir: &Path) -> bool {
    if std::fs::read_to_string(dir.join("SKILL.md"))
        .ok()
        .as_deref()
        != Some(skill_bundle::entrypoint())
    {
        return false;
    }
    if std::fs::read_to_string(dir.join(skill_bundle::VERSION_FILE))
        .ok()
        .map(|version| version.trim().to_owned())
        .as_deref()
        != Some(skill_bundle::version())
    {
        return false;
    }
    let refs = dir.join(skill_bundle::REFERENCES_DIR);
    let expected: std::collections::BTreeSet<&str> = skill_bundle::SUBSKILLS
        .iter()
        .map(|(name, _)| *name)
        .collect();
    for (name, body) in skill_bundle::SUBSKILLS {
        if std::fs::read_to_string(refs.join(format!("{name}.md")))
            .ok()
            .as_deref()
            != Some(*body)
        {
            return false;
        }
    }
    let Ok(entries) = std::fs::read_dir(refs) else {
        return false;
    };
    entries.flatten().all(|entry| {
        let path = entry.path();
        path.extension().and_then(|x| x.to_str()) != Some("md")
            || path
                .file_stem()
                .and_then(|x| x.to_str())
                .is_some_and(|name| expected.contains(name))
    })
}

/// Remove a full, version-marked Skill block emitted by pre-native-layout releases.
/// The short integration block written by `--agents-md` has no skill-version marker,
/// so it is deliberately left alone.
fn remove_legacy_inline_skill(runtime: &str) -> SetupReport {
    let (path, begin, end, label) = match runtime {
        "codex" => {
            let Some(path) = legacy_inline_skill_path("codex") else {
                return SetupReport::default();
            };
            (
                path,
                skill_bundle::BEGIN_MARKER,
                skill_bundle::END_MARKER,
                "legacy codex AGENTS.md skill",
            )
        }
        "opencode" => {
            let Some(path) = legacy_inline_skill_path("opencode") else {
                return SetupReport::default();
            };
            (
                path,
                skill_bundle::BEGIN_MARKER,
                skill_bundle::END_MARKER,
                "legacy opencode AGENTS.md skill",
            )
        }
        "cursor" => {
            let Some(path) = legacy_inline_skill_path("cursor") else {
                return SetupReport::default();
            };
            (
                path,
                skill_bundle::CURSOR_BEGIN_MARKER,
                skill_bundle::CURSOR_END_MARKER,
                "legacy cursor AGENTS.md skill",
            )
        }
        _ => return SetupReport::default(),
    };
    remove_marked_block_if_versioned(&path, begin, end, label)
}

fn remove_marked_block_if_versioned(
    path: &Path,
    begin_marker: &str,
    end_marker: &str,
    label: &str,
) -> SetupReport {
    match legacy::remove(path, begin_marker, end_marker) {
        Ok(Some(backup)) => {
            ui::info(format_args!(
                "  {} {label} removed → {} (backup: {})",
                ui::ok(ui::theme::symbols().check),
                ui::tilde(path),
                ui::tilde(&backup)
            ));
            SetupReport::changed_item()
        }
        Ok(None) => SetupReport::default(),
        Err(error) => {
            ui::warning(&format!(
                "{label} removal failed: {}: {error}",
                path.display()
            ));
            SetupReport::failure()
        }
    }
}

// ── MCP registration ──────────────────────────────────────────────────

/// MCP registration writes each runtime's native configuration. All of it is idempotent, and a
/// failed configuration write propagates.
fn register_mcp(runtime: Option<&str>) -> SetupReport {
    let exe = exe_str();
    let mut report = SetupReport::default();

    if wants(runtime, "claude-code") {
        report.merge(claude_mcp::register(&exe));
    }

    if wants(runtime, "codex") {
        if let Some(p) = codex_home_path().map(|h| h.join("config.toml")) {
            report.merge(upsert_codex_mcp(&p, &exe));
        } else {
            ui::warning("cannot install Codex MCP: HOME and CODEX_HOME are not set");
            report.merge(SetupReport::failure());
        }
    }

    if wants(runtime, "opencode") {
        if let Some(p) = home().map(|h| h.join(".config/opencode/opencode.json")) {
            report.merge(upsert_json_mcp(
                &p,
                "mcp",
                "agit",
                serde_json::json!({
                    "type": "local", "command": [exe, "mcp"], "enabled": true,
                }),
            ));
        } else {
            ui::warning("cannot install OpenCode MCP: HOME is not set");
            report.merge(SetupReport::failure());
        }
    }

    if wants(runtime, "cursor") {
        if let Some(p) = home().map(|h| h.join(".cursor/mcp.json")) {
            report.merge(upsert_json_mcp(
                &p,
                "mcpServers",
                "agit",
                serde_json::json!({
                    "command": exe, "args": ["mcp"],
                }),
            ));
        } else {
            ui::warning("cannot install Cursor MCP: HOME is not set");
            report.merge(SetupReport::failure());
        }
    }

    if wants(runtime, "workbuddy") {
        report.merge(native::workbuddy_mcp(&exe));
    }
    if wants(runtime, "hermes")
        && (runtime == Some("hermes")
            || crate::adapter::get("hermes").is_ok_and(|adapter| adapter.available()))
    {
        report.merge(native::hermes(&exe, "mcp"));
    }
    if runtime == Some("openclaw") {
        ui::info(format_args!(
            "  OpenClaw uses the native lifecycle plugin and Skill; a stdio MCP registration is not required."
        ));
    }

    report
}

/// Registration is determined by parsed TOML keys, and executable paths remain literal strings.
/// An invalid namespace is user-owned configuration and cannot be repaired by replacing it.
fn upsert_codex_mcp(path: &Path, exe: &str) -> SetupReport {
    let label = path.display().to_string();
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => match text.parse::<toml::Table>() {
            Ok(doc) => doc,
            Err(_) => {
                ui::warning(&format!(
                    "cannot install Codex MCP: {label} is not valid TOML"
                ));
                return SetupReport::failure();
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(_) => {
            ui::warning(&format!("cannot install Codex MCP: failed to read {label}"));
            return SetupReport::failure();
        }
    };
    let namespace = doc
        .entry("mcp_servers")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let Some(servers) = namespace.as_table_mut() else {
        ui::warning(&format!(
            "cannot install Codex MCP: `mcp_servers` in {label} must be a table"
        ));
        return SetupReport::failure();
    };
    if let Some(existing) = servers.get("agit") {
        if !existing.is_table() {
            ui::warning(&format!(
                "cannot install Codex MCP: `mcp_servers.agit` in {label} must be a table"
            ));
            return SetupReport::failure();
        }
        ui::info(format_args!(
            "  {} mcp({}) already in place",
            ui::dim("·"),
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        return SetupReport::item();
    }
    servers.insert(
        "agit".into(),
        toml::Value::Table(toml::Table::from_iter([
            ("command".into(), toml::Value::String(exe.into())),
            (
                "args".into(),
                toml::Value::Array(vec![toml::Value::String("mcp".into())]),
            ),
        ])),
    );
    let rendered = match toml::to_string_pretty(&doc) {
        Ok(rendered) => rendered,
        Err(_) => {
            ui::warning(&format!(
                "cannot serialize Codex MCP configuration for {label}"
            ));
            return SetupReport::failure();
        }
    };
    if write_append_overwrite(path, &rendered).is_err() {
        ui::warning(&format!("failed to write {label}"));
        return SetupReport::failure();
    }
    ui::info(format_args!(
        "  {} mcp → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(path)
    ));
    SetupReport::item()
}

/// Upsert one MCP entry into a JSON config: <root_key>.<name> = entry.
/// An entry that already carries the name counts as registered (contents are not compared — an
/// upgrade that changes the path edits in place, and comparing contents only yields false
/// positives).
fn upsert_json_mcp(
    path: &Path,
    root_key: &str,
    name: &str,
    entry: serde_json::Value,
) -> SetupReport {
    let label = path.display().to_string();
    let mut doc = match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(doc) => doc,
            Err(_) => {
                ui::warning(&format!("cannot install MCP: {label} is not valid JSON"));
                return SetupReport::failure();
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(_) => {
            ui::warning(&format!("cannot install MCP: failed to read {label}"));
            return SetupReport::failure();
        }
    };
    let Some(root) = doc.as_object_mut() else {
        ui::warning(&format!(
            "cannot install MCP: {label} must contain a JSON object"
        ));
        return SetupReport::failure();
    };
    let namespace = root
        .entry(root_key)
        .or_insert_with(|| serde_json::json!({}));
    let Some(map) = namespace.as_object_mut() else {
        ui::warning(&format!(
            "cannot install MCP: `{root_key}` in {label} must be an object"
        ));
        return SetupReport::failure();
    };
    if map.contains_key(name) {
        ui::info(format_args!(
            "  {} mcp({}) already in place",
            ui::dim("·"),
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
        return SetupReport::item();
    }
    map.insert(name.to_string(), entry);
    if write_json(path, &doc).is_err() {
        ui::warning(&format!("failed to write {label}"));
        return SetupReport::failure();
    }
    ui::info(format_args!(
        "  {} mcp → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&std::path::PathBuf::from(path))
    ));
    SetupReport::item()
}

// ── project AGENTS.md ─────────────────────────────────────────────────

/// AGENTS.md: a marked, idempotent section at the top of the current Git repository. Every
/// conversation in the project reads it, so it goes nowhere else: outside a repository, or in a
/// repository at the home folder, every conversation under that folder would read it.
fn append_agents_md() -> SetupReport {
    let Some(top) = crate::infra::config::repo_root() else {
        ui::warning(
            "`--agents-md` writes into a Git repository's top level; run it inside the project",
        );
        return SetupReport::failure();
    };
    if home().is_some_and(|home| same_dir(&home, &top)) {
        ui::warning(
            "`--agents-md` writes nothing into the home folder: every conversation under it would read it",
        );
        return SetupReport::failure();
    }
    let path = project_agents_path_for(&top);
    let report = upsert_marked_block(&path, include_str!("setup_agents_section.md"), "AGENTS.md");
    if report.failures == 0 {
        ui::info(format_args!(
            "  {} tells coding agents in this project when to use agit. Commit it if your \
             teammates' agents should follow it; delete the agit section to remove it.",
            ui::tilde(&path)
        ));
    }
    report
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The instruction files that every conversation of a runtime under the home folder reads,
/// whatever its project: Claude Code reads `CLAUDE.md` in every parent folder.
fn home_instruction_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = home()
        .map(|home| {
            [
                "AGENTS.md",
                "CLAUDE.md",
                ".claude/CLAUDE.md",
                ".codex/AGENTS.md",
                ".config/opencode/AGENTS.md",
            ]
            .iter()
            .map(|file| home.join(file))
            .collect()
        })
        .unwrap_or_default();
    if let Some(codex) = codex_home_path().map(|home| home.join("AGENTS.md"))
        && !files.contains(&codex)
    {
        files.push(codex);
    }
    files
}

/// The home-level instruction files that hold an agit section, which `agit setup --skill`
/// removes.
pub(crate) fn home_sections() -> Vec<PathBuf> {
    home_instruction_files()
        .into_iter()
        .filter(|path| {
            std::fs::read_to_string(path).is_ok_and(|text| {
                legacy::without_sections(
                    &text,
                    skill_bundle::BEGIN_MARKER,
                    skill_bundle::END_MARKER,
                )
                .is_some()
            })
        })
        .collect()
}

/// An agit section in a home-level instruction file reaches every conversation, also the ones
/// that have nothing to do with AgentGit (CLI #211). Only the section goes, and a backup of the
/// file stays beside it; the user's own text and a legacy skill block, which a native skill
/// install removes once it has replaced it, are kept.
fn remove_home_sections() -> SetupReport {
    let mut report = SetupReport::default();
    for path in home_instruction_files() {
        match legacy::remove_sections(&path, skill_bundle::BEGIN_MARKER, skill_bundle::END_MARKER) {
            Ok(Some(backup)) => {
                ui::info(format_args!(
                    "  {} agit section removed from {}, which every conversation reads (backup: {})",
                    ui::ok(ui::theme::symbols().check),
                    ui::tilde(&path),
                    ui::tilde(&backup)
                ));
                report.merge(SetupReport::changed_item());
            }
            Ok(None) => {}
            Err(error) => {
                ui::warning(&format!(
                    "agit section removal failed: {}: {error}",
                    path.display()
                ));
                report.merge(SetupReport::failure());
            }
        }
    }
    report
}

fn project_agents_path_for(cwd: &Path) -> PathBuf {
    if cwd.join("CLAUDE.md").exists() && !cwd.join("AGENTS.md").exists() {
        cwd.join("CLAUDE.md")
    } else {
        cwd.join("AGENTS.md")
    }
}

// ── shared primitives ─────────────────────────────────────────────────

fn home() -> Option<PathBuf> {
    crate::infra::config::user_home()
}

/// The path hooks and MCP entries run agit by: the `agit` on `PATH` that is this executable, such
/// as an installer's link in `~/.local/bin`, rather than the file the link resolves to. Codex
/// trusts a hook by its exact command, so a command that names the resolved file loses that trust
/// whenever an update moves the file, and an untrusted hook saves nothing. Without such a link the
/// executable's own path is used, except one a cache cleanup or temporary directory removes, for
/// which another `agit` on `PATH` is preferred; setup says which path it wrote either way.
fn exe_str() -> String {
    static CHOSEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CHOSEN
        .get_or_init(|| {
            let Ok(current) = std::env::current_exe() else {
                return "agit".into();
            };
            let path = std::env::var_os("PATH");
            match hook_executable(&current, path.as_deref()) {
                Chosen::Link(link) => link.display().to_string(),
                Chosen::Itself => {
                    if !ui::quiet() {
                        ui::hint(&format!(
                        "hooks and MCP run {}, which is not on PATH; run `agit setup` again if you move it",
                            current.display()
                        ));
                    }
                    current.display().to_string()
                }
                Chosen::OtherOnPath(other) => {
                    ui::warning(&format!(
                        "this agit runs from {}, which a cache cleanup removes; hooks and MCP run {} from PATH instead",
                        current.display(),
                        other.display()
                    ));
                    other.display().to_string()
                }
                Chosen::Ephemeral => {
                    ui::warning(&format!(
                        "hooks and MCP run {}, which a cache cleanup removes, and then nothing is saved; install agit with `npx -y create-agit@latest` and run `agit setup` again",
                        current.display()
                    ));
                    current.display().to_string()
                }
            }
        })
        .clone()
}

#[derive(Debug, PartialEq)]
enum Chosen {
    /// An entry on `PATH` that resolves to this executable.
    Link(PathBuf),
    /// This executable, at a path that survives an update.
    Itself,
    /// Another `agit` on `PATH`, because this one sits where a cleanup removes it.
    OtherOnPath(PathBuf),
    /// This executable, in a temporary or cache directory, with no other `agit` on `PATH`.
    Ephemeral,
}

fn hook_executable(current: &Path, path: Option<&std::ffi::OsStr>) -> Chosen {
    choose_executable(current, path, &ephemeral)
}

fn choose_executable(
    current: &Path,
    path: Option<&std::ffi::OsStr>,
    ephemeral: &dyn Fn(&Path) -> bool,
) -> Chosen {
    if let Some(link) = launcher_on_path(current, path, ephemeral) {
        return Chosen::Link(link);
    }
    if !ephemeral(current) {
        return Chosen::Itself;
    }
    let name = current.file_name().unwrap_or_default();
    path.map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file() && !ephemeral(candidate))
        .map_or(Chosen::Ephemeral, Chosen::OtherOnPath)
}

/// Whether a path lies where a cleanup removes it: a temporary directory or npm's caches, such as
/// the copy `npx` runs.
fn ephemeral(path: &Path) -> bool {
    // The directory decides, not where a link in it points: a link that npx puts on PATH lives
    // in its cache, and an installer's link lives in a directory that stays.
    let path = match (
        path.parent().and_then(|dir| dir.canonicalize().ok()),
        path.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    };
    let temp = std::env::temp_dir();
    let temp = temp.canonicalize().unwrap_or(temp);
    let text = path.to_string_lossy().replace('\\', "/");
    path.starts_with(&temp)
        || ["/tmp/", "/private/tmp/", "/var/folders/"]
            .iter()
            .any(|prefix| text.starts_with(prefix))
        || ["/_npx/", "/.npm/", "/npm-cache/"]
            .iter()
            .any(|part| text.contains(part))
}

fn launcher_on_path(
    current: &Path,
    path: Option<&std::ffi::OsStr>,
    ephemeral: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let target = current.canonicalize().ok()?;
    let name = current.file_name()?;
    std::env::split_paths(path?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| {
            !ephemeral(candidate)
                && (candidate.canonicalize().ok().as_ref() == Some(&target)
                    || launches(candidate, current, &target))
        })
}

/// Whether an existing hook's executable runs the agit setup would write, so the hook stays byte
/// for byte as it is: a runtime that trusts a hook by its command (Codex) keeps trusting it when
/// setup runs again by another path to the same program, for example from a second AGIT_HOME.
fn runs_this_agit(existing: &str) -> bool {
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    same_program(Path::new(existing), &current, &ephemeral)
}

fn same_program(existing: &Path, current: &Path, ephemeral: &dyn Fn(&Path) -> bool) -> bool {
    if !existing.is_absolute() || ephemeral(existing) {
        return false;
    }
    let Ok(target) = current.canonicalize() else {
        return false;
    };
    let Ok(resolved) = existing.canonicalize() else {
        return false;
    };
    // Only a lasting entry is kept: a link to this program or a launcher script that runs it. A
    // hook naming the file itself, even the file a wanted link leads to, moves to the link, or it
    // would keep running the old build once the link points elsewhere. A link is told from the
    // file by resolving only its directory: a file in a linked directory (`/var` on macOS) is
    // still the file.
    let unlinked = existing
        .parent()
        .and_then(|dir| dir.canonicalize().ok())
        .zip(existing.file_name())
        .map(|(dir, name)| dir.join(name));
    (resolved == target && unlinked.as_ref() != Some(&resolved))
        || launches(existing, current, &target)
}

/// Whether `candidate` is a launcher script that runs this executable: a short file starting with
/// `#!` whose `exec` line runs it, such as `exec /opt/agit/stable/agit "$@"`. Only the program an
/// `exec` line names counts, resolved to its file; a path in a comment or inside another word does
/// not. The script stays where it is when an update points it at another build, so hooks that run
/// it keep their command.
fn launches(candidate: &Path, current: &Path, target: &Path) -> bool {
    const LAUNCHER_LIMIT: u64 = 4096;
    if !candidate
        .metadata()
        .is_ok_and(|meta| meta.is_file() && meta.len() <= LAUNCHER_LIMIT)
    {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(candidate) else {
        return false;
    };
    text.starts_with("#!")
        && text.lines().filter_map(exec_program).any(|program| {
            let program = Path::new(&program);
            program == current || program.canonicalize().ok().as_deref() == Some(target)
        })
}

/// The program a shell `exec` line runs: its first word, with quotes and backslashes removed the
/// way the shell removes them. `None` when the word depends on an expansion (`$`, a backtick, a
/// glob), whose result cannot be read from the text.
fn exec_program(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("exec ")?.trim_start();
    let mut word = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => break,
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    c => word.push(c),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    // Inside double quotes a backslash escapes only these; before any other
                    // character it stays.
                    '\\' => match chars.next()? {
                        c @ ('$' | '`' | '"' | '\\') => word.push(c),
                        c => {
                            word.push('\\');
                            word.push(c);
                        }
                    },
                    '$' | '`' => return None,
                    c => word.push(c),
                }
            },
            '\\' => word.push(chars.next()?),
            '$' | '`' | '*' | '?' | '[' | '~' => return None,
            c => word.push(c),
        }
    }
    (!word.is_empty()).then_some(word)
}

/// Bundle writes report failures individually; successful files contribute to the runtime summary.
fn write_if_changed(path: &Path, body: &str, label: &str) -> SetupReport {
    if std::fs::read_to_string(path).ok().as_deref() == Some(body) {
        return SetupReport::item();
    }
    let ok = path
        .parent()
        .and_then(|d| std::fs::create_dir_all(d).ok())
        .and_then(|_| std::fs::write(path, body).ok());
    if ok.is_none() {
        ui::warning(&format!("{label} write failed: {}", path.display()));
        return SetupReport::failure();
    }
    SetupReport::changed_item()
}

/// Upsert a section marked with `<!-- agit:begin/end -->`: an existing marked section is replaced
/// whole (so a content upgrade lands), and otherwise the section is appended.
fn upsert_marked_block(path: &Path, inner: &str, label: &str) -> SetupReport {
    upsert_marked_block_with_markers(
        path,
        inner,
        label,
        skill_bundle::BEGIN_MARKER,
        skill_bundle::END_MARKER,
    )
}

fn upsert_marked_block_with_markers(
    path: &Path,
    inner: &str,
    label: &str,
    begin_marker: &str,
    end_marker: &str,
) -> SetupReport {
    // Markers are added only here: a body carrying its own pair would end up double-wrapped, and
    // a double wrap makes the next replacement eat only the inner pair while the outer one grows
    // by one on every run.
    let inner = strip_markers(inner.trim(), begin_marker, end_marker);
    let block = format!("{begin_marker}\n{inner}\n{end_marker}");
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // The replaced span runs from the first begin to the **last** end: a file that already nests
    // several marker layers collapses to one in a single pass instead of leaving the outer layer
    // to accumulate.
    let span = match (existing.find(begin_marker), existing.rfind(end_marker)) {
        (Some(b), Some(e)) if e >= b => Some((b, e)),
        _ => None,
    };
    let new = if let Some((b, e)) = span {
        let mut s = existing[..b].to_string();
        s.push_str(&block);
        s.push_str(&existing[e + end_marker.len()..]);
        s
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", existing.trim_end())
    };
    if new == existing && !existing.is_empty() {
        ui::info(format_args!(
            "  {} {label} block already present (idempotent)",
            ui::dim("·")
        ));
        return SetupReport::item();
    }
    if write_append_overwrite(path, &new).is_err() {
        ui::warning(&format!("{label} write failed: {}", path.display()));
        return SetupReport::failure();
    }
    ui::info(format_args!(
        "  {} {label} → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&PathBuf::from(path))
    ));
    SetupReport::item()
}

/// Strip the leading and trailing markers a body carries (nested layers included); the caller is
/// the one that adds them back.
fn strip_markers<'a>(mut inner: &'a str, begin_marker: &str, end_marker: &str) -> &'a str {
    loop {
        let stripped = inner
            .strip_prefix(begin_marker)
            .unwrap_or(inner)
            .trim_start();
        let stripped = stripped
            .strip_suffix(end_marker)
            .unwrap_or(stripped)
            .trim_end();
        if stripped == inner {
            return inner;
        }
        inner = stripped;
    }
}

fn write_append_overwrite(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, text)
}

fn write_json(path: &Path, doc: &serde_json::Value) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(doc).unwrap())
}

fn print_completions(shell: &str) -> SetupReport {
    let mut cmd = crate::commands::cli_def();
    let name = "agit";
    let mut buf = vec![];
    match shell {
        "bash" => clap_complete::generate(clap_complete::shells::Bash, &mut cmd, name, &mut buf),
        "zsh" => clap_complete::generate(clap_complete::shells::Zsh, &mut cmd, name, &mut buf),
        "fish" => clap_complete::generate(clap_complete::shells::Fish, &mut cmd, name, &mut buf),
        _ => {
            ui::error(&format!("unsupported shell `{shell}` (bash / zsh / fish)."));
            return SetupReport::failure();
        }
    }
    print!("{}", String::from_utf8_lossy(&buf));
    SetupReport::item()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An installer's link or launcher script on PATH is what hooks run, not the file behind it,
    /// so moving that file (an update) leaves the hook command, and a runtime's trust in it,
    /// unchanged. A link or a
    /// binary in a directory a cleanup removes (npx's cache) is never written while a lasting
    /// `agit` is on PATH, and a relative PATH entry is never chosen.
    #[cfg(unix)]
    #[test]
    fn hooks_run_agit_by_a_path_that_outlives_updates() {
        let root = tempfile::tempdir().unwrap();
        let dir = |name: &str| {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        };
        let (release, bin, cache, other) =
            (dir("release"), dir("bin"), dir("npx-cache"), dir("other"));
        let binary = release.join("agit");
        std::fs::write(&binary, b"").unwrap();
        std::os::unix::fs::symlink(&binary, bin.join("agit")).unwrap();
        let cached = cache.join("agit");
        std::fs::write(&cached, b"").unwrap();
        let in_cache = |path: &Path| path.starts_with(&cache);
        let path = |dirs: &[&Path]| std::env::join_paths(dirs).unwrap();

        let with_link = path(&[Path::new("bin"), &cache, &bin]);
        assert_eq!(
            choose_executable(&binary, Some(&with_link), &in_cache),
            Chosen::Link(bin.join("agit"))
        );
        let launcher = dir("launcher");
        std::fs::write(
            launcher.join("agit"),
            format!("#!/bin/sh\nexec {} \"$@\"\n", binary.display()),
        )
        .unwrap();
        assert_eq!(
            choose_executable(&binary, Some(&path(&[&launcher])), &in_cache),
            Chosen::Link(launcher.join("agit"))
        );
        assert_eq!(
            choose_executable(&binary, Some(&path(&[&other])), &in_cache),
            Chosen::Itself
        );
        std::fs::write(other.join("agit"), b"").unwrap();
        assert_eq!(
            choose_executable(&cached, Some(&path(&[&cache, &other])), &in_cache),
            Chosen::OtherOnPath(other.join("agit"))
        );
        assert_eq!(
            choose_executable(&cached, Some(&path(&[&cache])), &in_cache),
            Chosen::Ephemeral
        );
        // A hook that already runs this program by a lasting entry, a link or a launcher, is kept
        // as written; one that names the file itself moves to the link, and one that runs another
        // program, or sits in the cache, is not kept.
        let launcher_agit = launcher.join("agit");
        assert!(same_program(&bin.join("agit"), &binary, &in_cache));
        assert!(same_program(&launcher_agit, &binary, &in_cache));
        assert!(
            !same_program(&binary, &binary, &in_cache),
            "a hook naming the file behind a link moves to the link"
        );
        assert!(!same_program(&other.join("agit"), &binary, &in_cache));
        assert!(!same_program(&cached, &cached, &in_cache));
        // Only the program an `exec` line runs makes a launcher: a comment naming this build, or
        // a longer name that contains it, does not.
        let moved = dir("moved");
        std::fs::write(
            moved.join("agit"),
            format!(
                "#!/bin/sh\n# previous build: {}\nexec {}-next \"$@\"\n",
                binary.display(),
                binary.display()
            ),
        )
        .unwrap();
        assert!(!launches(
            &moved.join("agit"),
            &binary,
            &binary.canonicalize().unwrap()
        ));
        assert_eq!(
            exec_program("exec '/opt/agit/a b/agit' \"$@\""),
            Some("/opt/agit/a b/agit".into())
        );
        // A quoted part and what follows it are one word, the program the shell runs.
        assert_eq!(
            exec_program("exec \"/opt/agit/agit\"-next \"$@\""),
            Some("/opt/agit/agit-next".into())
        );
        assert_eq!(exec_program("exec \"$AGIT_BIN\" \"$@\""), None);
        // Inside double quotes a backslash before an ordinary character is part of the path.
        assert_eq!(
            exec_program("exec \"/opt/a\\b/agit\" \"$@\""),
            Some("/opt/a\\b/agit".into())
        );
        assert_eq!(
            exec_program("exec \"/opt/\\\"q\\\"/agit\""),
            Some("/opt/\"q\"/agit".into())
        );
        assert!(ephemeral(&std::env::temp_dir().join("agit")));
        assert!(ephemeral(Path::new(
            "/Users/me/.npm/_npx/0a1b/node_modules/.bin/agit"
        )));
        assert!(!ephemeral(Path::new("/usr/local/bin/agit")));
    }

    #[test]
    fn hook_registration_matches_native_arguments_instead_of_serialized_substrings() {
        for command in [r"C:\tools\agit.exe hooks ingest", "agit hooks ingest"] {
            let original = serde_json::json!([
                {"description": command, "hooks": [{"type": "command", "command": "user-hook"}]},
                {"hooks": [{"type": "prompt", "command": command}]},
                {"hooks": [{"type": "command", "command": format!("{command} --extra")}]}
            ]);
            let mut hooks = serde_json::json!({"SessionStart": original.clone()});
            assert_eq!(
                upsert_hook(&mut hooks, "SessionStart", command, "hooks ingest", None),
                1
            );
            let groups = hooks["SessionStart"].as_array().unwrap();
            assert_eq!(&groups[..3], original.as_array().unwrap());
            assert_eq!(groups[3]["hooks"][0]["command"], command);
            let installed = hooks.clone();
            assert_eq!(
                upsert_hook(&mut hooks, "SessionStart", command, "hooks ingest", None),
                0
            );
            assert_eq!(hooks, installed);
        }
    }

    /// A hook command is run by a shell, so an executable whose path holds a space or a quote
    /// still reaches it as one word naming that file. Joined bare, `sh` looks for the program
    /// named by the text before the space and the hook fails on every event; and an entry written
    /// that way is rewritten in place, so the broken copy does not keep failing beside the new one.
    #[cfg(unix)]
    #[test]
    fn a_hook_command_runs_under_sh_from_a_path_with_a_space() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Application Support/it's");
        std::fs::create_dir_all(&folder).unwrap();
        let seen = dir.path().join("argv");
        let exe = folder.join("agit");
        std::fs::write(
            &exe,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", seen.display()),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exe = exe.to_str().unwrap();
        let (event, argv) = HOOKS[1];
        let arguments = hook_arguments(argv, None);
        let mut hooks = serde_json::json!({
            event: [{"hooks": [{"type": "command", "command": format!("{exe} {arguments}")}]}]
        });
        // The bare join is not one program, so it never counts as already installed, not even
        // when it names the agit that runs setup.
        assert_eq!(
            hook_program(&format!("{exe} {arguments}"), &arguments),
            None
        );
        let command = hook_command(exe, argv, None);
        assert_eq!(
            upsert_hook(&mut hooks, event, &command, &arguments, None),
            1
        );
        assert_eq!(
            hooks[event],
            serde_json::json!([{"hooks": [{"type": "command", "command": command}]}])
        );
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &command])
            .status()
            .unwrap();
        assert!(status.success(), "{command}");
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            format!("{}\n", argv.join("\n"))
        );
        // A Windows path keeps its backslashes and stays one word for a POSIX hook shell too.
        let windows = r"C:\Users\Jo Smith\AppData\Roaming\npm\agit.cmd";
        let word = shell_word_for(windows, true);
        let output = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                &format!("set -- {word} {arguments}; printf '%s' \"$1\""),
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(output.stdout).unwrap(), windows);
        assert_eq!(
            hook_program(&format!("{word} {arguments}"), &arguments).as_deref(),
            Some(windows)
        );
    }

    #[test]
    fn runtime_filter_accepts_none_all_and_exact_match() {
        assert!(wants(None, "codex"));
        assert!(wants(Some("all"), "cursor"));
        assert!(wants(Some("cursor"), "cursor"));
        assert!(!wants(Some("cursor"), "codex"));
        assert!(!wants(Some("codex"), "all")); // a runtime name is no synonym for "all"
    }

    /// Capability presence, not a release number, decides whether writing Codex hooks is safe.
    #[test]
    fn codex_hook_capability_comes_from_the_feature_table() {
        assert_eq!(
            parse_codex_hooks_capability(
                "goals stable true\nhooks stable true\nplugins stable true\n"
            ),
            Some(CodexHooksCapability { enabled: true })
        );
        assert_eq!(
            parse_codex_hooks_capability("hooks under-development false\n"),
            Some(CodexHooksCapability { enabled: false })
        );
        assert_eq!(parse_codex_hooks_capability("plugins stable true\n"), None);
    }

    /// Installing into Codex keeps unrelated hooks, names the runtime explicitly, and is
    /// idempotent. A malformed file is preserved byte for byte instead of being replaced with an
    /// empty object, because configuration owned by another program is not repairable by guessing.
    #[test]
    fn codex_hook_file_is_merged_without_repairing_user_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [{"hooks": [{
                        "type": "command",
                        "command": "/usr/bin/user-hook"
                    }]}]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(install_hook_file(&path, "Codex", "/usr/bin/agit", Some("codex"), &[]).succeeded());
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(once.contains("/usr/bin/user-hook"), "{once}");
        assert!(once.contains("agit hooks ingest --runtime codex"), "{once}");
        assert!(once.contains("agit hooks settle --runtime codex"), "{once}");
        assert!(install_hook_file(&path, "Codex", "/usr/bin/agit", Some("codex"), &[]).succeeded());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);

        std::fs::write(&path, "not json\n").unwrap();
        assert_eq!(
            install_hook_file(&path, "Codex", "/usr/bin/agit", Some("codex"), &[]).failures,
            1
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json\n");
    }

    /// Installs from different executable paths keep one entry per hook: the stale path is
    /// replaced and extra copies are removed, while a user's hook sharing the group stays. Matching
    /// on the whole command string instead adds one more concurrent hook per install location. The
    /// install that rewrites the file keeps the file it read beside it; one that wrote without it
    /// would leave the person no copy of the settings it sorted.
    #[test]
    fn hooks_installed_from_another_path_are_replaced_instead_of_stacked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "hooks": {
                    "SessionStart": [
                        {"hooks": [
                            {"type": "command", "command": "/usr/bin/user-hook"},
                            {"type": "command", "command": "/usr/local/bin/agit hooks ingest --runtime workbuddy"}
                        ]},
                        {"hooks": [{"type": "command", "command": "/tmp/npx-cache/agit hooks ingest --runtime workbuddy"}]}
                    ]
                }
            })
            .to_string(),
        )
        .unwrap();

        let original = std::fs::read_to_string(&path).unwrap();
        let exe = "/home/me/.local/bin/agit";
        assert!(install_hook_file(&path, "WorkBuddy", exe, Some("workbuddy"), &[]).succeeded());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("settings.json.agit-backup")).unwrap(),
            original
        );
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let commands = |event: &str| -> Vec<String> {
            doc["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap().iter())
                .map(|entry| entry["command"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            commands("SessionStart"),
            [
                "/usr/bin/user-hook",
                "/home/me/.local/bin/agit hooks ingest --runtime workbuddy"
            ]
        );
        assert_eq!(
            commands("Stop"),
            ["/home/me/.local/bin/agit hooks settle --runtime workbuddy"]
        );
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(install_hook_file(&path, "WorkBuddy", exe, Some("workbuddy"), &[]).succeeded());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);
    }

    /// A hook repair (`setup --hooks --installed-only`) changes only the command text of agit's
    /// hooks for this program: the bare path the shell split becomes the quoted command, while
    /// the file keeps its key order, its layout, the person's hooks and a hook that runs another
    /// agit, and the first original is kept beside it, for its owner only even when the original
    /// is group-readable. A repair that parsed and rewrote the document would sort the person's
    /// settings, and one that installed would turn capture on where they never enabled it; the
    /// menu bar app runs the repair without asking.
    #[test]
    fn a_hook_repair_changes_only_the_broken_command_text() {
        let dir = tempfile::tempdir().unwrap();
        let exe = "/opt/Agent Git/agit-dev";
        let missing = dir.path().join("absent/settings.json");
        assert_eq!(
            repair_hook_file(&missing, "Claude Code", exe, None),
            SetupReport::default()
        );
        assert!(!missing.exists());

        let (event, argv) = HOOKS[1];
        let broken = format!("{exe} {}", hook_arguments(argv, None));
        let other = format!("/usr/local/bin/agit {}", hook_arguments(HOOKS[0].1, None));
        let original = format!(
            "{{\n  \"model\": \"opus\",\n  \"hooks\": {{\n    \"SessionStart\": [{{\"hooks\": [{{\"type\": \"command\", \"command\": \"{other}\"}}]}}],\n    \"{event}\": [ {{ \"hooks\": [ {{ \"command\": \"{broken}\", \"type\": \"command\" }}, {{\"type\": \"command\", \"command\": \"/usr/bin/user-hook\"}} ] }} ]\n  }},\n  \"env\": {{\"A\": \"1\"}}\n}}\n"
        );
        let path = dir.path().join("settings.json");
        std::fs::write(&path, &original).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o640))
            .unwrap();
        let report = repair_hook_file(&path, "Claude Code", exe, None);
        assert_eq!((report.items, report.changed, report.failures), (1, 1, 0));
        let command = serde_json::Value::String(hook_command(exe, argv, None)).to_string();
        let repaired = original.replace(&format!("\"{broken}\""), &command);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repaired);
        let backup = dir.path().join("settings.json.agit-backup");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &std::fs::metadata(&backup).unwrap().permissions()
            ) & 0o777,
            0o600
        );

        let report = repair_hook_file(&path, "Claude Code", exe, None);
        assert_eq!((report.items, report.changed, report.failures), (1, 0, 0));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), repaired);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

        // The same text as another `command` value: rewriting it would change that setting too.
        let shared = original.replace(
            "\"env\": {\"A\": \"1\"}",
            &format!("\"statusLine\": {{\"type\": \"command\", \"command\": \"{broken}\"}}"),
        );
        std::fs::write(&path, &shared).unwrap();
        assert_eq!(
            repair_hook_file(&path, "Claude Code", exe, None).failures,
            1
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), shared);
    }

    /// **The command written out must parse as an agit invocation.**
    ///
    /// This is the root-cause guard for B1: a hook whose subcommand takes no positional argument
    /// makes every SessionStart trigger exit with code 2, the harness swallows that silently, and
    /// nobody sees a thing. A command string written into a configuration file is the only thing
    /// in this repository that really runs without passing through CI, so it is run once here.
    #[test]
    fn every_installed_hook_command_parses_as_a_real_agit_invocation() {
        use clap::Parser as _;
        let claude = CLAUDE_HOOKS.iter().map(|(event, argv, _)| (*event, *argv));
        for (event, argv) in HOOKS.iter().copied().chain(claude) {
            let mut cli: Vec<&str> = vec!["agit"];
            cli.extend_from_slice(argv);
            crate::commands::Cli::try_parse_from(&cli).unwrap_or_else(|e| {
                panic!(
                    "hook {event} installs `{}`, which agit cannot parse: {e}",
                    argv.join(" ")
                )
            });
        }
        // The command string itself must round-trip to the same argv (the join swallows no
        // argument).
        assert_eq!(
            hook_command("/usr/bin/agit", HOOKS[0].1, None),
            "/usr/bin/agit hooks ingest"
        );
        assert_eq!(
            hook_command("/usr/bin/agit", HOOKS[0].1, Some("codex")),
            "/usr/bin/agit hooks ingest --runtime codex"
        );
    }

    /// What lands in settings.json is the same data as [`HOOKS`] — otherwise the test above only
    /// examines itself.
    #[test]
    fn the_settings_file_gets_exactly_those_commands() {
        let mut hooks = serde_json::json!({});
        for (event, argv) in HOOKS {
            assert_eq!(
                upsert_hook(
                    &mut hooks,
                    event,
                    &hook_command("agit", argv, None),
                    &hook_arguments(argv, None),
                    None
                ),
                1
            );
        }
        let text = hooks.to_string();
        assert!(text.contains("agit hooks ingest"), "{text}");
        assert!(text.contains("agit hooks settle"), "{text}");
        // Idempotent: the same command is never written twice.
        for (event, argv) in HOOKS {
            assert_eq!(
                upsert_hook(
                    &mut hooks,
                    event,
                    &hook_command("agit", argv, None),
                    &hook_arguments(argv, None),
                    None
                ),
                0
            );
        }
    }

    /// On a machine that still carries it, the old Stop hook must be retired.
    ///
    /// Leaving it does not merely "run one extra command": the old one locates the branch through
    /// `AGIT_SESSION` and the new one through the payload's session_id, so once the user switches
    /// sessions inside the runtime the two point at **different branches** — one conversation
    /// settles into two histories, which is harder to clean up than not settling at all.
    #[test]
    fn the_old_stop_hook_is_retired_on_upgrade() {
        // The legacy form, with an exe path that differs from this machine's (installed by npm
        // vs installed by setup.sh).
        let mut hooks = serde_json::json!({
            "SessionStart": [{"hooks":[{"type":"command","command":"/usr/local/bin/agit hooks ingest"}]}],
            "Stop": [{"hooks":[{"type":"command","command":"/usr/local/bin/agit commit --from-hook"}]}]
        });
        for (event, argv) in RETIRED_HOOKS {
            assert_eq!(
                retire_hook(&mut hooks, event, argv),
                1,
                "{event} must be retired"
            );
        }
        for (event, argv) in HOOKS {
            upsert_hook(
                &mut hooks,
                event,
                &hook_command("agit", argv, None),
                &hook_arguments(argv, None),
                None,
            );
        }
        let text = hooks.to_string();
        assert!(
            !text.contains("commit --from-hook"),
            "the old hook is gone: {text}"
        );
        assert!(text.contains("agit hooks settle"), "{text}");
        // Retiring again after one pass removes nothing — idempotent, so a repeated `agit setup`
        // does not report a change every time.
        for (event, argv) in RETIRED_HOOKS {
            assert_eq!(retire_hook(&mut hooks, event, argv), 0);
        }
    }

    /// Only our own hook shape is recognized; everything else is left untouched.
    ///
    /// Retiring can only delete a hook **whole**. Widen the test by one notch and what disappears
    /// is more than our own fragment: the user's wrapper goes with it, a shell chain loses its
    /// first half, and a tool passing this text as an argument has nothing to do with us at all.
    /// None of these are hypothetical — they are the ordinary ways agit gets fitted into an
    /// existing toolchain.
    #[test]
    fn only_our_own_hook_shape_is_retired() {
        const ARGV: &str = "commit --from-hook";
        for ours in [
            "agit commit --from-hook",
            "/usr/local/bin/agit commit --from-hook",
            "~/.local/bin/agit commit --from-hook",
            "C:\\Users\\me\\agit.exe commit --from-hook",
            "  /usr/local/bin/agit commit --from-hook  ",
            "'/opt/Agent Git/agit' commit --from-hook",
            "\"C:\\Program Files\\agit\\agit.exe\" commit --from-hook",
        ] {
            assert!(
                is_our_hook(ours, ARGV),
                "our own hook is recognized: {ours}"
            );
        }
        for theirs in [
            // wrapper: deleting the entry deletes the wrapper too.
            "/Users/me/bin/wrap agit commit --from-hook",
            // shell chain: ours is only one link of it.
            "/Users/me/bin/before && /usr/local/bin/agit commit --from-hook",
            // another tool passes this text as an argument.
            "/bin/echo commit --from-hook",
            // the name merely ends in agit.
            "/usr/local/bin/notagit commit --from-hook",
            // the path is chosen when the hook runs: the shell expands these in double quotes.
            "\"${AGIT_HOME:-/opt/Agent Git}/agit\" commit --from-hook",
            "\"$(command -v agit)\" commit --from-hook",
            "\"`which agit`\" commit --from-hook",
            // the arguments are incomplete / not at the end.
            "/usr/local/bin/agit commit --from-hook --extra",
            "/usr/local/bin/agit hooks settle",
        ] {
            assert!(
                !is_our_hook(theirs, ARGV),
                "not mistaken for our own: {theirs}"
            );
        }
        // Installing leaves such a hook as written as well, and adds agit's own beside it.
        let theirs = "\"${AGIT_HOME:-/opt/Agent Git}/agit\" hooks settle";
        let mut hooks = serde_json::json!({
            "Stop": [{"hooks": [{"type": "command", "command": theirs}]}]
        });
        assert_eq!(
            upsert_hook(
                &mut hooks,
                "Stop",
                "agit hooks settle",
                "hooks settle",
                None
            ),
            1
        );
        assert_eq!(hooks["Stop"][0]["hooks"][0]["command"], theirs);
        assert_eq!(hooks["Stop"][1]["hooks"][0]["command"], "agit hooks settle");
    }

    /// Retiring must not touch the user's own hooks.
    ///
    /// The group agit writes holds a single command, so "delete the whole group" looks right on
    /// the install-and-retire-your-own path. But the user may well have folded agit's command into
    /// their own group, or another tool may have merged `settings.json` — deleting the group is
    /// then **silently deleting someone else's configuration**, one of the hardest kinds of damage
    /// agit can do on a machine to track down: the victim never thinks to suspect agit.
    #[test]
    fn retiring_does_not_take_the_users_own_hooks_with_it() {
        let mut hooks = serde_json::json!({
            "Stop": [
                // The user folded agit's command into their own group, next to a command that
                // carries the same substring but is not ours — that one must stay.
                {"matcher": "*", "hooks": [
                    {"type": "command", "command": "/Users/me/bin/my-notifier"},
                    {"type": "command", "command": "/usr/local/bin/agit commit --from-hook"},
                    {"type": "command", "command": "/Users/me/bin/wrap agit commit --from-hook"},
                    {"type": "command", "command": "/Users/me/bin/log-turn"}
                ]},
                // Another group has nothing to do with agit.
                {"hooks": [{"type": "command", "command": "/Users/me/bin/unrelated"}]}
            ]
        });
        assert_eq!(retire_hook(&mut hooks, "Stop", "commit --from-hook"), 1);

        let stop = hooks["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2, "a group holding no agit command survives");
        let first = stop[0]["hooks"].as_array().unwrap();
        assert_eq!(first.len(), 3, "only our own entry is removed");
        assert_eq!(first[0]["command"], "/Users/me/bin/my-notifier");
        assert_eq!(
            first[1]["command"],
            "/Users/me/bin/wrap agit commit --from-hook"
        );
        assert_eq!(first[2]["command"], "/Users/me/bin/log-turn");
        assert_eq!(hooks["Stop"][0]["matcher"], "*", "the matcher is kept");
        assert_eq!(stop[1]["hooks"].as_array().unwrap().len(), 1);
        // Ours is gone while the wrapper's entry stays — so a substring cannot carry the
        // assertion.
        assert!(
            !hooks
                .to_string()
                .contains("/usr/local/bin/agit commit --from-hook")
        );
    }

    /// A group agit alone occupies goes away with its entry once emptied — an empty shell in the
    /// configuration file is something that does no work, and the next person to read it believes
    /// a hook still runs.
    #[test]
    fn a_group_that_only_held_our_hook_goes_away_entirely() {
        let mut hooks = serde_json::json!({
            "Stop": [
                {"hooks": [{"type": "command", "command": "~/.local/bin/agit commit --from-hook"}]},
                {"hooks": [{"type": "command", "command": "/Users/me/bin/keep-me"}]}
            ]
        });
        assert_eq!(retire_hook(&mut hooks, "Stop", "commit --from-hook"), 1);
        let stop = hooks["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "an emptied group is removed with it");
        assert_eq!(stop[0]["hooks"][0]["command"], "/Users/me/bin/keep-me");
    }

    /// Nothing in the retire table may collide with a hook still in service.
    #[test]
    fn nothing_retired_is_still_installed() {
        for (_, retired) in RETIRED_HOOKS {
            for (_, argv) in HOOKS {
                assert_ne!(
                    &argv.join(" ").as_str(),
                    retired,
                    "`{retired}` must not appear in both HOOKS and RETIRED_HOOKS"
                );
            }
        }
    }

    /// The entrypoint has to keep `import` and `new` apart for an agent that is *not* yet
    /// managed: a skill that only describes `new` sends it off to create an empty session
    /// instead of adopting the transcript it is already running in. Pins the discovery
    /// command, the adoption command, and the sentence that says `new` cannot take over.
    #[test]
    fn skill_distinguishes_importing_current_session_from_starting_a_new_one() {
        let skill = include_str!("setup_skill.md");
        assert!(skill.contains("agit status --check-missing"));
        assert!(
            skill.contains(
                "agit import <session-id> --from <runtime> --repo <owner/repo> -b <branch>"
            )
        );
        assert!(skill.contains("`agit import` links that existing transcript"));
        assert!(skill.contains("--propose-lineage"));
        assert!(skill.contains("pipes, JSON, CI and agent calls return choices without adopting"));
        assert!(skill.contains("`agit new` cannot take over the session that is already running"));
    }

    #[test]
    fn marked_block_upsert_appends_then_replaces_in_place() {
        let dir = std::env::temp_dir().join(format!("agit-setup-test-{}", std::process::id()));
        let p = dir.join("AGENTS.md");
        // First append after existing content
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, "# my project\n").unwrap();
        assert_eq!(upsert_marked_block(&p, "rule A", "t").items, 1);
        let v1 = std::fs::read_to_string(&p).unwrap();
        assert!(v1.starts_with("# my project"));
        assert!(v1.contains("<!-- agit:begin -->\nrule A\n<!-- agit:end -->"));
        // A second upsert replaces in place, leaving no earlier rule behind
        upsert_marked_block(&p, "rule B", "t");
        let v2 = std::fs::read_to_string(&p).unwrap();
        assert!(v2.contains("rule B") && !v2.contains("rule A") && v2.starts_with("# my project"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn marker_count(text: &str) -> (usize, usize) {
        (
            text.matches(skill_bundle::BEGIN_MARKER).count(),
            text.matches(skill_bundle::END_MARKER).count(),
        )
    }

    /// When the section body carries its own markers, or the file already nests several layers,
    /// exactly one pair survives the upsert: an implementation replacing only up to "the first
    /// end" leaves the outer end behind and grows the file by a line on every run.
    #[test]
    fn marked_block_upsert_keeps_exactly_one_pair_of_markers() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("AGENTS.md");
        let section = include_str!("setup_agents_section.md");
        std::fs::write(&p, "# my project\n").unwrap();
        upsert_marked_block(&p, section, "t");
        upsert_marked_block(&p, section, "t");
        let v = std::fs::read_to_string(&p).unwrap();
        assert_eq!(marker_count(&v), (1, 1), "{v}");
        assert!(v.contains("Session version control (agit)"));

        // A body carrying its own pair does not become double-wrapped either.
        let wrapped = format!(
            "{}\n{}\n{}",
            skill_bundle::BEGIN_MARKER,
            section.trim(),
            skill_bundle::END_MARKER
        );
        upsert_marked_block(&p, &wrapped, "t");
        assert_eq!(marker_count(&std::fs::read_to_string(&p).unwrap()), (1, 1));

        // A file that has grown several marker layers collapses to one, with the text around it
        // left unchanged.
        std::fs::write(
            &p,
            format!(
                "# my project\n\n{b}\n{b}\nold\n{e}\n{e}\n{e}\n\n## after\n",
                b = skill_bundle::BEGIN_MARKER,
                e = skill_bundle::END_MARKER
            ),
        )
        .unwrap();
        upsert_marked_block(&p, "new", "t");
        let v = std::fs::read_to_string(&p).unwrap();
        assert_eq!(marker_count(&v), (1, 1), "{v}");
        assert!(v.starts_with("# my project"));
        assert!(v.contains("\nnew\n") && !v.contains("old"));
        assert!(v.trim_end().ends_with("## after"));
    }

    #[test]
    fn claude_skill_sync_writes_all_references_and_removes_stale_markdown() {
        let dir = std::env::temp_dir().join(format!("agit-skill-sync-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(install_skill_dir(&dir, "claude-code").succeeded());
        assert_eq!(
            std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
            skill_bundle::entrypoint()
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(skill_bundle::VERSION_FILE))
                .unwrap()
                .trim(),
            skill_bundle::version()
        );
        for (name, body) in skill_bundle::SUBSKILLS {
            assert_eq!(
                std::fs::read_to_string(
                    dir.join(skill_bundle::REFERENCES_DIR)
                        .join(format!("{name}.md"))
                )
                .unwrap(),
                *body
            );
        }
        std::fs::write(
            dir.join(skill_bundle::REFERENCES_DIR)
                .join("old-command.md"),
            "old",
        )
        .unwrap();
        install_skill_dir(&dir, "claude-code");
        assert!(
            !dir.join(skill_bundle::REFERENCES_DIR)
                .join("old-command.md")
                .exists()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_skill_install_keeps_legacy_inline_skill() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file at the destination path makes every bundle write fail:
        // `dir/SKILL.md` cannot be created beneath a file.
        let blocked_dir = dir.path().join("skill");
        std::fs::write(&blocked_dir, "not a directory").unwrap();

        let legacy = dir.path().join("AGENTS.md");
        let original = format!(
            "before\n{}\n<!-- agit:skill-version:old -->\nold\n{}\nafter\n",
            skill_bundle::BEGIN_MARKER,
            skill_bundle::END_MARKER
        );
        std::fs::write(&legacy, &original).unwrap();

        let skill_report = install_skill_dir(&blocked_dir, "codex");
        assert!(!skill_report.succeeded());
        let mut cleanup_report = SetupReport::default();
        merge_legacy_cleanup(&mut cleanup_report, skill_report, || {
            remove_marked_block_if_versioned(
                &legacy,
                skill_bundle::BEGIN_MARKER,
                skill_bundle::END_MARKER,
                "legacy",
            )
        });
        assert_eq!(std::fs::read_to_string(legacy).unwrap(), original);
    }

    #[test]
    fn skill_paths_use_each_runtime_native_global_directory() {
        let home = Path::new("/tmp/agit-home");
        assert_eq!(
            skill_path_for_home(home, "claude-code"),
            home.join(".claude/skills/agit")
        );
        assert_eq!(
            skill_path_for_home(home, "codex"),
            home.join(".codex/skills/agit")
        );
        assert_eq!(
            skill_path_for_home(home, "opencode"),
            home.join(".config/opencode/skills/agit")
        );
        assert_eq!(
            skill_path_for_home(home, "cursor"),
            home.join(".cursor/skills/agit")
        );
    }

    #[test]
    fn codex_home_prefers_explicit_configuration_without_home() {
        let configured = Path::new("/tmp/configured-codex");
        let home = Path::new("/tmp/agit-home");
        assert_eq!(
            codex_home_for(None, Some(configured)),
            Some(configured.to_path_buf())
        );
        assert_eq!(codex_home_for(Some(home), None), Some(home.join(".codex")));
        assert_eq!(codex_home_for(None, None), None);
    }

    #[test]
    fn legacy_inline_skill_removal_preserves_surrounding_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        let text = format!(
            "before\n{}\n<!-- agit:skill-version:old -->\nold\n{}\nafter\n",
            skill_bundle::BEGIN_MARKER,
            skill_bundle::END_MARKER
        );
        std::fs::write(&path, text).unwrap();
        assert_eq!(
            remove_marked_block_if_versioned(
                &path,
                skill_bundle::BEGIN_MARKER,
                skill_bundle::END_MARKER,
                "legacy"
            )
            .items,
            1
        );
        let result = std::fs::read_to_string(path).unwrap();
        assert_eq!(result, "before\nafter\n");
    }

    #[test]
    fn json_mcp_upsert_preserves_other_servers() {
        let dir = std::env::temp_dir().join(format!("agit-setup-mcp-{}", std::process::id()));
        let p = dir.join("mcp.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&p, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let n = upsert_json_mcp(
            &p,
            "mcpServers",
            "agit",
            serde_json::json!({"command":"agit","args":["mcp"]}),
        );
        assert_eq!(n.items, 1);
        let doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(doc.pointer("/mcpServers/agit").is_some());
        assert!(doc.pointer("/mcpServers/other").is_some());
        // Idempotent: a second call changes nothing
        assert_eq!(
            upsert_json_mcp(&p, "mcpServers", "agit", serde_json::json!({})).items,
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_mcp_paths_roundtrip_without_toml_escape_interpretation() {
        for exe in [
            r"C:\Users\synthetic\agit.exe",
            r"C:\tools\agit.exe",
            "C:\\quoted\\\"agit\".exe",
            "line\ncarriage\rtab\tcontrol\u{1f}\\agit",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            assert!(upsert_codex_mcp(&path, exe).succeeded());
            let installed = std::fs::read_to_string(&path).unwrap();
            let doc: toml::Table = installed.parse().unwrap();
            assert_eq!(doc["mcp_servers"]["agit"]["command"].as_str(), Some(exe));
            assert_eq!(
                doc["mcp_servers"]["agit"]["args"].as_array().unwrap(),
                &[toml::Value::String("mcp".into())]
            );
            assert!(upsert_codex_mcp(&path, "changed").succeeded());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), installed);
        }
    }

    #[test]
    fn codex_mcp_registration_uses_keys_and_preserves_unrelated_values() {
        for original in [
            "# [mcp_servers.agit] is only a comment\nmodel = 'SYNTHETIC'\n[mcp_servers.other]\ncommand = 'other'\n",
            "mcp_servers = { other = { command = 'other' } }\nmodel = 'SYNTHETIC'\n",
            "['mcp_servers']\nother = { command = 'other' }\n",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            std::fs::write(&path, original).unwrap();
            let expected: toml::Table = original.parse().unwrap();
            assert!(upsert_codex_mcp(&path, "synthetic-agit").succeeded());
            let mut actual: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
            let entry = actual["mcp_servers"]
                .as_table_mut()
                .unwrap()
                .remove("agit")
                .unwrap();
            assert_eq!(entry["command"].as_str(), Some("synthetic-agit"));
            assert_eq!(actual, expected);
        }
        for original in [
            "['mcp_servers'.'agit']\ncommand = 'user-choice'\n# retained\n",
            "mcp_servers = { agit = { command = 'user-choice' } }\n# retained\n",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            std::fs::write(&path, original).unwrap();
            assert!(upsert_codex_mcp(&path, "replacement").succeeded());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
    }

    #[test]
    fn codex_mcp_refuses_unreadable_invalid_or_non_table_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        for bytes in [
            b"model = [\n".as_slice(),
            b"mcp_servers = 'user-data'\n".as_slice(),
            b"[mcp_servers]\nagit = 'user-data'\n".as_slice(),
            b"[mcp_servers.agit]\ncommand = 'x'\nmodel = [\n".as_slice(),
            &[0xff, 0xfe],
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(upsert_codex_mcp(&path, "agit").failures, 1);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(upsert_codex_mcp(&path, "agit").failures, 1);
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    }
}
