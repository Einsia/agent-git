//! `owner/name@branch` + immutable `agent_id` — where one RC session lands on this machine, in
//! its **parsed** form.
//!
//! # Why this is worth a type of its own
//!
//! All four pieces come from the hub (`session.start`'s `agent` / `expected_agent_id` /
//! `branch`), and the next thing they become is `~/.agit/repos/<owner>/<name>`,
//! `refs/heads/<branch>` and the identity fence written on the remote: an owner shaped like
//! `../..` is enough to walk that path out of agit's home directory, and a mismatched ID sends
//! commits to a new repo of the same name.
//!
//! Leaving it as "one `Option<String>` passed around supervisor / harness / roster, every user
//! calling `split_once('@')` for itself" does not work. The check on the `agit rc land` side is
//! only **one** consumer, and the latest one: the same raw string is injected into the harness's
//! `AGIT_SESSION` at launch — **before landing** — so the agent's own Stop hook runs
//! `agit commit --from-hook`, and that path goes `decode_session_env` → `parse_slug` →
//! `repo_dir`: the same escape, another process, and landing never gets to stop it. It is also
//! taken apart and fed to `repo_dir` by `settle_and_push`, and written into the roster to
//! survive a restart.
//!
//! Five consumers, one unvalidated string. Checking at the consumption point means every
//! consumption point has to remember, and missing one **has no symptom at all**. So the test
//! moves to the construction site: the fields are private and the only way in is
//! [`AgitSession::parse`] / [`AgitSession::new`]. Holding an `AgitSession` is the same as it
//! having passed the check; bypassing the check means constructing an instance first, and there
//! is no other constructor.
//!
//! # Why the test does not reuse `repo::valid_name`
//!
//! That test mixes in a rule that has nothing to do with path safety: a name may not start with
//! `agit-`. It exists so `agit clone x/y:Z` can tell a snapshot id from a branch name — a
//! reservation in the **branch namespace**, not path safety.
//!
//! The hub not only allows `agit-*`, it **produces them itself**: bind a directory named
//! `~/Code/agit-web`, `repo_name_from_path` takes the basename `agit-web`, and from then on
//! every `session.start` carries `agent: "alice/agit-web"`. Moving `valid_name` here upgrades a
//! naming policy the two sides already disagree about into "no session under that directory can
//! start at all".
//!
//! So this judges for itself, and only what it actually needs to judge: whether this segment is
//! safe to use as one path component.

use std::fmt;
use std::path::PathBuf;

/// Which branch of which repo a session settles to. **Construction is validation.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgitSession {
    owner: String,
    name: String,
    agent_id: String,
    branch: String,
}

/// Whether this segment is safe to use as **one** path component.
///
/// This judges path safety, not naming taste: empty, leading or trailing whitespace, `.`, `..`,
/// a separator or a NUL — each of those sends `PathBuf::join` somewhere else (an absolute
/// segment swallows the whole prefix as well). The character set matches the hub
/// (`agents::model::valid_name`), so a name the hub can produce is always accepted here — one
/// refused here means that repo can never open a session on this machine.
fn safe_segment(part: &str) -> crate::Result<()> {
    if part.is_empty() {
        anyhow::bail!("an empty path segment");
    }
    // **Refuse rather than trim.** Checking a normalized value while storing the raw one makes
    // "holding an AgitSession means it has been checked" true only of a string nobody uses:
    // `to_string()` is what gets injected into `AGIT_SESSION`, and downstream parses it again
    // with `parse_slug` (which trims the whole string), so the two sides see two different
    // directories.
    if part != part.trim() {
        anyhow::bail!("`{part}` has leading or trailing whitespace");
    }
    if part == "." || part == ".." {
        anyhow::bail!("`{part}` would climb out of the repos directory");
    }
    if !part
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("`{part}` may only contain letters, digits, `-` and `_`");
    }
    Ok(())
}

/// Whether a branch name is usable.
///
/// This test runs on daemon dispatch, which holds the global state lock and cannot spawn a
/// `git check-ref-format` synchronously per start/resume. Git's ref-format rules are pure string
/// rules; checking them in full here neither blocks a Tokio worker nor lets whether PATH can find
/// git decide whether the protocol accepts the same lineage.
pub fn valid_branch_name(branch: &str) -> bool {
    plausible_ref_name(branch)
}

