//! agit entry point.
//!
//! Does exactly three things: restore SIGPIPE, parse the subcommand, dispatch to the matching
//! file under `commands/`. No business logic belongs here.
//!
//! The command groups map one-to-one onto the PRD's "command overview":
//!
//! ```text
//! Auth        login · logout · whoami · config
//! Repos       init · clone · run · repo (create/list/info/visibility/collab/invite/rename/delete/path)
//! Adoption    import · status · switch · branch
//! Recording   commit · tag
//! Inspection  log · show · diff · view
//! Fork/resume fork · new · resume
//! Merging     merge · cherry-pick · revert
//! Remotes     push · pull · fetch
//! Find/share  search · share · pr
//! Export/ops  export · scan · setup · doctor
//! ```

use agit::commands::{self, Cli, Commands};
use std::process::exit;

fn main() {
    // git-parity: the Rust runtime ignores SIGPIPE by default, so in `agit log | head` the first
    // println! after the pipe closes panics (exit code 101). Restore the default disposition
    // before any output.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let raw_args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // The private transport worker forwards packets without local Git or store state.
    let transport_worker = raw_args.len() == 3 && raw_args[1] == "rc" && raw_args[2] == "tunnel";
    if !transport_worker && let Err(error) = agit::infra::git_runtime::initialize() {
        let code = agit::ExitCode::Precondition.as_i32();
        if raw_args.iter().any(|arg| arg == "--json") {
            exit(commands::json::emit_rejection_version(
                &commands::json::command_from_argv(&raw_args),
                commands::json::Version::from_argv(&raw_args),
                code,
                &format!("{error:#}"),
                Vec::new(),
            ));
        }
        eprintln!("{error:#}");
        exit(code);
    }
    let telemetry_restart = std::env::var(agit::telemetry::RESTART_ENV).ok();
    // Consume invocation state before helpers or the requested command can inherit it.
    unsafe { std::env::remove_var(agit::telemetry::RESTART_ENV) };
    if let Some(code) = commands::status::native_observation_worker(&raw_args) {
        exit(code);
    }
    if raw_args.len() == 2 && raw_args[1] == "--internal-telemetry-flush" {
        let _ = agit::telemetry::transport::flush();
        return;
    }
    if raw_args
        .get(1)
        .is_some_and(|arg| arg == "--internal-install-completed")
        && (raw_args.len() == 2 || (raw_args.len() == 3 && raw_args[2] == "--defer-notice"))
    {
        let _ = agit::telemetry::acquisition::installed(raw_args.len() == 3);
        return;
    }
    if raw_args.len() == 3 && raw_args[1] == "--internal-install-stage" {
        if let Some(raw) = raw_args[2].to_str() {
            let _ = agit::telemetry::installation::record(raw);
        }
        return;
    }
    agit::telemetry::begin(&raw_args, telemetry_restart.as_deref());
    let code = run(raw_args);
    agit::telemetry::finish(code);
    exit(code);
}

