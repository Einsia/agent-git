//! Typed recovery data belongs to one command invocation, never to diagnostic prose.
//!
//! Worker tasks explicitly carry a [`Reporter`] and enter it with [`Reporter::run`]. A
//! retained worker handle closes with its invocation and cannot publish into another one.

use serde::Serialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixCommand {
    kind: &'static str,
    argv: Vec<String>,
    cwd: String,
    env: BTreeMap<&'static str, String>,
    requires_interaction: bool,
}

/// Non-secret routing observed by the command that produced the recovery.
pub(crate) struct Context {
    cwd: PathBuf,
    home: PathBuf,
    hub: String,
}

impl Context {
    pub(crate) fn capture(directory: Option<&std::path::Path>, hub: Option<&str>) -> Option<Self> {
        let original = std::env::current_dir().ok()?;
        let cwd = directory
            .map(|directory| original.join(directory))
            .unwrap_or(original);
        let home = crate::infra::config::agit_home().ok()?;
        let home = if home.is_absolute() {
            home
        } else {
            cwd.join(home)
        };
        let hub = hub
            .map(str::to_owned)
            .unwrap_or_else(|| crate::infra::config::hub_url_at(Some(&home)));
        Some(Self { cwd, home, hub })
    }
}

impl FixCommand {
    /// Arguments must come from resolved, non-secret values; missing input is not an argument.
    pub(crate) fn current(args: &[&str], requires_interaction: bool) -> Option<Self> {
        Self::in_context(
            args.iter().map(OsString::from).collect(),
            Context::capture(None, None)?,
            requires_interaction,
        )
    }

    /// The request's selected Hub also governs the recovery, even if global routing changes.
    pub(crate) fn at_hub(args: &[&str], hub: &str, requires_interaction: bool) -> Option<Self> {
        Self::in_context(
            args.iter().map(OsString::from).collect(),
            Context::capture(None, Some(hub))?,
            requires_interaction,
        )
    }

    pub(crate) fn in_directory(
        args: Vec<OsString>,
        directory: Option<&std::path::Path>,
        requires_interaction: bool,
    ) -> Option<Self> {
        Self::in_context(
            args,
            Context::capture(directory, None)?,
            requires_interaction,
        )
    }

    /// The same command with `-y` before its `--`, unless it already carries it.
    pub(crate) fn confirmed(&self) -> Self {
        let mut confirmed = self.clone();
        if !confirmed
            .argv
            .iter()
            .any(|arg| arg == "-y" || arg == "--yes")
        {
            let at = confirmed
                .argv
                .iter()
                .position(|arg| arg == "--")
                .unwrap_or(confirmed.argv.len());
            confirmed.argv.insert(at, "-y".to_owned());
        }
        confirmed
    }

    /// A copied command retains its observed directory and routing without changing the shell.
    pub(crate) fn human_command(&self) -> Option<String> {
        if self
            .argv
            .iter()
            .chain(std::iter::once(&self.cwd))
            .chain(self.env.values())
            .any(|value| value.chars().any(char::is_control))
        {
            return None;
        }
        #[cfg(windows)]
        {
            Some(self.powershell_command())
        }
        #[cfg(not(windows))]
        {
            let mut words = self
                .env
                .iter()
                .map(|(name, value)| format!("{name}={}", posix_literal(value)))
                .collect::<Vec<_>>();
            words.push(posix_literal(&self.argv[0]));
            words.push("-C".into());
            words.push(posix_literal(&self.cwd));
            words.extend(self.argv[1..].iter().map(|arg| posix_literal(arg)));
            Some(words.join(" "))
        }
    }

    #[cfg(any(windows, test))]
    fn powershell_command(&self) -> String {
        let mut words = vec![
            "&".into(),
            powershell_literal(&self.argv[0]),
            "'-C'".into(),
            powershell_literal(&self.cwd),
        ];
        words.extend(self.argv[1..].iter().map(|arg| powershell_literal(arg)));
        format!(
            "pwsh -NoProfile -Command {{ \
             if ($PSVersionTable.PSVersion -lt [version]'7.3') {{ throw 'This command requires PowerShell 7.3 or newer.' }}; \
             $ErrorActionPreference = 'Stop'; \
             $PSNativeCommandArgumentPassing = 'Standard'; \
             $env:AGIT_HOME = {}; $env:AGIT_HUB_URL = {}; {}; exit $LASTEXITCODE }}",
            powershell_literal(&self.env["AGIT_HOME"]),
            powershell_literal(&self.env["AGIT_HUB_URL"]),
            words.join(" "),
        )
    }

