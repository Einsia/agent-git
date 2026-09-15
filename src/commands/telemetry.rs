//! Inspect and change device-scoped product analytics without contacting the Hub.

use crate::{
    ExitCode,
    telemetry::{schema, state},
};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub action: Option<Action>,
}

#[derive(clap::Subcommand)]
pub enum Action {
    /// Show saved and effective usage-statistics settings.
    Status,
    /// Enable account-linked usage statistics.
    Enable,
    /// Disable usage statistics and delete unsent events.
    Disable,
    /// Print the complete command and argument privacy policies.
    Schema,
    /// Preview sanitized command properties without executing or sending them.
    Preview {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<std::ffi::OsString>,
    },
}

pub fn run(args: Args) -> super::CmdResult {
    match args.action.unwrap_or(Action::Status) {
        Action::Status => {
            let preferences = state::read()?;
            let (queued_events, queued_bytes) = crate::telemetry::transport::queue_status();
            let hub = crate::infra::config::hub_url();
            let (_, identity_state) = crate::infra::credentials::analytics_account(&hub);
            println!(
                "{}",
                serde_json::json!({"saved": preferences.preference, "effective": state::enabled(&preferences), "override": state::override_reason(), "notice_version": preferences.notice_version, "decision_source": preferences.decision_source, "identity_state": identity_state, "destination_configured": crate::telemetry::transport::Destination::for_hub(&hub).is_some(), "queued_events": queued_events, "queued_bytes": queued_bytes})
            );
        }
        Action::Enable => {
            eprintln!("{}", state::DISCLOSURE);
            let saved = state::choose(
                state::Preference::Enabled,
                state::DecisionSource::ExplicitEnable,
                false,
            )?;
            if state::enabled(&saved) {
                eprintln!("{}", state::ENABLED_NOTICE);
                let _ =
                    crate::telemetry::acquisition::resume_pending(&crate::infra::config::hub_url());
            } else {
                eprintln!(
                    "Usage statistics are saved as enabled, but an environment override keeps this process off."
                );
            }
        }
        Action::Disable => {
            state::choose(
                state::Preference::Disabled,
                state::DecisionSource::ExplicitDisable,
                false,
            )?;
            eprintln!("Usage statistics are disabled. Unsent events have been deleted.");
        }
        Action::Schema => println!("{}", schema::REGISTRY_JSON),
        Action::Preview { command } => {
            let argv = std::iter::once(std::ffi::OsString::from("agit"))
                .chain(command)
                .collect::<Vec<_>>();
            println!("{}", serde_json::Value::Object(schema::properties(&argv)));
        }
    }
    Ok(ExitCode::Ok)
}
