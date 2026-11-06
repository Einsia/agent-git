//! Automatic publication runs after settlement releases its branch and session locks.

use crate::domain::repo::Repo;
use crate::ui;
use std::path::Path;
use std::process::Stdio;

/// Set on background children so a commit an automatic push started doesn't start another. It
/// only ever holds a push back: no push reads it to decide that it is automatic.
pub(super) const AUTOMATIC_ENV: &str = "AGIT_AUTO_PUSH";

/// The internal entry point background spawners publish through; [`automatic_push`] is the only
/// code that names it. It is not a flag of `agit push`, whose pushes are always the person's own.
pub(crate) const AUTOMATIC_ENTRY: &str = "__push-automatic";

static AUTOMATIC: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Marks this process as an automatic publication. Only [`AUTOMATIC_ENTRY`]'s entry point calls it.
pub(crate) fn mark_automatic() {
    let _ = AUTOMATIC.set(());
}

/// Whether this process publishes automatically, as [`AUTOMATIC_ENTRY`] marked it.
pub(crate) fn is_automatic() -> bool {
    AUTOMATIC.get().is_some()
}

/// Whether the person turned automatic publication off for `repository` on this computer: in its
/// own checkout here, or for every repository when there is none. A repository that publishes to
/// it on their behalf, such as the device-local one a Hub-enrolled folder uses, carries agit's own
/// value, so its enrolment and its pushes are judged by this instead; judged by its own value
/// alone, it keeps uploading after the person turned the destination off.
pub(crate) fn publication_explicitly_off(repository: &str) -> crate::Result<bool> {
    let (owner, name) = super::parse_slug(repository)?;
    match Repo::open(crate::infra::config::repo_dir(&owner, &name)?) {
        Some(checkout) => checkout.auto_push_explicitly_off(),
        None => Ok(crate::infra::config::get_global("push.auto")?.as_deref() == Some("false")),
    }
}

/// Whether `repo` may publish to `repository` automatically: its own preference allows it, its
/// folder was not turned off, and the person has not turned automatic publication off for the
/// destination on this computer.
pub(crate) fn publication_allowed(repo: &Repo, repository: &str) -> crate::Result<bool> {
    Ok(repo.auto_push_enabled()?
        && !repo.uploads_turned_off()?
        && !publication_explicitly_off(repository)?)
}

/// Which setting keeps [`publication_allowed`] from letting `repo` publish to a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Off {
    /// The destination's own setting in its checkout on this computer.
    Destination,
    /// The setting of the repository that records the conversation, when it is not the
    /// destination's checkout.
    CaptureRepository,
    /// The user preference, which covers every repository without a setting of its own, the
    /// destination included: it has no checkout here to hold one.
    UserDefault,
}

impl Off {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Destination => "destination",
            Self::CaptureRepository => "capture_repository",
            Self::UserDefault => "user_default",
        }
    }
}