    fn in_context(
        args: Vec<OsString>,
        context: Context,
        requires_interaction: bool,
    ) -> Option<Self> {
        let mut argv = vec!["agit".to_owned()];
        argv.extend(
            args.into_iter()
                .map(OsString::into_string)
                .collect::<Result<Vec<_>, _>>()
                .ok()?,
        );
        if argv.len() < 2 || argv[1].is_empty() || argv.iter().any(|arg| arg.contains('\0')) {
            return None;
        }
        if !context.cwd.is_absolute() || !context.home.is_absolute() {
            return None;
        }
        let cwd = context.cwd.into_os_string().into_string().ok()?;
        let home = context.home.into_os_string().into_string().ok()?;
        crate::infra::hub_authority::HubAuthority::parse(&context.hub).ok()?;
        let uri = context.hub.parse::<ureq::http::Uri>().ok()?;
        if !(context.hub.starts_with("http://") || context.hub.starts_with("https://"))
            || context.hub.contains('#')
            || !matches!(uri.scheme_str(), Some("http" | "https"))
            || uri
                .authority()
                .is_none_or(|authority| authority.as_str().contains('@'))
            || uri.query().is_some()
            || [&cwd, &home, &context.hub]
                .iter()
                .any(|value| value.contains('\0'))
        {
            return None;
        }
        Some(Self {
            kind: "agit_command",
            argv,
            cwd,
            env: BTreeMap::from([("AGIT_HOME", home), ("AGIT_HUB_URL", context.hub)]),
            requires_interaction,
        })
    }
}

#[cfg(not(windows))]
fn posix_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(any(windows, test))]
fn powershell_literal(value: &str) -> String {
    let mut quoted = String::from("'");
    for character in value.chars() {
        quoted.push(character);
        if matches!(
            character,
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}'
        ) {
            quoted.push(character);
        }
    }
    quoted.push('\'');
    quoted
}

#[derive(Default)]
struct State {
    closed: bool,
    actions: Vec<FixCommand>,
    refusal: Option<serde_json::Value>,
}

/// What a failed command reports for its reader to act on: the fix commands it registered and,
/// when it refused for a reason a caller can answer, that refusal.
#[derive(Default)]
pub(crate) struct Recovery {
    pub(crate) fix: Vec<FixCommand>,
    pub(crate) refusal: Option<serde_json::Value>,
}

/// An explicit handle for a worker contributing to its caller's recovery report.
#[derive(Clone)]
pub struct Reporter(Arc<Mutex<State>>);

thread_local! {
    static CURRENT: RefCell<Option<Reporter>> = const { RefCell::new(None) };
}

struct Binding {
    previous: Option<Reporter>,
    _thread: PhantomData<Rc<()>>,
}

impl Binding {
    fn enter(reporter: Reporter) -> Self {
        Self {
            previous: CURRENT.with(|slot| slot.replace(Some(reporter))),
            _thread: PhantomData,
        }
    }
}

impl Drop for Binding {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.replace(self.previous.take()));
    }
}

impl Reporter {
    pub fn run<T>(&self, task: impl FnOnce() -> T) -> T {
        let _binding = Binding::enter(self.clone());
        task()
    }

    fn register(&self, action: FixCommand) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if !state.closed && !state.actions.contains(&action) {
            state.actions.push(action);
        }
    }
}

