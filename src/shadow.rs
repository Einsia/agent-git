//! `agit shadow` — route `git` through `agit` in your interactive shell, cross-platform.
//!
//! A "git shadow" is a shell-level `git` function that forwards to `agit` (so every git command also
//! versions your agent context), while the verbs where agit intentionally differs from git — global
//! flags, `init`, `clone`, `version`, `help` — fall straight through to real git. `command git …`
//! always bypasses the shadow.
//!
//! Cross-platform: it knows bash, zsh, fish, and PowerShell, writes an idempotent, reversible block to
//! the right profile, and can report or remove itself.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

const BEGIN: &str = "# >>> agit shadow >>>";
const END: &str = "# <<< agit shadow <<<";

/// Every shell whose profile the shadow can be written to.
const ALL: [Shell; 4] = [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

impl Shell {
    pub fn label(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
            Shell::PowerShell => "powershell",
        }
    }

    pub fn parse(s: &str) -> Option<Shell> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bash" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "fish" => Some(Shell::Fish),
            "powershell" | "pwsh" | "ps" => Some(Shell::PowerShell),
            _ => None,
        }
    }
}

/// Detect the user's shell: `$SHELL` on Unix, PowerShell on Windows.
fn detect() -> Option<Shell> {
    if cfg!(windows) {
        return Some(Shell::PowerShell);
    }
    let sh = std::env::var("SHELL").ok()?;
    let base = sh.rsplit(['/', '\\']).next().unwrap_or("");
    // zsh before bash: many login shells symlink; match the most specific name present.
    for (needle, shell) in [("zsh", Shell::Zsh), ("fish", Shell::Fish), ("bash", Shell::Bash)] {
        if base.contains(needle) {
            return Some(shell);
        }
    }
    None
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot find your home directory ($HOME / $USERPROFILE)")
}

/// The profile file a shell reads on an interactive session.
fn profile_path(shell: Shell) -> Result<PathBuf> {
    let home = home()?;
    Ok(match shell {
        Shell::Bash => home.join(".bashrc"),
        Shell::Zsh => home.join(".zshrc"),
        Shell::Fish => home.join(".config").join("fish").join("config.fish"),
        // The conventional default $PROFILE for PowerShell 6+ on Windows. (pwsh's own $PROFILE is
        // authoritative if the user relocated Documents, but this is the standard location.)
        Shell::PowerShell => home
            .join("Documents")
            .join("PowerShell")
            .join("Microsoft.PowerShell_profile.ps1"),
    })
}

/// The shadow block for a shell. `git` and `agit` are called by name (resolved on PATH), so a
/// reinstall is only needed if agit leaves PATH — not every time it moves.
fn snippet(shell: Shell) -> String {
    match shell {
        Shell::Bash | Shell::Zsh => format!(
            "{BEGIN}\n\
             git() {{\n\
             \x20 case \"${{1:-}}\" in\n\
             \x20   -*|init|clone|version|help|\"\") command git \"$@\" ;;\n\
             \x20   *) agit \"$@\" ;;\n\
             \x20 esac\n\
             }}\n\
             {END}"
        ),
        Shell::Fish => format!(
            "{BEGIN}\n\
             function git\n\
             \x20   switch $argv[1]\n\
             \x20       case '-*' init clone version help ''\n\
             \x20           command git $argv\n\
             \x20       case '*'\n\
             \x20           agit $argv\n\
             \x20   end\n\
             end\n\
             {END}"
        ),
        Shell::PowerShell => format!(
            "{BEGIN}\n\
             function git {{\n\
             \x20   if ($args.Count -eq 0 -or $args[0] -like '-*' -or $args[0] -in @('init','clone','version','help')) {{\n\
             \x20       & (Get-Command git -CommandType Application | Select-Object -First 1).Source @args\n\
             \x20   }} else {{\n\
             \x20       agit @args\n\
             \x20   }}\n\
             }}\n\
             {END}"
        ),
    }
}

