//! agit entry point.
//!
//! Does exactly three things: restore SIGPIPE, parse the subcommand, dispatch to the matching
//! file under `commands/`. No business logic belongs here.
//!
//! The command groups map one-to-one onto the PRD's "command overview":
//!
//! ```text
//! Auth        login · logout · whoami · config
//! Repos       init · clone · run · repo (create/list/info/visibility/collab/rename/delete/path)
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
    let json_hint = raw_args.iter().any(|arg| arg == "--json");
    let cli = match <Cli as clap::Parser>::try_parse_from(raw_args.clone()) {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            if json_hint
                && !matches!(
                    error.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                )
            {
                exit(commands::json::emit_parse_error(
                    commands::json::command_from_argv(&raw_args),
                    error.exit_code(),
                    &error.to_string(),
                ));
            }
            error.exit();
        }
    };
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
                commands::upgrade::maybe_startup_nudge("resume", cli.json);
                if let Some(code) = enter_and_migrate(cli.directory.as_deref()) {
                    exit(code);
                }
                exit(dispatch(Commands::Resume(Default::default()), false));
            }
            verdict => exit(bare_help(verdict, cli.json)),
        }
    };

    // The JSON path moves the cd and the startup migration inside the envelope: migration
    // warnings are part of this output too, and left outside the envelope they become bare text
    // ahead of the JSON that the consumer cannot parse.
    let json = commands::json_requested(cli.json, &command);
    let command_name = commands::command_name(&command);
    // A best-effort, once-a-day update hint belongs to the process startup path so it also
    // appears for ordinary commands, not only after a successful push. The helper skips the JSON
    // path because stdout there is a strict machine-readable envelope.
    commands::upgrade::maybe_startup_nudge(command_name, json);
    if json {
        if let Some(reason) = commands::json::incompatible(&command) {
            exit(commands::json::emit_rejection(
                command_name,
                agit::ExitCode::Interactive.as_i32(),
                reason,
            ));
        }
        let directory = cli.directory.clone();
        let code = commands::json::capture(command_name, || {
            if let Some(code) = enter_and_migrate(directory.as_deref()) {
                return code;
            }
            dispatch(command, true)
        });
        exit(code);
    }

    if let Some(code) = enter_and_migrate(cli.directory.as_deref()) {
        exit(code);
    }
    exit(dispatch(command, false));
}

/// Enter the directory given by `-C` and run the startup migration. `Some(code)` = failure, exit
/// with that code.
///
/// All three paths go through it (bare `agit`, the JSON envelope, ordinary dispatch), and the JSON
/// one has to go through it **inside** the envelope — so it is one function, not the same code
/// written out three times.
fn enter_and_migrate(directory: Option<&std::path::Path>) -> Option<i32> {
    if let Some(d) = directory
        && let Err(e) = std::env::set_current_dir(d)
    {
        agit::ui::error(&format!("cannot enter {}: {e}", d.display()));
        return Some(agit::ExitCode::Usage.as_i32());
    }
    if let Err(e) = commands::migration::migrate_startup() {
        agit::ui::error(&format!("local storage migration failed: {e:#}"));
        return Some(agit::ExitCode::Precondition.as_i32());
    }
    None
}

/// What to do when no subcommand is given and the interface is **not** entered.
///
/// The branch that enters the interface sits at the call site: it has to cd and run the startup
/// migration first, and this path must not touch the store. It prints the help, which is what
/// `arg_required_else_help` does: in a pipe, in CI and in an agent session, `agit`'s output is
/// unchanged down to the byte.
fn bare_help(verdict: agit::tui::Verdict, json: bool) -> i32 {
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
            Err(e) => commands::json::emit_parse_error(
                commands::json::command_from_argv(&argv),
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

        Commands::Import(a) => commands::import::run(a),
        Commands::Status(a) => commands::status::run(a),
        Commands::Switch(a) => commands::switch::run(a),
        Commands::Memory(a) => commands::memory::run(a),
        Commands::Distill(a) => commands::memory::run_distill(a),
        Commands::Branch(a) => commands::branch::run(a),

        Commands::Commit(a) => commands::commit::run(a),
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

        Commands::Search(a) => commands::search::run(a),
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
            // `{e:#}` prints the whole anyhow error chain, which is what diagnostics need.
            agit::ui::error(&format!("{e:#}"));
            agit::ExitCode::Usage.as_i32()
        }
    }
}
