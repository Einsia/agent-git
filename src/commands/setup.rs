//! `agit setup` — install every integration in one pass. Idempotent, and installs everything by
//! default; without it everything still works, at the manual pace.
//!
//! * `--hooks`: settle automatically at the end of a turn (silent; a failure never blocks) +
//!   `AGIT_SESSION` injection + registering a newly opened unadopted session inside a bound
//!   workspace as pending adoption. Discovery answers only "which ones exist".
//!
//!   It never guesses "whose it is" and never creates a branch. hooks are a mechanism only Claude
//!   Code has (codex / opencode / cursor have no equivalent "end of turn callback"), so their
//!   discipline rides on instruction injection: the rule in the global instruction file teaches
//!   the agent to call `agit commit` itself.
//! * `--skill`: install one entrypoint plus command references read on demand into each runtime's
//!   native Skill directory:
//!   - claude-code → `~/.claude/skills/agit/`
//!   - codex → `$CODEX_HOME/skills/agit/` (default `~/.codex/skills/agit/`)
//!   - opencode → `~/.config/opencode/skills/agit/`
//!   - cursor → `~/.cursor/skills/agit/`
//!     Every directory holds `SKILL.md`, `VERSION` and `references/commands/`; `--skill` does not
//!     expand the full manual into `AGENTS.md`. Use `--agents-md` for the short adoption rule.
//! * `--mcp`: register `agit mcp` (stdio MCP server: search/show/view/status/commit) into each
//!   runtime's native MCP configuration:
//!   - claude-code → `claude mcp add` (prints guidance instead of failing when it cannot install)
//!   - codex → `[mcp_servers.agit]` in `~/.codex/config.toml`
//!   - opencode → `mcp.agit` in `~/.config/opencode/opencode.json`
//!   - cursor → `mcpServers.agit` in `~/.cursor/mcp.json`
//! * `--agents-md`: append a marked, idempotent section to the project AGENTS.md.
//! * `--completions <shell>`: shell completions.

use super::CmdResult;
use super::skill_bundle;
use crate::{ExitCode, ui};
use clap::Args as ClapArgs;
use std::path::{Path, PathBuf};

/// Valid values for `--runtime`. Separate from the adapter list: setup installs "where an
/// integration lands", the adapter answers "how a transcript is read and written". claude-desktop
/// has no integration site of its own (it reuses everything claude-code has), so it is not
/// listed here.
const RUNTIMES: &[&str] = &["all", "claude-code", "codex", "cursor", "opencode"];

#[derive(ClapArgs)]
pub struct Args {
    /// Install for one runtime only (default: every runtime noticed).
    #[arg(long, value_name = "all|claude-code|codex|cursor|opencode")]
    pub runtime: Option<String>,
    #[arg(long)]
    pub hooks: bool,
    #[arg(long)]
    pub skill: bool,
    #[arg(long)]
    pub mcp: bool,
    #[arg(long = "agents-md")]
    pub agents_md: bool,
    #[arg(long, value_name = "shell")]
    pub completions: Option<String>,
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
}

impl SetupReport {
    fn item() -> Self {
        Self {
            items: 1,
            failures: 0,
        }
    }

    fn failure() -> Self {
        Self {
            items: 0,
            failures: 1,
        }
    }

