//! The one way a command fetches the Hub's tags.
//!
//! A session tag this computer published stays on the turn's local commit, while the Hub's tag of
//! the same name is on the public commit that turn was published as: the same turn, the published
//! copy. A plain `git fetch --tags` refuses to clobber such a local tag and fails the whole fetch,
//! so every fetch of the Hub's tags leaves those names out and takes the rest as before.

use crate::Result;
use crate::domain::repo::Repo;
use std::io::Write;

/// Negative refspecs, which leave single names out of a fetch, arrived in Git 2.29.
const NEGATIVE_REFSPECS: (u32, u32) = (2, 29);

/// The arguments of one fetch of the Hub's tags, and the tags it left alone.
pub struct TagFetch {
    args: Vec<String>,
    /// Tags a Git without negative refspecs could not leave out, so the fetch took no tags.
    skipped: Vec<String>,
    /// The generated config naming the left-out tags. It lives as long as the fetch arguments do.
    _exclusions: Option<tempfile::NamedTempFile>,
}

impl TagFetch {
    /// `args` is a `fetch` that takes `--tags`. The left-out names reach Git through a generated
    /// config file of negative refspecs, so how many there are never meets a command-line limit.
    /// A record that cannot be read leaves the fetch as it would otherwise be.
    pub fn prepare(repo: &Repo, args: &[&str]) -> Self {
        let names =
            crate::domain::privacy_receipt::tags_published_elsewhere(repo).unwrap_or_default();
        let version = if names.is_empty() {
            None
        } else {
            git_version()
        };
        Self::plan(repo, args, names, version)
    }

    fn plan(repo: &Repo, args: &[&str], names: Vec<String>, version: Option<(u32, u32)>) -> Self {
        let owned = || args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let names = names
            .into_iter()
            .filter(|name| plain_refname(name))
            .collect::<Vec<_>>();
        if names.is_empty() || !args.contains(&"--tags") {
            return Self {
                args: owned(),
                skipped: Vec::new(),
                _exclusions: None,
            };
        }
        if version.is_some_and(|version| version >= NEGATIVE_REFSPECS) {
            let remote = remote_of(args).unwrap_or("origin");
            return match write_exclusions(repo, remote, &names) {
                Ok(file) => {
                    let mut full = vec![
                        "-c".to_owned(),
                        format!("include.path={}", file.path().display()),
                    ];
                    full.extend(owned());
                    Self {
                        args: full,
                        skipped: Vec::new(),
                        _exclusions: Some(file),
                    }
                }
                Err(_) => Self {
                    args: owned(),
                    skipped: Vec::new(),
                    _exclusions: None,
                },
            };
        }
        // Without negative refspecs no single tag can be left out, so this fetch takes none.
        Self {
            args: owned()
                .into_iter()
                .map(|arg| {
                    if arg == "--tags" {
                        "--no-tags".to_owned()
                    } else {
                        arg
                    }
                })
                .collect(),
            skipped: names,
            _exclusions: None,
        }
    }

    pub fn args(&self) -> Vec<&str> {
        self.args.iter().map(String::as_str).collect()
    }

    /// Names the tags a Git without negative refspecs could not fetch around, once the fetch ends.
    pub fn report(&self) {
        if self.skipped.is_empty() {
            return;
        }
        crate::ui::warning(&format!(
            "no tags were refreshed from the Hub: {} name their turns' public commits there, and this Git can't leave just them out (Git 2.29 or newer can)",
            self.skipped.join(", ")
        ));
    }
}

/// The remote a `fetch` names: its first argument that is not an option.
fn remote_of<'a>(args: &[&'a str]) -> Option<&'a str> {
    let fetch = args.iter().position(|arg| *arg == "fetch")?;
    args[fetch + 1..]
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .copied()
}

