//! What keeps a turn from being saved where nobody sees agit's output. Pi's extension and
//! OpenCode's plugin run `agit hooks settle` detached and discard what it prints, so a settlement
//! that fails there is kept on this computer, under `hooks-seen/<runtime>-failures/<session id>`,
//! until that conversation's next settlement succeeds, and `agit status` says so. The first
//! failure of each kind in a conversation is also printed as the one line Pi's extension shows,
//! when the extension says it shows it. None of it goes to the Hub, to telemetry or into a
//! published record.

use crate::infra::config;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Whether `runtime`'s host discards what a settlement prints.
pub(crate) fn output_discarded(runtime: &str) -> bool {
    matches!(runtime, "pi" | "opencode")
}

/// Why a turn wasn't saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// Its save was refused by the rules a save follows ([`NotSaved`]).
    NotSaved,
    /// Something failed before the save was judged, as reading the folder's settings.
    Failed,
}

/// A save the rules refused, said as the reason the person reads.
#[derive(Debug, thiserror::Error)]
#[error("this turn isn't saved: {0}")]
pub(crate) struct NotSaved(pub String);

/// A settlement that failed, as kept on this computer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Failure {
    /// When, in Unix seconds.
    pub at: i64,
    pub kind: Kind,
    /// Why, on one line: the refusal's reason, or what failed.
    pub line: String,
    /// The folder the conversation ran in, as its runtime said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

impl Failure {
    /// The line Pi's extension shows: what happened, and where to see it again.
    pub(crate) fn notice(&self) -> String {
        match self.kind {
            Kind::NotSaved => format!(
                "agit: this turn isn't saved: {}; `agit status` here shows it again",
                self.line
            ),
            Kind::Failed => format!(
                "agit couldn't save this turn: {}; `agit status` here shows it again",
                self.line
            ),
        }
    }
}

/// How much of a failure's text is kept and shown.
const LINE_CHARS: usize = 300;

/// Where `session`'s record is kept under `hooks-seen/<runtime>-<kind>/`. `None` for an id that
/// is not a plain file name: an id naming a path records nothing.
fn path(directory: &str, runtime: &str, session: &str) -> Option<PathBuf> {
    let plain = !session.is_empty()
        && session.len() <= 128
        && !session.starts_with('.')
        && session
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    plain.then_some(())?;
    Some(
        config::agit_home()
            .ok()?
            .join("hooks-seen")
            .join(format!("{runtime}-{directory}"))
            .join(session),
    )
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Replaces the file at `path` whole, so a reader never sees half of it.
fn replace(path: &Path, value: &impl Serialize) -> Option<()> {
    use std::io::Write as _;
    let directory = path.parent()?;
    config::create_state_dir(directory).ok()?;
    let mut file = tempfile::NamedTempFile::new_in(directory).ok()?;
    file.write_all(&serde_json::to_vec(value).ok()?).ok()?;
    file.persist(path).ok()?;
    Some(())
}

/// The conversations of `runtime`s whose last settlement failed and that ran in `folder` or
/// below it, with their failures, newest first.
pub(crate) fn in_folder(folder: &Path) -> Vec<(&'static str, String, Failure)> {
    let Ok(home) = config::agit_home() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for runtime in ["pi", "opencode"] {
        let Ok(entries) =
            std::fs::read_dir(home.join("hooks-seen").join(format!("{runtime}-failures")))
        else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(session) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if path("failures", runtime, &session).is_none() {
                continue;
            }
            let Some(failure) = read::<Failure>(&entry.path()) else {
                continue;
            };
            if failure.cwd.as_deref().is_some_and(|cwd| {
                cwd.canonicalize()
                    .unwrap_or_else(|_| cwd.to_owned())
                    .starts_with(folder)
            }) {
                found.push((runtime, session, failure));
            }
        }
    }
    found.sort_by_key(|(_, _, failure)| std::cmp::Reverse(failure.at));
    found
}

