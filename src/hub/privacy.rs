//! Repository privacy configuration, publication and read transport.

use anyhow::{Result, ensure};

pub mod grants;
pub(crate) mod location;
pub mod policy;
pub(crate) mod publication;
pub mod sessions;
pub mod status;

#[cfg(test)]
mod test_server;

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
