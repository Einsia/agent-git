//! Persistent identity of this daemon namespace.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub machine_fingerprint: String,
    pub display_name: String,
    pub created_at: String,
}

fn identity_path() -> crate::Result<PathBuf> {
    let path = super::rc_dir()?.join("identity.json");
    #[cfg(windows)]
    if path.try_exists()? {
        super::windows_security::validate_path(&path, false, true)?;
    }
    Ok(path)
}

/// Load or create the machine identity.
pub fn identity() -> crate::Result<Identity> {
    let p = identity_path()?;
    if let Ok(s) = std::fs::read_to_string(&p)
        && let Ok(id) = serde_json::from_str::<Identity>(&s)
    {
        return Ok(id);
    }
    let id = Identity {
        machine_fingerprint: uuid::Uuid::new_v4().to_string(),
        display_name: super::hostname(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    write_private(&p, &serde_json::to_string_pretty(&id)?)?;
    Ok(id)
}

pub fn set_display_name(name: &str) -> crate::Result<Identity> {
    let mut id = identity()?;
    id.display_name = name.to_string();
    write_private(&identity_path()?, &serde_json::to_string_pretty(&id)?)?;
    Ok(id)
}

fn write_private(p: &std::path::Path, body: &str) -> crate::Result<()> {
    #[cfg(windows)]
    super::windows_security::write_private_file(p, body.as_bytes())?;
    #[cfg(not(windows))]
    std::fs::write(p, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