/// Records how a settlement of `runtime`'s conversation `session` went: a success removes what a
/// failure kept, and a failure is kept. Returns the line to show when `shown`, the caller showing
/// it, and the failure is of a kind not shown for this conversation before; what was shown stays
/// recorded past later successes, so each kind is shown once in a conversation.
pub(crate) fn settled(
    runtime: &str,
    session: &str,
    cwd: Option<&Path>,
    result: Result<(), &anyhow::Error>,
    shown: bool,
) -> Option<String> {
    let record = path("failures", runtime, session)?;
    let error = match result {
        Ok(()) => {
            let _ = std::fs::remove_file(&record);
            return None;
        }
        Err(error) => error,
    };
    let (kind, said) = match error.downcast_ref::<NotSaved>() {
        Some(NotSaved(reason)) => (Kind::NotSaved, reason.clone()),
        None => (Kind::Failed, format!("{error:#}")),
    };
    let line: String = said
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(LINE_CHARS)
        .collect();
    let failure = Failure {
        at: super::upload_outcome::now(),
        kind,
        line,
        cwd: cwd.map(Path::to_owned),
    };
    replace(&record, &failure)?;
    if !shown {
        return None;
    }
    let told = path("failures-told", runtime, session)?;
    let mut kinds: Vec<Kind> = read(&told).unwrap_or_default();
    if kinds.contains(&kind) {
        return None;
    }
    kinds.push(kind);
    replace(&told, &kinds)?;
    Some(failure.notice())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed settlement is kept until the conversation's next one succeeds, listed for the
    /// folder it ran in, and its line is handed out once for each kind of failure in a
    /// conversation, to a caller that shows it. One handed out at every failure repeats it at
    /// every run; one counted as told where nothing showed it never reaches the person; one
    /// forgotten at a success tells the same failure again. An id naming a path records nothing.
    #[test]
    fn a_failure_is_kept_until_a_settlement_succeeds_and_told_once_for_each_kind() {
        if crate::rc::in_isolated_test(
            "commands::settle_failures::tests::a_failure_is_kept_until_a_settlement_succeeds_and_told_once_for_each_kind",
        ) {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        // SAFETY: `in_isolated_test` runs this test alone in a process of its own.
        unsafe { std::env::set_var("AGIT_HOME", home.path().join("agit")) };
        let folder = home.path().join("work");
        std::fs::create_dir_all(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        let refused = anyhow::Error::from(NotSaved("me/gone isn't on this computer".into()));
        let broken = anyhow::anyhow!("cannot read\nthe folder's settings");
        let settle = |result: Result<(), &anyhow::Error>, shown: bool| {
            settled("pi", "s1", Some(&folder), result, shown)
        };
        let kept = || {
            in_folder(&folder)
                .into_iter()
                .map(|(runtime, session, failure)| (runtime, session, failure.kind, failure.line))
                .collect::<Vec<_>>()
        };

        assert_eq!(settle(Err(&refused), false), None);
        let refusal = (
            "pi",
            "s1".to_owned(),
            Kind::NotSaved,
            "me/gone isn't on this computer".to_owned(),
        );
        assert_eq!(kept(), [refusal.clone()]);
        assert_eq!(
            settle(Err(&refused), true).as_deref(),
            Some(
                "agit: this turn isn't saved: me/gone isn't on this computer; `agit status` here shows it again"
            ),
            "a failure not shown before was not told where it shows"
        );
        assert_eq!(
            settle(Err(&refused), true),
            None,
            "the same kind was told twice"
        );
        assert_eq!(
            settle(Err(&broken), true).as_deref(),
            Some(
                "agit couldn't save this turn: cannot read the folder's settings; `agit status` here shows it again"
            )
        );
        assert_eq!(kept()[0].2, Kind::Failed);
        assert_eq!(settle(Ok(()), true), None);
        assert!(kept().is_empty(), "a success left the failure listed");
        assert_eq!(
            settle(Err(&refused), true),
            None,
            "a kind told in this conversation was told again after a success"
        );
        assert_eq!(kept(), [refusal]);
        assert!(in_folder(&home.path().join("elsewhere")).is_empty());

        assert_eq!(
            settled("pi", "../s1", Some(&folder), Err(&refused), true),
            None
        );
        assert!(!home.path().join("agit/hooks-seen/s1").exists());
    }
}
