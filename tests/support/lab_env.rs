//! The environment every agit a test starts begins from.
//!
//! A test's AGIT_HOME is new, so the first command the binary runs reports a new installation.
//! With no Hub set the binary takes the production Hub, and that report and the command's usage
//! statistics go to production. A command cleared here keeps nothing from the parent but a Hub on
//! a local port nothing listens on; a test that serves a Hub sets its own after this, which wins.
//!
//! A test whose subject is the production Hub itself removes that Hub again, and then must send
//! its statistics to a local sink. The sink's address and key are spelled only here: the binary
//! ignores a sink whose address or key fails its check and sends to production instead.

// Each test that includes this module uses only part of it.
#![allow(dead_code)]

use std::process::Command;

/// A Hub address that refuses connections.
pub const LOCAL_HUB: &str = "http://127.0.0.1:9";

/// A port on the local machine nothing listens on.
pub const REFUSING_PORT: u16 = 9;

/// The key a local sink is configured with.
pub const SINK_KEY: &str = "test_key";

pub trait LabEnv {
    /// Clear the environment and point the Hub at [`LOCAL_HUB`].
    fn env_clear_local(&mut self) -> &mut Self;

    /// Send usage statistics to `port` on the local machine, whatever Hub the command uses.
    fn telemetry_sink(&mut self, port: u16) -> &mut Self;
}

impl LabEnv for Command {
    fn env_clear_local(&mut self) -> &mut Self {
        self.env_clear().env("AGIT_HUB_URL", LOCAL_HUB)
    }

    fn telemetry_sink(&mut self, port: u16) -> &mut Self {
        self.env("AGIT_TELEMETRY_HOST", format!("http://127.0.0.1:{port}"))
            .env("AGIT_TELEMETRY_KEY", SINK_KEY)
    }
}

/// The same environment for a command run on a pseudo-terminal.
#[cfg(feature = "rc")]
impl LabEnv for portable_pty::CommandBuilder {
    fn env_clear_local(&mut self) -> &mut Self {
        self.env_clear();
        self.env("AGIT_HUB_URL", LOCAL_HUB);
        self
    }

    fn telemetry_sink(&mut self, port: u16) -> &mut Self {
        self.env("AGIT_TELEMETRY_HOST", format!("http://127.0.0.1:{port}"));
        self.env("AGIT_TELEMETRY_KEY", SINK_KEY);
        self
    }
}