/// Whether `name` is a tag name Git accepts and that needs no quoting in a config value. A name
/// outside this set is not left out, so its fetch reports the conflict as it would without this.
fn plain_refname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 1024
        && name.bytes().all(|byte| {
            byte >= 0x80
                || (byte.is_ascii_graphic()
                    && !matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\' | b'"'))
        })
        && !name.contains("..")
        && !name.contains("@{")
        && name != "@"
        && !name.ends_with('.')
        && name
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

fn write_exclusions(
    repo: &Repo,
    remote: &str,
    names: &[String],
) -> Result<tempfile::NamedTempFile> {
    anyhow::ensure!(plain_refname(remote), "unexpected remote name");
    let directory = repo.common_dir()?.join("agit");
    crate::infra::config::create_state_dir(&directory)?;
    let mut file = tempfile::Builder::new()
        .prefix("fetch-exclusions-")
        .suffix(".config")
        .tempfile_in(&directory)?;
    let mut text = format!("[remote \"{remote}\"]\n");
    for name in names {
        text.push_str(&format!("\tfetch = \"^refs/tags/{name}\"\n"));
    }
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(file)
}

fn git_version() -> Option<(u32, u32)> {
    let output = crate::infra::git_runtime::command()
        .arg("--version")
        .output()
        .ok()?;
    parse_version(&output.stdout)
}

fn parse_version(output: &[u8]) -> Option<(u32, u32)> {
    let text = std::str::from_utf8(output).ok()?;
    let version = text
        .trim()
        .strip_prefix("git version ")?
        .split_ascii_whitespace()
        .next()?;
    let mut parts = version.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// Thousands of tags published under another commit are left out of one fetch through the
    /// generated config, whatever a command line could hold, while a tag new on the remote still
    /// arrives and a recorded tag the local repository deleted since fetches like any other. A
    /// helper that passed one `-c` per name would outgrow the Windows command line at this size,
    /// and one that kept leaving out a recorded name no local tag holds would never fetch it.
    #[test]
    fn a_fetch_takes_the_remote_tags_around_thousands_published_elsewhere() {
        const PUBLISHED: usize = 3000;
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote");
        let local = temp.path().join("local");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "-q", "-b", "main"]);
        git(&remote, &["commit", "-q", "--allow-empty", "-m", "turn"]);
        git(&remote, &["commit", "-q", "--allow-empty", "-m", "public"]);
        let public = git(&remote, &["rev-parse", "HEAD"]);
        let names = (0..PUBLISHED)
            .map(|index| format!("ms-turn-{index:05}"))
            .collect::<Vec<_>>();
        let mut refs = names
            .iter()
            .map(|name| format!("create refs/tags/{name} {public}\n"))
            .collect::<String>();
        refs.push_str(&format!("create refs/tags/ms-moved {public}\n"));
        let mut update = std::process::Command::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        update
            .stdin
            .take()
            .unwrap()
            .write_all(refs.as_bytes())
            .unwrap();
        assert!(update.wait().unwrap().success());
        let output = std::process::Command::new("git")
            .args(["clone", "-q", "--no-tags"])
            .arg(&remote)
            .arg(&local)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let turn = git(&local, &["rev-parse", "HEAD~1"]);
        let mut refs = names
            .iter()
            .map(|name| format!("create refs/tags/{name} {turn}\n"))
            .collect::<String>();
        refs.push_str(&format!("create refs/tags/ms-moved {turn}\n"));
        let mut update = std::process::Command::new("git")
            .arg("-C")
            .arg(&local)
            .args(["update-ref", "--stdin"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        update
            .stdin
            .take()
            .unwrap()
            .write_all(refs.as_bytes())
            .unwrap();
        assert!(update.wait().unwrap().success());
        let repo = Repo::at(&local);
        let mut published = names
            .iter()
            .map(|name| crate::domain::privacy_receipt::PublishedTag {
                name: name.clone(),
                local: turn.clone(),
                public: public.clone(),
            })
            .collect::<Vec<_>>();
        published.push(crate::domain::privacy_receipt::PublishedTag {
            name: "ms-moved".into(),
            local: turn.clone(),
            public: public.clone(),
        });
        crate::domain::privacy_receipt::record_published_tags(&repo, &published).unwrap();
        // Deleted here since: an ordinary tag again, which the fetch takes from the remote.
        git(&local, &["tag", "-d", "ms-moved"]);
        git(&remote, &["tag", "ms-new", &public]);

        let fetch = TagFetch::prepare(&repo, &["fetch", "origin", "--prune", "--tags"]);
        let args = fetch.args();
        assert!(
            args.join(" ").len() < 4096,
            "the names stay off the command line"
        );
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&local)
            .args(&args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(git(&local, &["rev-parse", "refs/tags/ms-turn-02999"]), turn);
        assert_eq!(git(&local, &["rev-parse", "refs/tags/ms-new"]), public);
        assert_eq!(git(&local, &["rev-parse", "refs/tags/ms-moved"]), public);
    }

    /// A Git without negative refspecs cannot leave single tags out, so the fetch takes no tags and
    /// names the ones it fetched around; with them the arguments only gain the generated config.
    /// A Git that got negative refspecs passed anyway would fail every fetch on an invalid refspec.
    #[test]
    fn an_older_git_takes_no_tags_and_names_the_ones_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q"]);
        let repo = Repo::at(temp.path());
        let args = ["fetch", "origin", "--prune", "--tags"];
        let old = TagFetch::plan(&repo, &args, vec!["ms-verified".into()], Some((2, 28)));
        assert_eq!(old.args(), ["fetch", "origin", "--prune", "--no-tags"]);
        assert_eq!(old.skipped, ["ms-verified"]);
        let new = TagFetch::plan(&repo, &args, vec!["ms-verified".into()], Some((2, 29)));
        assert_eq!(new.args()[0], "-c");
        assert!(new.args()[1].starts_with("include.path="));
        assert_eq!(&new.args()[2..], args);
        assert!(new.skipped.is_empty());
        let none = TagFetch::plan(&repo, &args, Vec::new(), Some((2, 28)));
        assert_eq!(none.args(), args);
        assert_eq!(parse_version(b"git version 2.28.0\n"), Some((2, 28)));
        assert_eq!(
            parse_version(b"git version 2.47.1.windows.1\n"),
            Some((2, 47))
        );
    }
}