    fn merge(&mut self, other: Self) {
        self.items += other.items;
        self.failures += other.failures;
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
    // Reject an unknown runtime on the spot: silently installing nothing is the most expensive
    // class of configuration bug — the user believes the integration is in place and finds out
    // only at the next session.
    if let Some(rt) = args.runtime.as_deref()
        && !RUNTIMES.contains(&rt)
    {
        ui::error(&format!(
            "unknown runtime `{rt}` (expected one of: {})",
            RUNTIMES.join(" / ")
        ));
        return Ok(ExitCode::Usage);
    }

    // No flag = install everything (except completions — that one takes a shell argument).
    let all =
        !args.hooks && !args.skill && !args.mcp && !args.agents_md && args.completions.is_none();
    let (hooks, skill, mcp, am) = (
        args.hooks || all,
        args.skill || all,
        args.mcp || all,
        args.agents_md || all,
    );

    let rt = args.runtime.as_deref();
    let mut report = SetupReport::default();
    if hooks {
        report.merge(install_hooks(rt));
    }
    if skill {
        report.merge(install_skill(rt));
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
        return Ok(ExitCode::Failure);
    }
    if report.items > 0 {
        ui::success(&format!(
            "All set ({} items). Idempotent: re-running writes nothing twice.",
            report.items
        ));
    } else {
        println!("Nothing to install.");
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

/// Hooks agit no longer installs and must retire. **Matched by argv, without the exe path** —
/// once the install location moves (npm's `/usr/local/bin` ↔ `setup.sh`'s `~/.local/bin`),
/// matching on the whole command string retires nothing, and one Stop settles twice.
///
/// `commands/hooks.rs` carries the reason `hooks settle` replaces `commit --from-hook`: the latter
/// does not read stdin and can only locate the branch through a possibly stale `AGIT_SESSION`.
const RETIRED_HOOKS: &[(&str, &str)] = &[("Stop", "commit --from-hook")];

/// The command string of one hook. The exe path can contain spaces, but Claude Code hands
/// `command` to a shell, so this stays a bare join. Quoting is a separate concern — adding it
/// here changes the semantics.
fn hook_command(exe: &str, argv: &[&str]) -> String {
    format!("{exe} {}", argv.join(" "))
}

/// Claude Code hooks (~/.claude/settings.json). Idempotent: the same command is never written
/// twice.
fn install_hooks(runtime: Option<&str>) -> SetupReport {
    if !wants(runtime, "claude-code") {
        return SetupReport::default();
    }
    let Some(settings) = home().map(|h| h.join(".claude/settings.json")) else {
        ui::warning("cannot install Claude Code hooks: HOME is not set");
        return SetupReport::failure();
    };
    let exe = exe_str();

    let mut doc: serde_json::Value = std::fs::read_to_string(&settings)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let hooks = doc
        .as_object_mut()
        .expect("settings.json must be an object")
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));

    let mut added = 0;
    for (event, argv) in RETIRED_HOOKS {
        added += retire_hook(hooks, event, argv);
    }
    for (event, argv) in HOOKS {
        added += upsert_hook(hooks, event, &hook_command(&exe, argv));
    }
    if added > 0 {
        if write_json(&settings, &doc).is_err() {
            ui::warning(&format!("failed to write {}", settings.display()));
            return SetupReport::failure();
        }
        println!(
            "  {} hooks → {}",
            ui::ok(ui::theme::symbols().check),
            ui::tilde(&settings)
        );
    } else {
        println!("  {} hooks already in place", ui::dim("·"));
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
    let Some(rest) = command.trim().strip_suffix(argv) else {
        return false;
    };
    let exe = rest.trim_end();
    // A real separator must sit between the executable and the arguments; otherwise a matching
    // tail like `.../notagit-commit --from-hook` counts too.
    if exe.len() == rest.len() {
        return false;
    }
    // It must be a single token. In `some earlier command && /bin/agit commit --from-hook` ours
    // is one link of the chain, and deleting the entry takes the user's first half with it.
    if exe.split_whitespace().count() != 1 {
        return false;
    }
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe);
    matches!(name.trim_matches(['"', '\'']), "agit" | "agit.exe")
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

/// A command already present under the same hook event is left untouched.
fn upsert_hook(hooks: &mut serde_json::Value, event: &str, cmd: &str) -> usize {
    let arr = hooks
        .as_object_mut()
        .expect("hooks must be an object")
        .entry(event)
        .or_insert_with(|| serde_json::json!([]));
    let arr = arr.as_array_mut().expect("hook event must be an array");
    for group in arr.iter() {
        if group.to_string().contains(cmd) {
            return 0;
        }
    }
    arr.push(serde_json::json!({
        "hooks": [{"type": "command", "command": cmd}]
    }));
    1
}

// ── skill / instruction install ───────────────────────────────────────

/// The agit skill lands in each runtime's native Skill directory. Beyond how many files were
/// written, this also reports whether the install is complete.
fn install_skill(runtime: Option<&str>) -> SetupReport {
    let mut report = SetupReport::default();
    for name in ["claude-code", "codex", "opencode", "cursor"] {
        if !wants(runtime, name) {
            continue;
        }
        let Some(dir) = skill_path(name) else {
            ui::warning(&format!("cannot install {name} Skill: HOME is not set"));
            report.merge(SetupReport::failure());
            continue;
        };
        let skill_report = install_skill_dir(&dir, name);
        report.merge(skill_report);
        // Older releases placed the full entrypoint in an AGENTS.md marker. Remove
        // only that self-owned block after the native Skill is present; user content
        // and the separate short --agents-md block are preserved. The closure is
        // deliberately gated on a complete bundle result so a failed install cannot
        // destroy the only working copy of the Skill.
        merge_legacy_cleanup(&mut report, skill_report, || {
            remove_legacy_inline_skill(name)
        });
    }
    report
}

/// Resolve the native global Skill directory for a runtime.
pub(crate) fn skill_path(runtime: &str) -> Option<PathBuf> {
    match runtime {
        "codex" => codex_home_path().map(|h| h.join("skills/agit")),
        "claude-code" | "opencode" | "cursor" => home().map(|h| skill_path_for_home(&h, runtime)),
        _ => None,
    }
}

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
                        println!(
                            "  {} stale skill reference removed → {}",
                            ui::ok(ui::theme::symbols().check),
                            ui::tilde(&path)
                        );
                        report.merge(SetupReport::item());
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
    let Ok(existing) = std::fs::read_to_string(path) else {
        return SetupReport::default();
    };
    let Some(begin) = existing.find(begin_marker) else {
        return SetupReport::default();
    };
    let body_start = begin + begin_marker.len();
    let Some(relative_end) = existing[body_start..].find(end_marker) else {
        return SetupReport::default();
    };
    let mut end = body_start + relative_end + end_marker.len();
    // Markers are line-oriented. Consume the newline immediately following the
    // owned block so removing it does not leave a blank line in user content.
    if existing[end..].starts_with('\n') {
        end += 1;
    }
    let body = &existing[body_start..body_start + relative_end];
    if !body.contains("<!-- agit:skill-version:") {
        return SetupReport::default();
    }

    let mut new = String::with_capacity(existing.len() - (end - begin));
    new.push_str(&existing[..begin]);
    new.push_str(&existing[end..]);
    if write_append_overwrite(path, &new).is_err() {
        ui::warning(&format!("{label} removal failed: {}", path.display()));
        return SetupReport::failure();
    }
    println!(
        "  {} {label} removed → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&PathBuf::from(path))
    );
    SetupReport::item()
}

// ── MCP registration ──────────────────────────────────────────────────

/// MCP registration writes each runtime's native configuration. All of it is idempotent, and a
/// failed configuration write propagates.
fn register_mcp(runtime: Option<&str>) -> SetupReport {
    let exe = exe_str();
    let mut report = SetupReport::default();

    if wants(runtime, "claude-code") {
        let ok = std::process::Command::new("claude")
            .args(["mcp", "add", "agit", "--", &exe, "mcp"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!(
                "  {} mcp → claude mcp add agit",
                ui::ok(ui::theme::symbols().check)
            );
        } else {
            println!(
                "  {} mcp: register manually with `claude mcp add agit -- {exe} mcp`",
                ui::dim("·")
            );
        }
        report.merge(SetupReport::item());
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

    report
}

/// The codex MCP entry lives in config.toml. A toml-editing dependency cannot be pulled in, so
/// this is a minimal hand-written upsert: the section name `[mcp_servers.agit]` being present
/// counts as registered (nobody else's content is touched), and otherwise a section is appended at
/// the end of the file. The section body is ours, so checking the section name before appending
/// again is enough to be idempotent.
fn upsert_codex_mcp(path: &Path, exe: &str) -> SetupReport {
    let block = format!(
        "\n[mcp_servers.agit]\n# session version control tools: search / show / view / status / commit\ncommand = \"{exe}\"\nargs = [\"mcp\"]\n"
    );
    upsert_toml_section(path, "[mcp_servers.agit]", &block)
}

fn upsert_toml_section(path: &Path, section: &str, block: &str) -> SetupReport {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(section) {
        println!(
            "  {} mcp({}) already in place",
            ui::dim("·"),
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        return SetupReport::item();
    }
    if write_append(path, block).is_err() {
        ui::warning(&format!("failed to write {}", path.display()));
        return SetupReport::failure();
    }
    println!(
        "  {} mcp → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&std::path::PathBuf::from(path))
    );
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
    let mut doc: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let root = doc
        .as_object_mut()
        .expect("config file must be an object")
        .entry(root_key)
        .or_insert_with(|| serde_json::json!({}));
    let map = root.as_object_mut().expect("mcp config must be an object");
    let label = path.display().to_string();
    if map.contains_key(name) {
        println!(
            "  {} mcp({}) already in place",
            ui::dim("·"),
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        return SetupReport::item();
    }
    map.insert(name.to_string(), entry);
    if write_json(path, &doc).is_err() {
        ui::warning(&format!("failed to write {label}"));
        return SetupReport::failure();
    }
    println!(
        "  {} mcp → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&std::path::PathBuf::from(path))
    );
    SetupReport::item()
}

// ── project AGENTS.md ─────────────────────────────────────────────────

/// AGENTS.md: a marked, idempotent section.
fn append_agents_md() -> SetupReport {
    let Some(path) = project_agents_path() else {
        ui::warning("cannot locate project AGENTS.md: current directory is unavailable");
        return SetupReport::failure();
    };
    upsert_marked_block(&path, include_str!("setup_agents_section.md"), "AGENTS.md")
}

pub(crate) fn project_agents_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(project_agents_path_for(&cwd))
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
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn exe_str() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "agit".into())
}

/// Write only when the content differs (what idempotent means here: already current skips
/// quietly).
fn write_if_changed(path: &Path, body: &str, label: &str) -> SetupReport {
    if std::fs::read_to_string(path).ok().as_deref() == Some(body) {
        println!("  {} {label} is up to date", ui::dim("·"));
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
    println!(
        "  {} {label} → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&PathBuf::from(path))
    );
    SetupReport::item()
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
        println!(
            "  {} {label} block already present (idempotent)",
            ui::dim("·")
        );
        return SetupReport::item();
    }
    if write_append_overwrite(path, &new).is_err() {
        ui::warning(&format!("{label} write failed: {}", path.display()));
        return SetupReport::failure();
    }
    println!(
        "  {} {label} → {}",
        ui::ok(ui::theme::symbols().check),
        ui::tilde(&PathBuf::from(path))
    );
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

fn write_append(path: &Path, block: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(block.as_bytes())
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

    #[test]
    fn runtime_filter_accepts_none_all_and_exact_match() {
        assert!(wants(None, "codex"));
        assert!(wants(Some("all"), "cursor"));
        assert!(wants(Some("cursor"), "cursor"));
        assert!(!wants(Some("cursor"), "codex"));
        assert!(!wants(Some("codex"), "all")); // a runtime name is no synonym for "all"
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
        for (event, argv) in HOOKS {
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
            hook_command("/usr/bin/agit", HOOKS[0].1),
            "/usr/bin/agit hooks ingest"
        );
    }

    /// What lands in settings.json is the same data as [`HOOKS`] — otherwise the test above only
    /// examines itself.
    #[test]
    fn the_settings_file_gets_exactly_those_commands() {
        let mut hooks = serde_json::json!({});
        for (event, argv) in HOOKS {
            assert_eq!(
                upsert_hook(&mut hooks, event, &hook_command("agit", argv)),
                1
            );
        }
        let text = hooks.to_string();
        assert!(text.contains("agit hooks ingest"), "{text}");
        assert!(text.contains("agit hooks settle"), "{text}");
        // Idempotent: the same command is never written twice.
        for (event, argv) in HOOKS {
            assert_eq!(
                upsert_hook(&mut hooks, event, &hook_command("agit", argv)),
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
            upsert_hook(&mut hooks, event, &hook_command("agit", argv));
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
            // the arguments are incomplete / not at the end.
            "/usr/local/bin/agit commit --from-hook --extra",
            "/usr/local/bin/agit hooks settle",
        ] {
            assert!(
                !is_our_hook(theirs, ARGV),
                "not mistaken for our own: {theirs}"
            );
        }
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
        assert!(skill.contains("agit import <session-id> --repo <owner/repo> -b <branch>"));
        assert!(skill.contains("`agit import` links that existing transcript"));
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
    fn toml_section_upsert_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("agit-setup-toml-{}", std::process::id()));
        let p = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            upsert_toml_section(
                &p,
                "[mcp_servers.agit]",
                "\n[mcp_servers.agit]\ncommand = \"agit\"\n"
            )
            .items,
            1
        );
        assert_eq!(
            upsert_toml_section(
                &p,
                "[mcp_servers.agit]",
                "\n[mcp_servers.agit]\ncommand = \"changed\"\n"
            )
            .items,
            1
        );
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("[mcp_servers.agit]").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