pub fn current_reporter() -> Option<Reporter> {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// Records why the command refuses, as `{code, text, ...details}`, for `--json`'s `refusal`.
/// A command refuses once, so the first refusal stands. Codes are an open set: a reader that
/// doesn't know one shows `text`. Prose and subprocess output never call this.
pub fn refuse(code: &'static str, text: &str, details: serde_json::Value) {
    let Some(reporter) = current_reporter() else {
        return;
    };
    let mut refusal = serde_json::json!({"code": code, "text": text});
    if let serde_json::Value::Object(details) = details {
        refusal
            .as_object_mut()
            .expect("a refusal is an object")
            .extend(details);
    }
    let mut state = reporter
        .0
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if !state.closed && state.refusal.is_none() {
        state.refusal = Some(refusal);
    }
}

/// Prose and subprocess output never call this typed registration path.
pub fn register(make_action: impl FnOnce() -> Option<FixCommand>) {
    if let Some(reporter) = current_reporter()
        && let Some(action) = make_action()
    {
        reporter.register(action);
    }
}

/// Register only when this error determines the command's final failure, after recovery ends.
pub fn register_terminal_error(error: &anyhow::Error) {
    if let Some(required) = error.downcast_ref::<super::LoginRequired>() {
        register(|| required.fix());
    }
    if let Some(mismatch) = error.downcast_ref::<crate::hub::identity::HubMismatch>() {
        register(|| mismatch.fix());
    }
    register_terminal_api_error(error);
}

/// Register only when this response determines the command's final failure, after recovery ends.
pub fn register_terminal_api_error(error: &anyhow::Error) {
    if let Some(api) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::hub::client::ApiError>())
        && api.status == 404
        && let Some(repository) = api.subject()
    {
        refuse(
            "repository_not_found",
            &api.to_string(),
            serde_json::json!({"repository": repository, "hub": api.hub_name()}),
        );
    }
    register(|| {
        let api = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::hub::client::ApiError>())?;
        api.remedies()
            .contains(&crate::hub::client::Remediation::Authenticate {})
            .then(|| FixCommand::at_hub(&["login", "--hub", api.base()], api.base(), true))
            .flatten()
    });
}

#[cfg(any(unix, all(windows, target_env = "msvc"), test))]
pub(crate) struct Scope {
    reporter: Reporter,
    _binding: Binding,
}

#[cfg(any(unix, all(windows, target_env = "msvc"), test))]
impl Scope {
    pub(crate) fn enter() -> Self {
        let reporter = Reporter(Arc::new(Mutex::new(State::default())));
        Self {
            _binding: Binding::enter(reporter.clone()),
            reporter,
        }
    }

    pub(crate) fn finish(self, code: i32) -> Recovery {
        let mut state = self
            .reporter
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        let recovery = Recovery {
            fix: std::mem::take(&mut state.actions),
            refusal: state.refusal.take(),
        };
        drop(state);
        if code == 0 {
            Recovery::default()
        } else {
            recovery
        }
    }
}

