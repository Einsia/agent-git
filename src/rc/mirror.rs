//! Persisted project bindings verified by the local executor.
//!
//! Authenticated project binding writes this state. Controllers cannot populate an
//! allowlist through device discovery or connection setup. Reloading checks stored
//! paths against the filesystem; absent paths remain visible without granting access.

use crate::protocol::{LocalProject, LocalWorkspace};
use crate::rc::policy::CanonicalRoots;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mirror {
    /// workspace_id → projects (project_id → local_path)
    #[serde(default)]
    pub workspaces: BTreeMap<String, BTreeMap<String, String>>,
    /// The filesystem-verified form of `workspaces`, built at load and bind.
    ///
    /// Kept out of JSON so persisted state cannot assert that its own spelling is canonical.
    /// Invalid or missing entries remain reportable through `to_local`, but never enter
    /// the machine-side allowlist.
    #[serde(skip)]
    verified: BTreeMap<String, BTreeMap<String, PathBuf>>,
}

const FILE: &str = "workspaces.json";

impl Mirror {
    pub fn load() -> Mirror {
        let mut mirror: Mirror = super::load_json(FILE);
        mirror.rebuild_verified();
        mirror
    }

    pub fn save(&self) -> crate::Result<()> {
        super::save_json(FILE, self)
    }

    /// Validate and store a project path, returning its canonical spelling.
    pub fn bind(
        &mut self,
        workspace_id: &str,
        project_id: &str,
        local_path: &Path,
    ) -> Result<PathBuf, crate::rc::policy::PolicyError> {
        let dir = crate::rc::policy::require_bindable_dir(local_path)?;
        self.workspaces
            .entry(workspace_id.to_string())
            .or_default()
            .insert(project_id.to_string(), dir.to_string_lossy().to_string());
        self.verified
            .entry(workspace_id.to_string())
            .or_default()
            .insert(project_id.to_string(), dir.clone());
        Ok(dir)
    }

    pub fn unbind(&mut self, workspace_id: &str, project_id: &str) {
        if let Some(ps) = self.workspaces.get_mut(workspace_id) {
            ps.remove(project_id);
        }
        if let Some(ps) = self.verified.get_mut(workspace_id) {
            ps.remove(project_id);
        }
    }

    pub fn has_workspace(&self, workspace_id: &str) -> bool {
        self.workspaces.contains_key(workspace_id)
    }

    pub fn project_path(&self, workspace_id: &str, project_id: &str) -> Option<PathBuf> {
        self.verified.get(workspace_id)?.get(project_id).cloned()
    }

    /// The allowlist for one workspace: every bound project root.
    pub fn roots(&self, workspace_id: &str) -> CanonicalRoots {
        CanonicalRoots::from_verified(
            self.verified
                .get(workspace_id)
                .map(|ps| ps.values().cloned().collect())
                .unwrap_or_default(),
        )
    }

    /// Project metadata returned by workspace discovery.
    pub fn to_local(&self) -> Vec<LocalWorkspace> {
        self.workspaces
            .iter()
            .map(|(wid, ps)| LocalWorkspace {
                workspace_id: wid.clone(),
                projects: ps
                    .iter()
                    .map(|(pid, lp)| LocalProject {
                        project_id: pid.clone(),
                        local_path: lp.clone(),
                        exists: Path::new(lp).is_dir(),
                        git_origin: git_origin(Path::new(lp)),
                    })
                    .collect(),
            })
            .collect()
    }

    /// Rebuild filesystem authority from persisted bindings.
    fn rebuild_verified(&mut self) {
        self.verified.clear();
        for (workspace_id, projects) in &self.workspaces {
            let verified = projects
                .iter()
                .filter_map(|(project_id, path)| {
                    crate::rc::policy::require_bindable_dir(Path::new(path))
                        .ok()
                        .map(|path| (project_id.clone(), path))
                })
                .collect();
            self.verified.insert(workspace_id.clone(), verified);
        }
    }
}

/// `git remote get-url origin` for a directory, if it is a repo. Best effort.
pub fn git_origin(dir: &Path) -> Option<String> {
    if !dir.join(".git").exists() {
        return None;
    }
    // This runs git inside a directory the agent can write to — `.git/config` is an execution
    // channel; see `meta::GIT_SAFE`.
    let out = crate::infra::git_runtime::command()
        .args(crate::domain::meta::GIT_SAFE)
        .arg("-C")
        .arg(dir)
        .args(["remote", "get-url", "origin"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unavailable_bound_folders_remain_visible_without_granting_access() {
        let home = tempfile::tempdir().unwrap();
        let volume = home.path().join("volume");
        std::fs::create_dir(&volume).unwrap();
        let mut mirror = Mirror::default();
        mirror.bind("workspace", "project", &volume).unwrap();
        let persisted = serde_json::to_vec(&mirror).unwrap();
        std::fs::remove_dir(&volume).unwrap();
        let mut reloaded: Mirror = serde_json::from_slice(&persisted).unwrap();
        reloaded.rebuild_verified();
        assert!(reloaded.roots("workspace").is_empty());
        assert!(!reloaded.to_local()[0].projects[0].exists);
        assert!(reloaded.workspaces["workspace"].contains_key("project"));
        std::fs::create_dir(&volume).unwrap();
        reloaded.rebuild_verified();
        assert_eq!(
            reloaded.project_path("workspace", "project"),
            Some(std::fs::canonicalize(&volume).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn stored_bindings_are_verified_without_trusting_path_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let outside = tmp.path().join("outside");
        let alias = tmp.path().join("stored-alias");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let mut mirror = Mirror {
            workspaces: BTreeMap::from([(
                "ws".into(),
                BTreeMap::from([
                    ("valid".into(), alias.to_string_lossy().to_string()),
                    (
                        "missing".into(),
                        tmp.path().join("missing").to_string_lossy().to_string(),
                    ),
                ]),
            )]),
            verified: BTreeMap::new(),
        };
        mirror.rebuild_verified();
        let cached = mirror.roots("ws");
        assert_eq!(cached.first(), Some(&std::fs::canonicalize(&real).unwrap()));
        assert!(mirror.project_path("ws", "missing").is_none());
        assert!(
            mirror.workspaces["ws"].contains_key("missing"),
            "an invalid cache entry remains reportable to the hub but grants no authority"
        );

        // Retargeting the stored spelling later cannot rewrite the verified allowlist.
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        assert_eq!(mirror.roots("ws"), cached);
        assert_eq!(mirror.project_path("ws", "valid"), cached.first().cloned());
    }
}
