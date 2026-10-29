//! The Claude Code program the daemon launches.
//!
//! A daemon started by a service manager, a desktop app or a scheduler often runs with a minimal
//! PATH, while Claude Code's installers put the program in per-user directories that only an
//! interactive shell adds. The program is resolved again on every launch and capability check, so
//! an installation made after the daemon started is picked up without restarting it.

use std::path::{Path, PathBuf};

const PROGRAM: &str = "claude";

/// The program, and the directory a child must find beside it when it is not on PATH.
pub(crate) struct Program {
    pub path: PathBuf,
    pub outside_path: bool,
}

impl Program {
    /// Environment the child needs: an npm-installed program is a script that runs `node` from
    /// its own directory, which a minimal PATH does not include.
    pub(crate) fn env(&self) -> Option<(String, String)> {
        if !self.outside_path {
            return None;
        }
        let directory = self.path.parent()?;
        let mut paths = vec![directory.to_path_buf()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let joined = std::env::join_paths(paths).ok()?;
        Some(("PATH".into(), joined.to_string_lossy().into_owned()))
    }
}

/// PATH first, then the directories Claude Code's installers and Node package managers use.
pub(crate) fn resolve() -> Option<Program> {
    // A test's stand-in program is what a launch on its thread spawns, so it is also what this
    // resolves to; otherwise a launch test on a machine without Claude Code stops here.
    #[cfg(all(test, unix))]
    if let Some(program) = crate::rc::harness::proc::harness_program_override() {
        return Some(Program {
            path: program.into(),
            outside_path: false,
        });
    }
    resolve_from(
        crate::adapter::which(PROGRAM),
        crate::infra::config::user_home().as_deref(),
    )
}

fn resolve_from(on_path: Option<PathBuf>, home: Option<&Path>) -> Option<Program> {
    if let Some(path) = on_path {
        return Some(Program {
            path,
            outside_path: false,
        });
    }
    candidates(home?)
        .into_iter()
        .find(|path| executable(path))
        .map(|path| Program {
            path,
            outside_path: true,
        })
}

fn candidates(home: &Path) -> Vec<PathBuf> {
    let variable = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let mut directories: Vec<PathBuf> = vec![
        // The native installer, and the per-user npm install it can migrate from.
        home.join(".local/bin"),
        home.join(".claude/local"),
    ];
    directories.extend(variable("PNPM_HOME"));
    directories.extend([home.join("Library/pnpm"), home.join(".local/share/pnpm")]);
    directories.extend(variable("BUN_INSTALL").map(|root| root.join("bin")));
    directories.push(home.join(".bun/bin"));
    directories.extend(variable("VOLTA_HOME").map(|root| root.join("bin")));
    directories.push(home.join(".volta/bin"));
    directories.extend(variable("NPM_CONFIG_PREFIX").map(|root| root.join("bin")));
    directories.push(home.join(".npm-global/bin"));
    // nvm keeps one global prefix per Node version; prefer the newest-named one.
    if let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path().join("bin")))
            .collect();
        versions.sort();
        directories.extend(versions.into_iter().rev());
    }
    directories.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/home/linuxbrew/.linuxbrew/bin",
        ]
        .map(PathBuf::from),
    );
    directories
        .into_iter()
        .map(|directory| directory.join(PROGRAM))
        .collect()
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A daemon whose PATH lacks Claude Code still finds the per-user installation, and hands
    /// the child that directory so a script-based install can find `node` beside it.
    #[test]
    fn a_per_user_installation_is_found_without_the_shell_path() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join(PROGRAM);
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let found = resolve_from(None, Some(home.path()))
            .expect("the native installer's directory is searched");
        assert_eq!(found.path, program);
        let (name, value) = found.env().unwrap();
        assert_eq!(name, "PATH");
        assert!(value.starts_with(bin.to_str().unwrap()));

        let on_path = resolve_from(Some(PathBuf::from("/usr/bin/claude")), Some(home.path()));
        assert!(
            on_path.unwrap().env().is_none(),
            "a program on PATH changes nothing"
        );
    }
}