#[cfg(any(unix, all(windows, target_env = "msvc"), test))]
impl Drop for Scope {
    fn drop(&mut self) {
        let mut state = self
            .reporter
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        state.actions.clear();
        state.refusal = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(value: &str) -> FixCommand {
        FixCommand::in_context(
            vec!["resume".into(), value.into(), "--no-launch".into()],
            Context {
                cwd: std::env::current_dir().unwrap(),
                home: std::env::temp_dir().join("synthetic-store"),
                hub: "https://example.invalid".into(),
            },
            false,
        )
        .unwrap()
    }

    #[test]
    fn human_commands_keep_routing_and_reject_terminal_control_characters() {
        let command = action("literal'branch");
        let text = command.human_command().unwrap();
        assert!(text.contains("AGIT_HOME"));
        assert!(text.contains("AGIT_HUB_URL"));
        assert!(text.contains("-C"));
        for value in ["line\nnext", "\u{1b}[31m", "\u{85}"] {
            assert!(action(value).human_command().is_none());
        }
        let powershell = command.powershell_command();
        assert!(powershell.contains("'literal''branch'"));
        assert!(powershell.contains("$PSNativeCommandArgumentPassing = 'Standard'"));
        assert!(powershell.starts_with("pwsh -NoProfile -Command {"));
        assert!(powershell.contains("exit $LASTEXITCODE"));
        for quote in ['\'', '\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'] {
            assert_eq!(
                powershell_literal(&quote.to_string()),
                format!("'{quote}{quote}'")
            );
        }
    }

    #[test]
    fn known_arguments_are_lossless_data_including_literal_angle_brackets() {
        for expected in [
            "<work>",
            "",
            "@work",
            "'\"‘’‚‛",
            "line\nnext\\",
            "$(fixture); & other",
        ] {
            let value = serde_json::to_value(action(expected)).unwrap();
            assert_eq!(value["argv"][2], expected);
        }
        let original = action("<work>\n'\"‘’‚‛\\");
        let decoded: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(decoded["argv"][2], "<work>\n'\"‘’‚‛\\");
        assert_eq!(decoded["env"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn invocation_reports_deduplicate_and_workers_cannot_leak_into_a_later_scope() {
        let first = Scope::enter();
        let worker = current_reporter().unwrap();
        register(|| Some(action("first")));
        let concurrent = worker.clone();
        std::thread::spawn(move || {
            concurrent.run(|| {
                register(|| Some(action("first")));
                register(|| Some(action("worker")));
            })
        })
        .join()
        .unwrap();
        assert_eq!(first.finish(4).fix, vec![action("first"), action("worker")]);
        assert!(current_reporter().is_none());
        let second = Scope::enter();
        worker.run(|| register(|| Some(action("late"))));
        register(|| Some(action("second")));
        assert_eq!(second.finish(4).fix, vec![action("second")]);
        let success = Scope::enter();
        register(|| Some(action("unneeded")));
        refuse(
            "unneeded",
            "a refusal of a command that succeeded",
            serde_json::json!({}),
        );
        let succeeded = success.finish(0);
        assert!(succeeded.fix.is_empty() && succeeded.refusal.is_none());
        assert!(current_reporter().is_none());
    }

    /// A command refuses once: the first refusal stands, with its details beside its code and
    /// text, and nothing outside an invocation is kept. A later refusal that replaced the first
    /// would ask the reader a question about something the command never got to.
    #[test]
    fn the_first_refusal_stands_with_its_details() {
        refuse(
            "outside",
            "no invocation is listening",
            serde_json::json!({}),
        );
        let scope = Scope::enter();
        refuse(
            "folder_bound_elsewhere",
            "~/app already belongs to alice/other",
            serde_json::json!({"bound_to": "alice/other"}),
        );
        refuse("later", "a later refusal", serde_json::json!({}));
        assert_eq!(
            scope.finish(1).refusal,
            Some(serde_json::json!({
                "code": "folder_bound_elsewhere",
                "text": "~/app already belongs to alice/other",
                "bound_to": "alice/other",
            }))
        );
    }

    #[test]
    fn actions_reject_nuls_and_credential_bearing_context() {
        for hub in [
            "https://user:secret@example.invalid",
            "https://example.invalid?token=secret",
            "https://example.invalid/#access_token=SYNTHETIC",
            "file:///tmp/hub",
        ] {
            assert!(
                FixCommand::in_context(
                    vec!["login".into()],
                    Context {
                        cwd: std::env::current_dir().unwrap(),
                        home: std::env::temp_dir(),
                        hub: hub.into(),
                    },
                    true
                )
                .is_none()
            );
        }
        assert!(
            FixCommand::in_context(
                vec!["resume".into(), "bad\0value".into()],
                Context {
                    cwd: std::env::current_dir().unwrap(),
                    home: std::env::temp_dir(),
                    hub: "https://example.invalid".into(),
                },
                false
            )
            .is_none()
        );
    }

    #[test]
    fn a_selected_request_hub_controls_both_recovery_routing_values() {
        for hub in [
            "https://request.invalid",
            "https://request.invalid/base@path",
        ] {
            let action = FixCommand::at_hub(&["login", "--hub", hub], hub, true).unwrap();
            assert_eq!(action.argv, ["agit", "login", "--hub", hub]);
            assert_eq!(action.env["AGIT_HUB_URL"], hub);
        }
        assert!(
            FixCommand::at_hub(
                &["login", "--hub", "https://user:secret@request.invalid"],
                "https://user:secret@request.invalid",
                true,
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_arguments_are_not_replaced_with_lossy_targets() {
        use std::os::unix::ffi::OsStringExt;
        assert!(
            FixCommand::in_context(
                vec!["resume".into(), OsString::from_vec(vec![0xff])],
                Context {
                    cwd: std::env::current_dir().unwrap(),
                    home: std::env::temp_dir(),
                    hub: "https://example.invalid".into(),
                },
                false
            )
            .is_none()
        );
    }
}
