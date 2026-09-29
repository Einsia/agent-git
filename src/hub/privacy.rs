//! Repository privacy configuration, publication and read transport.

use anyhow::{Result, ensure};

pub(crate) mod publication;
pub mod repository_keys;
pub mod sources;

fn repository_path(repository: &str) -> Result<String> {
    let (owner, name) = crate::commands::parse_slug(repository)?;
    ensure!(
        [owner.as_str(), name.as_str()].iter().all(|part| {
            crate::domain::privacy_envelope::valid_token(part) && *part != "." && *part != ".."
        }),
        "invalid unlock repository"
    );
    Ok(format!("api/agents/{owner}/{name}/privacy"))
}
