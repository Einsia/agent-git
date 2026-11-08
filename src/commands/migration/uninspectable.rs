//! A repository the read-only startup check could not inspect, named for the person.
//!
//! The check is there so a read-only command never reads a repository with an interrupted
//! recovery as settled. One that Git can't inspect could be such a repository, so it is never
//! read: a command that reads it stops, and any other leaves it out for the rest of the process
//! (see [`crate::infra::config::leave_out`]) and says so. What Git said can name paths on this
//! computer: it is printed to the person, never recorded.

use crate::domain::repo::StorageUnknown;
use crate::infra::config;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// How much of Git's line is shown.
const SAID_CHARS: usize = 200;

/// What a read-only command reads, asked only once a repository can't be inspected.
pub enum Reads {
    /// Every repository: any one that can't be inspected stops the command.
    Every,
    /// These repositories (`owner/name`): one of them stops the command, any other is left out.
    Only(BTreeSet<String>),
    /// `agit doctor`: nothing stops it; each one is a finding.
    Findings,
}

#[derive(Debug, Clone)]
pub struct Uninspectable {
    /// `owner/name`.
    pub repository: String,
    root: PathBuf,
    /// The linked worktree it failed in; `None` for the repository's own checkout.
    worktree: Option<PathBuf>,
    /// Git's first line, cut and made printable, or how it exited.
    said: String,
    /// Whether Git couldn't say where the storage is, rather than failing some other way.
    storage: bool,
}

impl Uninspectable {
    /// `error`, from inspecting the repository at `root` (or its worktree), named.
    pub(super) fn new(root: &Path, worktree: Option<&Path>, error: &anyhow::Error) -> Self {
        let storage = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StorageUnknown>());
        let said = match storage {
            Some(unknown) => match (unknown.stderr_line(), unknown.status()) {
                (Some(line), _) => shown(line),
                (None, Some(code)) => format!("it exited with status {code}"),
                (None, None) => "it stopped without an exit status".to_owned(),
            },
            None => shown(
                format!("{error:#}")
                    .lines()
                    .next()
                    .unwrap_or("the inspection failed"),
            ),
        };
        Self {
            repository: repository_of(root),
            root: root.to_path_buf(),
            worktree: worktree
                .filter(|worktree| !same_place(worktree, root))
                .map(Path::to_path_buf),
            said,
            storage: storage.is_some(),
        }
    }

    /// What Git couldn't do. Only the storage question can leave the repository unnamed, on a
    /// line that already sits under its name.
    fn what(&self, named: bool) -> String {
        match (self.storage, named) {
            (true, true) => format!("Git couldn't tell where {}'s storage is", self.repository),
            (true, false) => "Git couldn't tell where its storage is".to_owned(),
            (false, _) => format!("Git couldn't inspect {}", self.repository),
        }
    }

    fn located(&self, named: bool) -> String {
        let mut line = self.what(named);
        if let Some(worktree) = &self.worktree {
            line.push_str(&format!(", in its worktree {}", crate::ui::tilde(worktree)));
        }
        line.push_str(": ");
        line.push_str(&self.said);
        line
    }

    fn look(&self) -> String {
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        let path = self.worktree.as_deref().unwrap_or(&self.root);
        format!(
            "see Git's answer in full: `git -C {} rev-parse --git-common-dir`",
            quote_arg(&path.to_string_lossy())
        )
    }

    /// The linked worktree it failed in, if it wasn't the repository's own checkout.
    pub fn worktree(&self) -> Option<&Path> {
        self.worktree.as_deref()
    }

    /// The next step under the line.
    pub fn step(&self) -> String {
        format!("{}; `agit doctor` checks the repository", self.look())
    }

    /// The note when the command goes on without it.
    pub(super) fn left_out_note(&self) -> String {
        format!(
            "{}; nothing in this folder goes to {}, so this leaves it out",
            self.located(true),
            self.repository
        )
    }

    /// `agit doctor`'s finding, on the repository's own row.
    pub fn finding(&self) -> String {
        format!("{}; {}", self.located(false), self.look())
    }

    /// What a later lookup of it in the same process is refused with.
    fn refusal(&self) -> String {
        let what = if self.storage {
            "Git couldn't tell where its storage is"
        } else {
            "Git couldn't inspect it"
        };
        format!("{} is left out: {what}", self.repository)
    }

    /// Leave it out of the rest of this process, and keep it for doctor's findings.
    pub fn leave_out(&self) {
        config::leave_out(self.root.clone(), self.refusal());
        let mut held = HELD
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !held.iter().any(|known| known.root == self.root) {
            held.push(self.clone());
        }
    }
}

impl std::fmt::Display for Uninspectable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.located(true))
    }
}

impl std::error::Error for Uninspectable {}

static HELD: std::sync::Mutex<Vec<Uninspectable>> = std::sync::Mutex::new(Vec::new());

/// The repositories this process left out, in the order they were found.
pub fn left_out() -> Vec<Uninspectable> {
    HELD.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The step for an error that names a repository Git can't inspect.
pub fn step(error: &anyhow::Error) -> Option<String> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Uninspectable>())
        .map(Uninspectable::step)
}

/// `owner/name` from a repository's place under the repositories directory, or its path.
fn repository_of(root: &Path) -> String {
    let named = config::repos_dir()
        .ok()
        .and_then(|repos| root.strip_prefix(repos).ok().map(Path::to_path_buf))
        .filter(|relative| relative.components().count() == 2);
    match named {
        Some(relative) => relative.to_string_lossy().replace('\\', "/"),
        None => crate::ui::tilde(root),
    }
}

fn same_place(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// Someone else's line, made printable and cut.
fn shown(line: &str) -> String {
    let printable = crate::ui::printable(line.trim());
    if printable.chars().count() <= SAID_CHARS {
        return printable;
    }
    let mut cut: String = printable.chars().take(SAID_CHARS).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_git_said_is_shown_cut_and_inert() {
        let long = format!("fatal: \u{1b}]52;c;cGF5bG9hZA==\u{7}{}", "x".repeat(400));
        let shown = shown(&long);
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert_eq!(shown.chars().count(), SAID_CHARS + 1, "{shown:?}");
        assert!(shown.ends_with('…'));
        assert_eq!(super::shown("  fatal: short  "), "fatal: short");
    }
}