/// The string rules of `git check-ref-format --branch`.
fn plausible_ref_name(b: &str) -> bool {
    !b.is_empty()
        && b == b.trim()
        // git reads a leading `-` as an option — `--upload-pack=...` is the dangerous shape.
        && !b.starts_with('-')
        && b != "HEAD"
        && b != "@"
        && !b.starts_with('/')
        && !b.ends_with('/')
        && !b.ends_with('.')
        && !b.contains("..")
        && !b.contains("//")
        && !b.contains("@{")
        && !b.contains(['~', '^', ':', '?', '*', '[', '\\', '\0'])
        && !b.chars().any(|c| c.is_control() || c == ' ')
        // Both rules bind every ref component, not just the last one.
        && b.split('/').all(|part| {
            !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock")
        })
}

impl AgitSession {
    /// The whole-string form `owner/name@branch` — the harness is given it as `AGIT_SESSION` and
    /// the roster stores it.
    ///
    /// Split on the **first** `@`: a branch name cannot hold an `@` (`plausible_ref_name` does
    /// not reject `@` itself, but the character set of the slug half excludes it, so an extra
    /// `@` only makes the slug check fail).
    pub fn parse(s: &str, expected_agent_id: &str) -> crate::Result<AgitSession> {
        let (slug, branch) = s
            .split_once('@')
            .ok_or_else(|| anyhow::anyhow!("`{s}` is not an <owner>/<name>@<branch> lineage"))?;
        AgitSession::new(slug, expected_agent_id, branch)
    }

    /// The three parts given separately (`session.start` / `session.resume` have this shape).
    pub fn new(slug: &str, expected_agent_id: &str, branch: &str) -> crate::Result<AgitSession> {
        let (owner, name) = slug
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("`{slug}` is not an <owner>/<name> slug"))?;
        for part in [owner, name] {
            safe_segment(part).map_err(|e| anyhow::anyhow!("unusable repo slug `{slug}`: {e}"))?;
        }
        if !valid_branch_name(branch) {
            anyhow::bail!("`{branch}` is not a usable branch name");
        }
        let agent_id = uuid::Uuid::parse_str(expected_agent_id.trim())
            .map_err(|_| {
                anyhow::anyhow!("`{expected_agent_id}` is not a usable immutable agent id")
            })?
            .to_string();
        Ok(AgitSession {
            owner: owner.to_string(),
            name: name.to_string(),
            agent_id,
            branch: branch.to_string(),
        })
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// `owner/name`. This is the positional argument of `agit push`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The repo path on this machine.
    ///
    /// **Only this knows how to build the directory out of a lineage** — callers no longer
    /// `split('/')` for themselves and feed the pieces to `repo_dir`, which is exactly the vine
    /// `../..` climbs out on.
    pub fn repo_dir(&self) -> crate::Result<PathBuf> {
        crate::infra::config::repo_dir(&self.owner, &self.name)
    }
}

impl fmt::Display for AgitSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}@{}", self.owner, self.name, self.branch)
    }
}

#[cfg(test)]
mod tests {
    use super::{AgitSession, valid_branch_name};

    const AGENT_ID: &str = "00000000-0000-0000-0000-000000000001";

    /// All four parts of a lineage come from the hub, and they become
    /// `~/.agit/repos/<owner>/<name>`, `refs/heads/<branch>` and the remote identity fence. This
    /// pins the whole point of the type: with no instance there is no path to build and no git
    /// command to start.
    #[test]
    fn a_lineage_that_could_leave_the_repos_directory_never_becomes_an_agit_session() {
        for bad in [
            "../..@b",
            "..@b",
            "a/..@b",
            "../a@b",
            "/etc/passwd@b",
            "a/b/c@b",
            ".@b",
            "a/.@b",
            "a@b",      // no `/`
            "@b",       // owner and name both empty
            "a/b@",     // empty branch
            "acme/api", // no `@`
            "a\\b/c@d",
            "a/b\0c@d",
        ] {
            assert!(
                AgitSession::parse(bad, AGENT_ID).is_err(),
                "`{bad}` must not parse"
            );
        }
    }