/// Why [`publication_allowed`] refuses `repo` publishing to `repository`, and the narrowest
/// command that ends the refusal: the destination's own `push.auto` in its checkout here, and only
/// when turning that on is enough. A wait that only a wider switch ends gets no command. Turning
/// on the user preference to release one destination's turns would start automatic uploads of
/// every repository without a setting of its own, which the person did not ask for.
pub(crate) fn refusal(repo: &Repo, repository: &str) -> crate::Result<(Off, Option<String>)> {
    let (owner, name) = super::parse_slug(repository)?;
    let checkout = Repo::open(crate::infra::config::repo_dir(&owner, &name)?);
    let records_here = checkout.as_ref().is_some_and(|checkout| {
        matches!(
            (checkout.root().canonicalize(), repo.root().canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
    });
    if !records_here && !repo.auto_push_enabled()? {
        return Ok((Off::CaptureRepository, None));
    }
    Ok(match checkout {
        Some(_) => (
            Off::Destination,
            Some(format!("agit config --repo {repository} push.auto true")),
        ),
        None => (Off::UserDefault, None),
    })
}

/// The refusal an automatic publication meets while its repository's `push.auto` is off.
pub(crate) const DISABLED_FOR_REPOSITORY: &str =
    "automatic publication is disabled for this repository";

/// A background child may use saved consent, never a parent's interactive confirmation flag.
pub(crate) fn configure(command: &mut std::process::Command) {
    command.env(AUTOMATIC_ENV, "1").env_remove("AGIT_YES");
}

/// Makes `command`, an agit executable with its global options set, an automatic publication of
/// `target`. Every background push is spawned through here.
pub(crate) fn automatic_push(command: &mut std::process::Command, target: &str) {
    configure(command);
    command.args([AUTOMATIC_ENTRY, target]);
}

#[cfg(all(test, unix))]
#[test]
fn unattended_children_cannot_inherit_interactive_consent() {
    let mut command = std::process::Command::new("sh");
    command.env("AGIT_YES", "1").args([
        "-c",
        "test \"$AGIT_AUTO_PUSH\" = 1 && test -z \"${AGIT_YES+x}\"",
    ]);
    configure(&mut command);
    assert!(command.status().unwrap().success());
}

pub(super) fn branch_tip(directory: &Path, branch: &str) -> Option<String> {
    let repo = Repo::open(directory)?;
    let branch_ref = format!("refs/heads/{branch}");
    if let Some(commit) = repo.native_branch_commit(&branch_ref) {
        return Some(commit);
    }
    repo.git(&["rev-parse", "--verify", &branch_ref])
        .ok()
        .map(|value| value.trim().to_owned())
}

pub(super) fn after_settlement(
    directory: &Path,
    slug: &str,
    branch: &str,
    before: Option<&str>,
    quiet: bool,
) {
    if std::env::var_os(super::commit::SUPERVISOR_RESULT_ENV).is_some()
        || crate::rc::harness::settlement_is_delegated()
        || is_automatic()
        || std::env::var_os(AUTOMATIC_ENV).is_some()
    {
        return;
    }
    let Some(after) = branch_tip(directory, branch) else {
        return;
    };
    if before == Some(after.as_str()) {
        return;
    }
    let repo = Repo::at(directory);
    match repo.auto_push_enabled() {
        Ok(false) => return,
        Ok(true) => {}
        Err(error) => {
            ui::warning(&format!(
                "saved locally; automatic pushing is disabled: {error:#}"
            ));
            return;
        }
    }
    match super::project::blocks_auto_push(&repo, slug, branch) {
        Ok(false) => {}
        Ok(true) => return,
        Err(error) => {
            ui::warning(&format!(
                "saved locally; project policy could not be verified: {error:#}"
            ));
            return;
        }
    }
    let target = format!("{slug}@{branch}");
    // A settlement detached from the runtime has no terminal to tell the outcome on, so it is
    // recorded for the end of the session to tell.
    let upload = super::upload_outcome::start(slug, branch, &after);
    // A child process isolates the noninteractive publish policy and its JSON output from the
    // settlement caller. It uses the ordinary identity, access, visibility and secret gates.
    let outcome = std::env::current_exe().and_then(|executable| {
        let mut command = crate::infra::background::command(executable);
        command.arg("--json");
        automatic_push(&mut command, &target);
        command
            .current_dir(directory)
            .env("AGIT_SESSION", &target)
            .env("AGIT_TUI", "0")
            .stdin(Stdio::null());
        // Statistics attribute the push to the settling invocation, which may be a hook.
        if let Some(parent) = crate::telemetry::parent_invocation_id() {
            command.env("AGIT_TELEMETRY_PARENT_ID", parent);
        }
        command.output()
    });
    if let Some(upload) = upload {
        upload.finish(match &outcome {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(super::project::UploadFailure::reported(
                output.status.code(),
                &failure_reason(&output.stdout).map_or_else(String::new, |(reason, _)| reason),
            )
            .waiting_on(None, Some(&target))),
            Err(_) => Err(super::project::UploadFailure::Other),
        });
    }
    match outcome {
        Ok(output) if output.status.success() => {
            if !quiet {
                ui::success(&format!("automatically pushed {target}"));
                // The next turn uploads again, so the way to stop it comes with the notice.
                if super::project::binds(slug) {
                    ui::hint(&format!("not this one? `agit project drop {target}`"));
                }
            }
        }
        Ok(output) => match failure_reason(&output.stdout) {
            Some((reason, hints)) => {
                ui::warning(&format!(
                    "saved locally; automatic push failed for {target}: {reason}"
                ));
                if hints.is_empty() {
                    ui::hint(&format!("retry with `agit push {target}`"));
                }
                for hint in hints {
                    ui::hint(&hint);
                }
            }
            None => unexplained(&target),
        },
        Err(_) => unexplained(&target),
    }
}

fn unexplained(target: &str) {
    ui::warning(&format!(
        "saved locally; automatic push failed for {target}. Run `agit push {target}` to inspect the problem and retry."
    ));
}

/// The child's own error and hint lines, so the reason and its next commands are read where the
/// turn was settled. A child that produced no envelope, or no error line in it, has no reason to
/// relay.
fn failure_reason(stdout: &[u8]) -> Option<(String, Vec<String>)> {
    let document: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let lines = document["diagnostics"]["stderr"].as_array()?;
    let messages = |level: &str| {
        lines
            .iter()
            .filter(|line| line["level"] == level)
            .filter_map(|line| line["message"].as_str().map(str::to_owned))
            .collect::<Vec<_>>()
    };
    let reason = messages("error").join("; ");
    (!reason.is_empty()).then(|| (reason, messages("hint")))
}

/// The settling command repeats the child's reason and next commands; reading only the exit
/// status would leave "run push to inspect" as the whole explanation.
#[cfg(test)]
#[test]
fn a_failed_child_relays_its_error_and_hints() {
    let envelope = serde_json::json!({
        "ok": false,
        "diagnostics": {"stderr": [
            {"level": "info", "message": "Reading remote history and session modes…"},
            {"level": "error", "message": "remote branch work has diverged from this checkout"},
            {"level": "hint", "message": "keep both: `agit fork alice/app@work -b work-local`"},
        ]},
    });
    assert_eq!(
        failure_reason(envelope.to_string().as_bytes()),
        Some((
            "remote branch work has diverged from this checkout".into(),
            vec!["keep both: `agit fork alice/app@work -b work-local`".into()]
        ))
    );
    assert_eq!(failure_reason(b"not an envelope"), None);
}

/// Every background push is spawned through [`automatic_push`], which marks it automatic: a push
/// spawned another way would run as the person's own and skip what an automatic one is held to.
/// Each argument list in the source that starts with `push` is listed here with why it is not a
/// background `agit push`; a new one fails until it is spawned through [`automatic_push`] or added
/// with its reason.
#[cfg(test)]
#[test]
fn every_background_push_is_spawned_as_automatic() {
    const KNOWN: &[(&str, &str, &str)] = &[
        (
            "src/commands/push.rs",
            r#"let mut args = vec!["push", "--no-follow-tags", "--recurse-submodules=no"];"#,
            "git transport",
        ),
        (
            "src/domain/lfs/local.rs",
            r#"let mut args = vec!["push", "--object-id", "origin"];"#,
            "git lfs transport",
        ),
        (
            "src/hub/git.rs",
            r#"vec!["push", "--", "--progress", "refs/heads/fetch"],"#,
            "git transport",
        ),
        (
            "src/commands/project.rs",
            r#"let args = vec!["push".into(), target, "--yes".into()];"#,
            "the person's own save",
        ),
        (
            "src/commands/project.rs",
            r#"run_child(command, &["push".into(), target.into()], cwd)"#,
            "names the automatic push in its errors",
        ),
        (
            "src/commands/front/on.rs",
            r#"&["push", &destination.slug(), "-b", "main", "--private"],"#,
            "the person's own `agit on` creating their repository",
        ),
        (
            "src/hub/identity.rs",
            r#"&["push", slug, "--all", "--to-hub", current],"#,
            "a command suggested to the person",
        ),
    ];
    const TESTS: &[&str] = &[
        "src/commands/auto_push.rs",
        "src/commands/clone.rs",
        "src/commands/scan.rs",
        "src/commands/push/publication_tests.rs",
        "src/domain/refs.rs",
        "src/domain/secrets/mod.rs",
        "src/domain/repo/mod.rs",
        "src/hub/git.rs",
    ];
    fn files(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    files(&root.join("src"), &mut found);
    let shape = regex::Regex::new(r#"\[\s*"push"|"push"\.into\(\)|\.arg\(\s*"push"\s*\)"#).unwrap();
    let mut unknown = Vec::new();
    for path in found {
        let relative = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(&path).unwrap();
        // A test module's pushes run git in a fixture, never in the background.
        let production = text.split("#[cfg(test)]").next().unwrap_or_default();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| shape.is_match(line))
        {
            let in_tests = TESTS.contains(&relative.as_str())
                && (relative.ends_with("tests.rs") || !production.contains(line));
            let known = KNOWN
                .iter()
                .any(|(file, known, _)| *file == relative && *known == line);
            if !in_tests && !known {
                unknown.push(format!("{relative}: {line}"));
            }
        }
    }
    assert!(
        unknown.is_empty(),
        "spawn background pushes through auto_push::automatic_push: {unknown:#?}"
    );
}