/// Strip any existing agit-shadow block (between the markers) from profile `content`, and trim the
/// blank lines it leaves behind. Pure — the unit of behavior the tests pin.
fn without_block(content: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skipping = false;
    for line in content.lines() {
        let t = line.trim();
        if t == BEGIN {
            skipping = true;
            continue;
        }
        if skipping {
            if t == END {
                skipping = false;
            }
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

/// Profile content with exactly one current shadow block: the old one (if any) replaced, appended
/// after the existing content.
fn with_block(content: &str, snippet: &str) -> String {
    let base = without_block(content);
    if base.is_empty() {
        format!("{snippet}\n")
    } else {
        format!("{base}\n\n{snippet}\n")
    }
}

fn on_path(cmd: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&paths).any(|dir| {
        dir.join(cmd).is_file()
            || (cfg!(windows)
                && (dir.join(format!("{cmd}.exe")).is_file() || dir.join(format!("{cmd}.cmd")).is_file()))
    })
}

/// `agit shadow install [--shell <shell>]` — write the shadow block into the shell's profile.
pub fn install(shell: Option<Shell>) -> Result<i32> {
    let shell = shell
        .or_else(detect)
        .context("could not detect your shell: pass --shell bash|zsh|fish|powershell")?;
    let path = profile_path(shell)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let had = existing.contains(BEGIN);
    std::fs::write(&path, with_block(&existing, &snippet(shell)))
        .with_context(|| format!("cannot write {}", path.display()))?;

    println!(
        "{} the git shadow ({}) in {}",
        if had { "Updated" } else { "Installed" },
        shell.label(),
        path.display()
    );
    if !on_path("agit") {
        eprintln!("  note: `agit` is not on your PATH: the shadow calls it by name, so add it first.");
    }
    println!("Start a new shell (or re-source the profile). `command git …` always runs pure git.");
    Ok(0)
}

/// `agit shadow uninstall [--shell <shell>]` — remove the block. With no `--shell`, clean every
/// profile that has one, so a shell switch never strands a shadow.
pub fn uninstall(shell: Option<Shell>) -> Result<i32> {
    let shells: Vec<Shell> = shell.map(|s| vec![s]).unwrap_or_else(|| ALL.to_vec());
    let mut removed = 0;
    for s in shells {
        let Ok(path) = profile_path(s) else { continue };
        let Ok(existing) = std::fs::read_to_string(&path) else { continue };
        if !existing.contains(BEGIN) {
            continue;
        }
        let cleaned = without_block(&existing);
        let cleaned = if cleaned.is_empty() { String::new() } else { format!("{cleaned}\n") };
        std::fs::write(&path, cleaned).with_context(|| format!("cannot write {}", path.display()))?;
        println!("Removed the git shadow ({}) from {}", s.label(), path.display());
        removed += 1;
    }
    if removed == 0 {
        println!("No git shadow found to remove.");
    }
    Ok(0)
}

/// The shells whose profile currently carries the shadow block: `(label, profile path)`. The read-only
/// core `status()` prints from, exposed so the debug bundle can report shadow state without spawning a
/// second process or duplicating the profile-scan.
pub fn status_lines() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for s in ALL {
        let Ok(path) = profile_path(s) else { continue };
        if std::fs::read_to_string(&path).map(|c| c.contains(BEGIN)).unwrap_or(false) {
            out.push((s.label(), path.display().to_string()));
        }
    }
    out
}

/// `agit shadow status` — where the shadow is installed, if anywhere.
pub fn status() -> Result<i32> {
    let installed = status_lines();
    for (label, path) in &installed {
        println!("● installed ({label}): {path}");
    }
    if installed.is_empty() {
        println!("git shadow not installed. Enable it with: agit shadow install");
    }
    Ok(0)
}

/// `agit shadow [install|uninstall|status]`, with an optional `--shell <shell>`.
pub fn run(args: &[String]) -> Result<i32> {
    let mut sub: Option<String> = None;
    let mut shell: Option<Shell> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--shell" if i + 1 < args.len() => {
                shell = Shell::parse(&args[i + 1]);
                if shell.is_none() {
                    bail!("unknown shell `{}`: use bash|zsh|fish|powershell", args[i + 1]);
                }
                i += 2;
            }
            s if !s.starts_with('-') && sub.is_none() => {
                sub = Some(s.to_string());
                i += 1;
            }
            _ => i += 1,
        }
    }
    match sub.as_deref() {
        None | Some("status") => status(),
        Some("install") | Some("on") => install(shell),
        Some("uninstall") | Some("remove") | Some("off") => uninstall(shell),
        Some(other) => {
            eprintln!("agit shadow: unknown subcommand `{other}` (install | uninstall | status)");
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_then_uninstall_leaves_the_profile_as_it_was() {
        let original = "export PATH=$HOME/bin:$PATH\nalias ll='ls -la'\n";
        let installed = with_block(original, &snippet(Shell::Bash));
        assert!(installed.contains(BEGIN) && installed.contains(END));
        assert!(installed.contains("command git"));
        // Removing the block restores the original content (modulo the trailing newline shape).
        assert_eq!(without_block(&installed).trim(), original.trim());
    }

    #[test]
    fn reinstalling_replaces_the_block_rather_than_duplicating_it() {
        let base = "# my rc\n";
        let once = with_block(base, &snippet(Shell::Zsh));
        let twice = with_block(&once, &snippet(Shell::Zsh));
        assert_eq!(once, twice, "a second install must not stack a second block");
        assert_eq!(twice.matches(BEGIN).count(), 1, "exactly one shadow block");
    }

    #[test]
    fn each_shell_gets_its_own_syntax() {
        assert!(snippet(Shell::Fish).contains("function git"));
        assert!(snippet(Shell::PowerShell).contains("Get-Command git -CommandType Application"));
        assert!(snippet(Shell::Bash).contains("case \"${1:-}\""));
    }

    #[test]
    fn shell_names_and_aliases_parse() {
        assert_eq!(Shell::parse("pwsh"), Some(Shell::PowerShell));
        assert_eq!(Shell::parse("ZSH"), Some(Shell::Zsh));
        assert_eq!(Shell::parse("tcsh"), None);
    }
}
