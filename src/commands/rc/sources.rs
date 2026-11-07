//! Register native stores locally and print source-correct connection coordinates.

use crate::commands::CmdResult;
use crate::rc::runtime_sources::Registry;
use anyhow::Context;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub struct SourceArgs {
    #[command(subcommand)]
    action: SourceAction,
}

#[derive(Subcommand)]
enum SourceAction {
    /// Register a Codex home, including directories with custom names.
    Add {
        /// Native runtime directory containing sessions and configuration.
        home: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Codex executable for this source; defaults to Codex on PATH when available.
        #[arg(long)]
        executable: Option<PathBuf>,
        /// Existing nonstandard native Unix socket; omit to use the standard socket.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// List registered homes and report inaccessible or replaced directories.
    List,
    /// Inspect a bounded page of conversations in an authorized local project.
    Sessions {
        source_id: String,
        #[arg(long)]
        project: PathBuf,
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Read observed model and permissions without taking control.
    Settings {
        source_id: String,
        thread_id: String,
        #[arg(long)]
        project: PathBuf,
    },
    /// Stop discovering a source without deleting its native files or stopping its server.
    Remove { source_id: String },
    /// Print the native CLI command for joining a conversation in this source.
    Connect {
        source_id: String,
        thread_id: String,
    },
}

pub(super) fn run(args: SourceArgs) -> CmdResult {
    let registry = Registry::open()?;
    let result = match args.action {
        SourceAction::Add {
            home,
            name,
            executable,
            socket,
        } => serde_json::to_value(registry.register(
            &home,
            executable.as_deref(),
            socket.as_deref(),
            name.as_deref(),
        )?)?,
        SourceAction::List => {
            // The catalog may not exist yet, or be mid-rebuild: the sources list anyway.
            let sweeps = crate::rc::runtime_catalog::sweep_states().unwrap_or_default();
            let sources = registry.list()?.into_iter().map(|source| {
                let error = source.validate().err().map(|error| format!("{error:#}"));
                let sweep = sweeps.get(&source.source_id).map(|(status, reason)| {
                    serde_json::json!({"status": status, "reason": reason})
                });
                serde_json::json!({"source": source, "available": error.is_none(), "error": error, "sweep": sweep})
            }).collect::<Vec<_>>();
            serde_json::json!({"sources":sources,"discovery":registry.discovery()?})
        }
        SourceAction::Sessions {
            source_id,
            project,
            after,
            limit,
        } => {
            let context =
                crate::rc::runtime_context::RuntimeContext::resolve(&registry, &source_id)?;
            let roots = project_roots(project)?;
            serde_json::to_value(context.list_page(&roots, after.as_deref(), limit)?)?
        }
        SourceAction::Settings {
            source_id,
            thread_id,
            project,
        } => {
            let context =
                crate::rc::runtime_context::RuntimeContext::resolve(&registry, &source_id)?;
            let roots = project_roots(project)?;
            let thread = context.locate(&thread_id, &roots)?;
            let settings = context.settings(&thread)?;
            serde_json::json!({"permission_mode":settings.permission_mode,"model":settings.model()})
        }
        SourceAction::Remove { source_id } => {
            registry.remove(&source_id)?;
            serde_json::json!({"source_id":source_id,"enabled":false})
        }
        SourceAction::Connect {
            source_id,
            thread_id,
        } => {
            anyhow::ensure!(
                !thread_id.is_empty()
                    && thread_id.len() <= 512
                    && !thread_id.chars().any(char::is_control),
                "invalid native thread ID"
            );
            let source = registry.resolve(&source_id)?.native()?;
            serde_json::json!({
                "environment":{"CODEX_HOME":source.home()},
                "argv":[source.executable(), "--remote", format!("unix://{}",source.socket().display()), "resume", thread_id],
                "thread_id":thread_id,"source_id":source_id
            })
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(crate::ExitCode::Ok)
}

fn project_roots(project: PathBuf) -> crate::Result<crate::rc::policy::CanonicalRoots> {
    let project = project
        .canonicalize()
        .context("project directory is unavailable")?;
    anyhow::ensure!(project.is_dir(), "project path is not a directory");
    Ok(crate::rc::policy::CanonicalRoots::from_untrusted([project]))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn custom_codex_home_registration_is_a_public_cli_command() {
        let args = crate::commands::Cli::try_parse_from([
            "agit",
            "rc",
            "sources",
            "add",
            "/profiles/codex-control2",
            "--name",
            "control2",
            "--executable",
            "/usr/bin/codex",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(crate::commands::Commands::Rc(_))
        ));
    }
}