fn run(raw_args: Vec<std::ffi::OsString>) -> i32 {
    let skip_update = std::env::var_os(commands::upgrade::RESTART_ENV).is_some();
    // Consume the restart marker before any command can pass its environment to another CLI.
    unsafe { std::env::remove_var(commands::upgrade::RESTART_ENV) };
    let json_hint = raw_args.iter().any(|arg| arg == "--json");
    let json_version_hint = commands::json::Version::from_argv(&raw_args);
    let cli = match <Cli as clap::Parser>::try_parse_from(raw_args.clone()) {
        Ok(cli) => cli,
        Err(error) => {
            agit::telemetry::observe(agit::telemetry::Observation::Stage("parse"));
            agit::telemetry::observe(agit::telemetry::Observation::Parse(error.kind()));
            use clap::error::ErrorKind;
            if json_hint
                && !matches!(
                    error.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                )
            {
                return commands::json::emit_parse_error_version(
                    commands::json::command_from_argv(&raw_args),
                    json_version_hint,
                    error.exit_code(),
                    &error.to_string(),
                );
            }
            let code = error.exit_code();
            let _ = error.print();
            return code;
        }
    };
    let json_version = cli.json_version.unwrap_or_default();
    let json = cli.command.as_ref().map_or(cli.json, |command| {
        commands::json_requested(cli.json, command)
    });
    if cli.json_version.is_some() && !json {
        let error = <Cli as clap::CommandFactory>::command().error(
            clap::error::ErrorKind::MissingRequiredArgument,
            "--json-version requires --json",
        );
        agit::telemetry::observe(agit::telemetry::Observation::Stage("parse"));
        let code = error.exit_code();
        let _ = error.print();
        return code;
    }
    if cli.no_color {
        unsafe { std::env::set_var("NO_COLOR", "1") };
    }
    if cli.yes {
        unsafe { std::env::set_var("AGIT_YES", "1") };
    }
    if cli.quiet {
        unsafe { std::env::set_var("AGIT_QUIET", "1") };
    }
    // Three states: a flag that is given states its position, an absent flag writes nothing —
    // writing nothing and writing "0" are different things, and the latter overrides the AGIT_TUI
    // the user exported themselves.
    if cli.tui {
        unsafe { std::env::set_var("AGIT_TUI", "1") };
    }
    if cli.no_tui {
        unsafe { std::env::set_var("AGIT_TUI", "0") };
    }
    // `--json` turns the TUI off, and **overrides `--tui`** — so it is written after it.
    //
    // The fourth entry test has no `AGIT_JSON` environment variable to read: JSON is a parameter
    // passed down through `dispatch`. Without this line, `agit --json <cmd>` opens the full-screen
    // interface in a terminal while `json::capture` is collecting its stdout into the JSON
    // envelope — a screenful of escape sequences poured into it.
    //
    // The global flag is enough: the subcommand-level forms `scan --json` / `view --json` have no
    // TUI entry point. Once they do, this has to follow `json_requested` instead.
    if cli.json {
        unsafe { std::env::set_var("AGIT_TUI", "0") };
    }

    let Some(command) = cli.command else {
        // No subcommand: **arbitrate first, then decide whether to touch the store**.
        //
        // Only actually entering the interface needs the cd and the startup migration. In a pipe,
        // in CI and in an agent session this command takes clap's help path and exits, and in this
        // binary that is a parse error — it happens before any store access. Migrating first puts
        // migration warnings ahead of the help and pays for a full scan on a command that does
        // nothing.
        match agit::tui::should_enter() {
            agit::tui::Verdict::Enter => {
                if !skip_update
                    && let Some(exe) = commands::upgrade::maybe_startup_nudge("resume", cli.json)
                {
                    return commands::upgrade::restart_after_upgrade(&exe, &raw_args);
                }
                if let Some(code) = prepare_startup(cli.directory.as_deref(), Startup::Migrate) {
                    return code;
                }
                return dispatch(Commands::Resume(Default::default()), false);
            }
            verdict => return bare_help(verdict, cli.json, json_version),
        }
    };

    // The JSON path moves the cd and the startup migration inside the envelope: migration
    // warnings are part of this output too, and left outside the envelope they become bare text
    // ahead of the JSON that the consumer cannot parse.
    if !json && matches!(&command, Commands::Push(args) if args.audit) {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            agit::ui::error(
                "push --audit requires an interactive terminal for review and final confirmation",
            );
            exit(agit::ExitCode::Interactive.as_i32());
        }
        if let Err(error) = commands::push::check_audit_environment() {
            agit::ui::error(&error.to_string());
            exit(agit::ExitCode::Usage.as_i32());
        }
    }
    let command_name = commands::command_name(&command);
    let startup = startup_for(&command);
    if json && let Some(reason) = commands::json::incompatible(&command) {
        agit::telemetry::observe(agit::telemetry::Observation::Stage("json_admission"));
        return commands::json::emit_rejection_version(
            command_name,
            json_version,
            agit::ExitCode::Interactive.as_i32(),
            reason,
            if json_version == commands::json::Version::V2 {
                commands::json::incompatible_fixes(
                    &command,
                    cli.directory.as_deref(),
                    &[
                        ("--yes", cli.yes),
                        ("--quiet", cli.quiet),
                        ("--no-color", cli.no_color),
                        ("--no-tui", cli.no_tui),
                    ],
                )
            } else {
                Vec::new()
            },
        );
    }
    // Admitted commands report updates outside JSON capture, keeping notices on process stderr.
    if !skip_update
        && startup.allows_nudge()
        && let Some(exe) = commands::upgrade::maybe_startup_nudge(command_name, json)
    {
        return commands::upgrade::restart_after_upgrade(&exe, &raw_args);
    }
    if json {
        let directory = cli.directory.clone();
        let code = commands::json::capture_version(command_name, json_version, || {
            if let Some(code) = prepare_startup(directory.as_deref(), startup) {
                return code;
            }
            dispatch(command, true)
        });
        return code;
    }

    if let Some(code) = prepare_startup(cli.directory.as_deref(), startup) {
        return code;
    }
    dispatch(command, false)
}