    /// What is checked and what is stored must be **the same byte string**.
    ///
    /// Checking a trimmed value while storing the raw one lets the whitespace-carrying copy in
    /// `AGIT_SESSION` resolve downstream (`parse_slug` trims the whole string) to a different
    /// directory — two halves of one settlement path pointing at two repos, both "checked".
    #[test]
    fn a_segment_with_surrounding_whitespace_is_refused_rather_than_quietly_trimmed() {
        for bad in [
            "acme/api @main",
            "acme /api@main",
            " acme/api@main",
            "acme/api\n@main",
        ] {
            assert!(
                AgitSession::parse(bad, AGENT_ID).is_err(),
                "`{bad}` must not parse"
            );
        }
    }

    /// **A repo name starting with `agit-` stays usable.**
    ///
    /// `repo::valid_name` refuses it, but that rule is there for `agit clone x/y:Z` to tell a
    /// snapshot id apart, and has nothing to do with path safety. The hub produces such names on
    /// its own: binding `~/Code/agit-web` gives `alice/agit-web`. Moving that rule here means no
    /// session under that directory can start at all.
    #[test]
    fn a_repo_whose_name_starts_with_agit_still_gets_a_lineage() {
        let l = AgitSession::parse("alice/agit-web@s-202608211530-9c3f", AGENT_ID).unwrap();
        assert_eq!(l.slug(), "alice/agit-web");
    }

    /// A well-formed lineage goes through and turns back into its string **unchanged** — that
    /// string is the `AGIT_SESSION` given to the harness, and one byte off settles the child
    /// process somewhere else.
    #[test]
    fn a_well_formed_lineage_round_trips_through_its_string_form() {
        let l = AgitSession::parse("acme/api@s/2f1a", AGENT_ID).unwrap();
        assert_eq!(l.owner(), "acme");
        assert_eq!(l.name(), "api");
        assert_eq!(l.branch(), "s/2f1a");
        assert_eq!(l.to_string(), "acme/api@s/2f1a");
        assert_eq!(AgitSession::parse(&l.to_string(), AGENT_ID).unwrap(), l);
    }

    /// A `/` in a branch name is normal (`s/2f1a`); the repo-segment rule does not judge it.
    #[test]
    fn a_branch_may_contain_slashes_even_though_a_repo_segment_may_not() {
        assert!(valid_branch_name("s/2f1a"));
        assert!(valid_branch_name("s-202608211530-9c3f"));
        assert!(AgitSession::new("acme/api", AGENT_ID, "feature/x/y").is_ok());
    }

    /// An option-shaped branch name is refused: git reads it as an argument, not a ref name.
    #[test]
    fn an_option_shaped_branch_name_is_refused_without_needing_git_to_say_so() {
        for bad in [
            "-x",
            "--upload-pack=touch /tmp/pwn",
            "a..b",
            "a b",
            "a~1",
            "a^",
            "a:b",
            "a.lock",
            "a.lock/b",
            "a/.hidden",
            ".hidden/a",
            "a/",
            "/a",
            "a@{0}",
            "HEAD",
            "@",
            "",
        ] {
            assert!(!valid_branch_name(bad), "`{bad}` must be refused");
        }
    }

    #[test]
    fn the_pure_branch_validator_keeps_git_valid_nontrivial_names() {
        for good in [
            "main",
            "feature/x.y",
            "release/v1_2-rc1",
            "topic@alice",
            // CJK fixture: git accepts a non-ASCII branch name, and ASCII here proves nothing.
            "用户/修复",
        ] {
            assert!(valid_branch_name(good), "`{good}` must remain usable");
        }
    }

    #[test]
    fn an_invalid_or_different_agent_id_is_part_of_the_lineage_identity() {
        assert!(AgitSession::new("acme/api", "not-an-id", "main").is_err());
        let first = AgitSession::new("acme/api", AGENT_ID, "main").unwrap();
        let second =
            AgitSession::new("acme/api", "00000000-0000-0000-0000-000000000002", "main").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.agent_id(), AGENT_ID);
    }
}