/// Apply the working directory before inspecting storage. Read-only inspection must refuse
/// pending recovery without performing it; JSON callers keep failures inside their envelope.
fn prepare_startup(directory: Option<&std::path::Path>, startup: Startup) -> Option<i32> {
    agit::telemetry::observe(agit::telemetry::Observation::Stage("startup"));
    if let Some(d) = directory
        && let Err(e) = std::env::set_current_dir(d)
    {
        agit::ui::error(&format!("cannot enter {}: {e}", d.display()));
        return Some(agit::ExitCode::Usage.as_i32());
    }
    let prepared = match startup {
        Startup::Inspect => commands::migration::check_readonly_startup(),
        Startup::Migrate => commands::migration::migrate_startup().map(|_| ()),
        Startup::ScopedDoctor
        | Startup::ScopedDiff
        | Startup::ScopedImport
        | Startup::ScopedReview
        | Startup::RemoteSearch
        | Startup::LocalSearch
        | Startup::ToolDispatcher => Ok(()),
    };
    if let Err(e) = prepared {
        agit::ui::error(&format!("local storage preparation failed: {e:#}"));
        return Some(agit::ExitCode::Precondition.as_i32());
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Startup {
    Migrate,
    Inspect,
    /// The command validates its explicit local scope before inspecting recovery evidence.
    ScopedDoctor,
    ScopedDiff,
    /// Explicit import inspection checks only its selected local repository before discovery.
    ScopedImport,
    /// Review and guarded edits inspect recovery only after selecting their repository.
    ScopedReview,
    /// Search queries the Hub without inspecting or migrating local repositories.
    RemoteSearch,
    /// Offline search must not run update checks or create migration/cache state.
    LocalSearch,
    /// Each dispatched tool prepares its own storage; the protocol parent has no local scope.
    ToolDispatcher,
}

impl Startup {
    fn allows_nudge(self) -> bool {
        !matches!(
            self,
            Self::ScopedDoctor
                | Self::ScopedImport
                | Self::ScopedReview
                | Self::RemoteSearch
                | Self::LocalSearch
                | Self::ToolDispatcher
        )
    }
}

fn startup_for(command: &Commands) -> Startup {
    match command {
        Commands::Rc(args) if matches!(args.action, commands::rc::Action::Tunnel) => {
            Startup::ToolDispatcher
        }
        Commands::Rc(args) if matches!(args.action, commands::rc::Action::Local(_)) => {
            Startup::ToolDispatcher
        }
        Commands::Status(_) => Startup::Inspect,
        Commands::Search(args) if args.local => Startup::LocalSearch,
        Commands::Search(_) => Startup::RemoteSearch,
        Commands::Mcp(_) => Startup::ToolDispatcher,
        Commands::Diff(args) if args.range.is_none() => Startup::ScopedDiff,
        Commands::Doctor(args) if args.repo.is_some() || args.repair_permissions.is_some() => {
            Startup::ScopedDoctor
        }
        Commands::Doctor(_) => Startup::Inspect,
        Commands::Show(args)
            if args.raw
                && args.log_only
                && args
                    .target
                    .as_deref()
                    .is_some_and(|target| target.contains('@')) =>
        {
            Startup::ScopedReview
        }
        Commands::Scan(args) if args.sensitive => Startup::ScopedReview,
        Commands::Push(args) if args.audit => Startup::ScopedReview,
        Commands::Revert(args) if args.expected_head.is_some() => Startup::ScopedReview,
        Commands::Import(args) if commands::import::needs_readonly_startup(args) => {
            Startup::ScopedImport
        }
        _ => Startup::Migrate,
    }
}

/// What to do when no subcommand is given and the interface is **not** entered.
///
/// The branch that enters the interface sits at the call site: it has to cd and run the startup
/// migration first, and this path must not touch the store. It prints the help, which is what
/// `arg_required_else_help` does: in a pipe, in CI and in an agent session, `agit`'s output is
/// unchanged down to the byte.
fn bare_help(verdict: agit::tui::Verdict, json: bool, version: commands::json::Version) -> i32 {
    match verdict {
        // `--tui` asks for the interface explicitly and there is no terminal: error out, do not
        // silently degrade into a help page. A silent degradation lets a script believe the flag
        // took effect.
        agit::tui::Verdict::NoTerminal => return agit::ExitCode::Interactive.as_i32(),
        agit::tui::Verdict::Explain(note) => agit::tui::warn_skipped(&note),
        agit::tui::Verdict::Enter | agit::tui::Verdict::Skip => {}
    }
    // clap prints the help itself.
    //
    // A hand-written `print_help()` has the same content, but clap's `arg_required_else_help` goes
    // down the **error** channel: stderr, exit code 2. Printing it here writes stdout instead, so
    // `agit 2>/dev/null` turns from "prints nothing" into "prints the whole help". Behavior on the
    // pipe side must not change by a single byte, so reuse clap's own path rather than reproducing
    // its output. A `--json` consumer reads an envelope, not a screenful of help text.
    //
    // With the subcommand required, `agit --json` is itself a parse failure and the block at the
    // top of `main` wraps it into a `parse_error` envelope. With the subcommand optional it parses
    // **successfully**, that path is bypassed entirely, and the caller gets text it cannot parse.
    //
    // So this branch takes the definition that requires a subcommand and parses the real argv with
    // it, which yields exactly that error. The help branch cannot do the same: for a person
    // sitting at a terminal the subcommand really is optional, and a usage line reading
    // `<COMMAND>` is a lie.
    if json {
        let argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
        let mut strict = agit::commands::cli_def().subcommand_required(true);
        let code = match strict.try_get_matches_from_mut(&argv) {
            Err(e) => commands::json::emit_parse_error_version(
                commands::json::command_from_argv(&argv),
                version,
                e.exit_code(),
                &e.to_string(),
            ),
            // Impossible: this definition requires a subcommand, and reaching here means there
            // is none.
            Ok(_) => agit::ExitCode::Usage.as_i32(),
        };
        return code;
    }

    let mut cmd = agit::commands::cli_def().arg_required_else_help(true);
    match cmd.try_get_matches_from_mut(["agit"]) {
        Err(e) => {
            let _ = e.print();
            e.exit_code()
        }
        // Impossible: with no subcommand this definition always raises that error. Reaching here
        // anyway counts as a usage error.
        Ok(_) => agit::ExitCode::Usage.as_i32(),
    }
}

/// The dispatch table. Deliberately too boring to get wrong — every change lives in the file
/// being called.
fn dispatch(cmd: Commands, json: bool) -> i32 {
    agit::telemetry::observe(agit::telemetry::Observation::Stage("dispatch"));
    let _echo = commands::echo::Invocation::enter(&cmd, json);
    let result = match cmd {
        Commands::Login(a) => commands::login::run(a),
        Commands::Rc(a) => commands::rc::run(a),
        Commands::Logout(a) => commands::logout::run(a),
        Commands::Whoami(a) => commands::whoami::run(a, json),
        Commands::Config(a) => commands::config::run(a),

        Commands::Init(a) => commands::init::run(a),
        Commands::Clone(a) => commands::clone::run(a),
        Commands::Run(a) => commands::run::run(a),
        Commands::Repo(a) => commands::repo::run(a),

        Commands::Import(a) => commands::import::run_with_output(a, json),
        Commands::Status(a) => commands::status::run(a),
        Commands::Memory(a) => commands::memory::run(a),
        Commands::Distill(a) => commands::memory::run_distill(a),
        Commands::Branch(a) => commands::branch::run(a),

        Commands::Commit(a) => commands::commit::run(a),
        Commands::File(a) => commands::file::run(a),
        Commands::Tag(a) => commands::tag::run(a),

        Commands::Log(a) => commands::log::run(a),
        Commands::Show(a) => commands::show::run(a),
        Commands::Diff(a) => commands::diff::run(a),
        Commands::View(a) => commands::view::run(a),

        Commands::Fork(a) => commands::fork::run(a),
        Commands::New(a) => commands::new::run(a),
        Commands::Resume(a) => commands::resume::run(a),

        Commands::Merge(a) => commands::merge::run(a),
        Commands::CherryPick(a) => commands::cherry_pick::run(a),
        Commands::Revert(a) => commands::revert::run(a),

        Commands::Push(a) => commands::push::run(a),
        Commands::Fetch(a) => commands::fetch::run(a),
        Commands::Pull(a) => commands::pull::run(a),

        Commands::Search(a) => commands::search::run(*a),
        Commands::Share(a) => commands::share::run(a),
        Commands::Pr(a) => commands::pr::run(a),

        Commands::Export(a) => commands::export::run(a),
        Commands::Scan(a) => commands::scan::run(a),
        Commands::Secrets(a) => commands::secret_vault::run(a),
        Commands::Setup(a) => commands::setup::run(a),
        Commands::Upgrade(a) => commands::upgrade::run(a),
        Commands::Doctor(a) => commands::doctor::run(a),
        Commands::Hooks(a) => commands::hooks::run(a),
        Commands::Mcp(a) => commands::mcp::run(a),
    };

    match result {
        Ok(code) => code.as_i32(),
        Err(e) => {
            commands::fix::register_terminal_error(&e);
            agit::ui::error(&commands::terminal_error_message(&e));
            commands::terminal_error_code(&e, agit::ExitCode::Failure).as_i32()
        }
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn desktop_protocol_commands_do_not_migrate_unrelated_repositories() {
        for action in ["start", "status", "bridge", "catalog"] {
            let cli = Cli::try_parse_from(["agit", "rc", "local", action]).unwrap();
            let startup = startup_for(&cli.command.unwrap());
            assert_eq!(startup, Startup::ToolDispatcher);
            assert!(!startup.allows_nudge());
        }
    }

    #[test]
    fn audited_push_defers_storage_and_never_admits_an_update_nudge() {
        for arguments in [
            vec!["agit", "push", "me/repo@work", "--audit"],
            vec!["agit", "push", "me/repo@work", "--audit", "--dry-run"],
            vec!["agit", "--yes", "push", "me/repo@work", "--audit"],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let startup = startup_for(&cli.command.unwrap());
            assert_eq!(startup, Startup::ScopedReview);
            assert!(!startup.allows_nudge());
        }
        let ordinary = Cli::try_parse_from(["agit", "push", "me/repo@work"]).unwrap();
        assert_eq!(startup_for(&ordinary.command.unwrap()), Startup::Migrate);
    }

    #[test]
    fn explicit_raw_log_review_skips_global_migration_and_nudges() {
        let cli = Cli::try_parse_from([
            "agit",
            "show",
            "audit/source@0123456789012345678901234567890123456789",
            "--log-only",
            "--raw",
            "--no-tui",
        ])
        .unwrap();
        let startup = startup_for(&cli.command.unwrap());
        assert_eq!(startup, Startup::ScopedReview);
        assert!(!startup.allows_nudge());
        for args in [
            vec!["agit", "show", "audit/source@main", "--raw"],
            vec!["agit", "show", "audit/source@main", "--log-only"],
            vec!["agit", "show", "--raw", "--log-only"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert_eq!(startup_for(&cli.command.unwrap()), Startup::Migrate);
        }
    }

    #[test]
    fn mcp_defers_storage_and_nudges_to_each_tool() {
        let cli = Cli::try_parse_from(["agit", "mcp"]).unwrap();
        let startup = startup_for(&cli.command.unwrap());
        assert_eq!(startup, Startup::ToolDispatcher);
        assert!(!startup.allows_nudge());
        let commit = Cli::try_parse_from(["agit", "commit"]).unwrap();
        assert_eq!(startup_for(&commit.command.unwrap()), Startup::Migrate);
    }

    #[test]
    fn local_search_never_admits_a_startup_nudge_before_validation_or_login() {
        for arguments in [
            vec!["agit", "search", "--local", "needle"],
            vec!["agit", "search", "--local", "--counts", "needle"],
            vec!["agit", "search", "--local", "--query", "\"\""],
            vec!["agit", "--quiet", "search", "--local", "needle"],
            vec!["agit", "--json", "search", "--local", "needle"],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            let startup = startup_for(&cli.command.unwrap());
            assert_eq!(startup, Startup::LocalSearch);
            assert!(
                !startup.allows_nudge(),
                "TTY and production mode cannot admit a local update request"
            );
        }
        let remote = Cli::try_parse_from(["agit", "search", "needle"]).unwrap();
        let startup = startup_for(&remote.command.unwrap());
        assert_eq!(startup, Startup::RemoteSearch);
        assert!(
            !startup.allows_nudge(),
            "remote search leaves update checks outside its request path"
        );
    }
}
