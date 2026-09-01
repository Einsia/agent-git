//! Local mirror of the hub's workspace definitions.
//!
//! The hub (Aurora) is the truth for *what a workspace is*. But `agitd` must be
//! able to enforce the path allowlist while offline and must report what it
//! knows at register time, so it keeps a mirror at `~/.agit/rc/workspaces.json`.
//! It is written whenever the hub tells us something (register result,
//! `project.bind`), and read to build the allowlist. It is never authoritative:
//! on register, both sides reconcile and neither deletes the other's records.
//!
//! But "the hub is the truth for definitions" holds for **names**, not for
//! **authority**: for every root in the allowlist, the only evidence that counts
//! is the verified record this machine wrote itself at `project.bind`
//! (owner-only, see `require_role`). A register response carries no caller, so a
//! hub row claiming "this folder is bound to this workspace" can only **narrow**
//! the authority already held here, never add to it — otherwise a compromised
//! hub puts `~/.ssh` or the whole `$HOME` into the allowlist with one line of
//! JSON on every reconnect (they exist, they are directories, they are not on
//! `NEVER_BIND`, and `require_bindable_dir` does not stop them).

use crate::protocol::{HubWorkspace, LocalProject, LocalWorkspace};
use crate::rc::policy::CanonicalRoots;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mirror {
    /// workspace_id → projects (project_id → local_path)
    #[serde(default)]
    pub workspaces: BTreeMap<String, BTreeMap<String, String>>,
    /// The filesystem-verified form of `workspaces`, built once at load/bind/adopt.
    ///
    /// Kept out of JSON so a legacy mirror cannot assert that its own spelling is canonical.
    /// Invalid or missing legacy entries remain reportable through `to_local`, but never enter
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

    /// Reconcile with the hub's view (register result). Hub is truth for
    /// definitions, but only about the workspaces it actually names: inside one
    /// it may drop a project (that is an unbind), while a workspace it never
    /// mentions is left exactly as it was.
    /// Returns the rows **turned away** as `(workspace_id, project_id, local_path, why)`, so
    /// the caller can say so — silently dropping a folder the user did bind is just as hard
    /// to track down.
    pub fn adopt(&mut self, hub: &[HubWorkspace]) -> Vec<(String, String, String, String)> {
        let mut refused = vec![];
        // For a hub row to enter the allowlist it must line up with a (workspace, root) pair
        // this machine has **already verified**.
        //
        // Passing `require_bindable_dir` is not enough: `~/.ssh` exists, is a directory and is
        // not on `NEVER_BIND`, so it clears that gate. And a register response carries no
        // caller — on the `project.bind` path the hub judged the owner and the machine judged
        // the directory; on this path the machine cannot judge the person. The only local
        // evidence it holds is the record the owner's bind wrote into `verified`, so a
        // reconnect may only **narrow** it: the hub can make a root disappear (an unbind), it
        // cannot conjure a new one — otherwise a compromised hub only has to slip in one
        // `{project_id, "/Users/dev/.ssh"}` row per reconnect.
        //
        // Matching is on (workspace, canonical path), not on project_id: what the owner
        // approved on this machine is "this root belongs to this workspace". project_id is the
        // hub's bookkeeping, and a new id must not void the owner's authorization — but moving
        // the root into **another workspace** hands it to a different group of people, and
        // that has to be blocked.
        //
        // "Narrow only" is said about **the workspaces this response names**. A response that
        // never mentions ws-1 at all (a bug in a staged hub rollout, a lagging replica, a
        // response filtered by feature) is not an unbind, it is silence — and the gate above
        // turns the cost of one missing row from recoverable into permanent: once the local
        // record is cleared, `prior` never again matches the same root the hub lists next
        // time, and every reconnect is turned away by "bind it again from the workspace page"
        // — the folder is still there on the web interface, the agent says it has no
        // permission, and the owner must rebind by hand to recover. This is what "neither
        // deletes the other's records" in the module header is about. Keeping them is not a
        // widening: every row kept is one the owner bound by hand on this machine.
        let prior = std::mem::take(&mut self.verified);
        // Keep a copy of the **durable** record the owner bound by hand, before `workspaces`
        // is emptied below.
        //
        // It has two uses, both so that "momentarily unresolvable" does not become a permanent
        // unbind:
        //   * When the directory does not verify right now (the volume was ejected, a network
        //     share is not mounted yet at boot, the directory was renamed), that row stays in
        //     `workspaces` — and not in `verified`: the allowlist only honors what verifies
        //     right now.
        //   * Once the volume is back, this record is itself the proof the owner bound it.
        //     `rebuild_verified` uses exactly this at the next start; using the same copy here
        //     spares that restart.
        let durable = self.workspaces.clone();
        let mentioned: std::collections::BTreeSet<&str> =
            hub.iter().map(|w| w.workspace_id.as_str()).collect();
        self.workspaces
            .retain(|workspace_id, _| !mentioned.contains(workspace_id.as_str()));
        self.verified = prior
            .iter()
            .filter(|(workspace_id, _)| !mentioned.contains(workspace_id.as_str()))
            .map(|(workspace_id, ps)| (workspace_id.clone(), ps.clone()))
            .collect();
        for w in hub {
            let mut projects = std::collections::BTreeMap::new();
            let mut verified = std::collections::BTreeMap::new();
            // Which roots the owner bound on this machine inside this workspace. `verified` is
            // the preferred evidence; where it was never built (the directory did not resolve
            // at that moment), fall back to the durable records that verify **right now** —
            // both come from the same owner authorization, and `rebuild_verified` reads it the
            // same way, so this is not a widening.
            let owner_bound = |dir: &std::path::Path| {
                prior
                    .get(&w.workspace_id)
                    .is_some_and(|ps| ps.values().any(|bound| bound == dir))
                    || durable.get(&w.workspace_id).is_some_and(|ps| {
                        ps.values().any(|recorded| {
                            crate::rc::policy::require_bindable_dir(Path::new(recorded))
                                .is_ok_and(|bound| bound == dir)
                        })
                    })
            };
            for p in &w.projects {
                match crate::rc::policy::require_bindable_dir(std::path::Path::new(&p.local_path)) {
                    Ok(dir) if owner_bound(&dir) => {
                        projects.insert(p.project_id.clone(), dir.to_string_lossy().to_string());
                        verified.insert(p.project_id.clone(), dir);
                    }
                    Ok(_) => refused.push((
                        w.workspace_id.clone(),
                        p.project_id.clone(),
                        p.local_path.clone(),
                        "this machine has no owner-bound record of that folder in this workspace; \
                         bind it again from the workspace page"
                            .to_string(),
                    )),
                    Err(e) => {
                        // **Unresolvable is not unbound.** The hub still lists this
                        // project and the owner never revoked it — the volume is just
                        // not mounted right now, or the directory was renamed. Dropping
                        // that row with nothing but a line in `refused` lets the
                        // unconditional `save()` right after it in `pump.rs` erase the
                        // owner's hand-bound record from disk: `to_local` can no longer
                        // report even `exists: false`, `prior` never matches again once
                        // the volume is back, and every reconnect is turned away by
                        // "bind it again from the workspace page".
                        //
                        // So the record stays and `verified` gets nothing: the allowlist
                        // only honors roots that verify right now, and a path that cannot
                        // be mounted enters no confinement.
                        if let Some(kept) = durable
                            .get(&w.workspace_id)
                            .and_then(|ps| ps.get(&p.project_id))
                        {
                            projects.insert(p.project_id.clone(), kept.clone());
                        }
                        refused.push((
                            w.workspace_id.clone(),
                            p.project_id.clone(),
                            p.local_path.clone(),
                            e.to_string(),
                        ));
                    }
                }
            }
            self.workspaces.insert(w.workspace_id.clone(), projects);
            self.verified.insert(w.workspace_id.clone(), verified);
        }
        refused
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

    /// What we tell the hub at register.
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

    /// Rebuild the non-serialized proof cache once when loading an old mirror.
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
    let out = std::process::Command::new("git")
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
    use crate::protocol::{HubProject, HubWorkspace};

    /// Paths the hub names go through the gate too.
    ///
    /// This mirror **is** this machine's allowlist, and whether something is in it is the whole
    /// basis for whether it can be confined. The bind path passes `require_bindable_dir`, and so
    /// must the path that takes a register response on reconnect: gate only one of the two
    /// entrances to the same data and the ungated one is the one every reconnect takes.
    #[test]
    fn the_hubs_folders_go_through_the_same_gate_as_a_fresh_bind() {
        let ok = tempfile::tempdir().expect("tmp");
        let mut m = Mirror::default();
        // The owner bound it on this machine, through the owner-only `project.bind`.
        m.bind("ws-1", "p-ok", ok.path()).expect("bind");
        let refused = m.adopt(&[HubWorkspace {
            workspace_id: "ws-1".into(),
            name: "w".into(),
            projects: vec![
                HubProject {
                    project_id: "p-ok".into(),
                    local_path: ok.path().to_string_lossy().to_string(),
                },
                // A system directory: the bind path turns it away, and so must this one.
                HubProject {
                    project_id: "p-etc".into(),
                    local_path: "/etc".into(),
                },
                // A directory that does not exist at all.
                HubProject {
                    project_id: "p-gone".into(),
                    local_path: "/definitely/not/here/at/all".into(),
                },
            ],
        }]);

        let roots = m.roots("ws-1");
        assert_eq!(
            roots.len(),
            1,
            "only the valid directory survives: {roots:?}"
        );
        assert_eq!(
            refused.len(),
            2,
            "what is turned away must be reported, not dropped silently"
        );
        assert!(refused.iter().any(|(_, p, _, _)| p == "p-etc"));
        assert!(refused.iter().any(|(_, p, _, _)| p == "p-gone"));
    }

    /// A register response taken on reconnect can only **narrow** the allowlist, never widen it.
    ///
    /// `require_bindable_dir` turns away system roots; it does not turn away `~/.ssh` (it
    /// exists, it is a directory, it is not on `NEVER_BIND`), and a register response carries no
    /// caller — the machine cannot judge who said it. Without this gate — a hub row must match
    /// a local owner-bind record — a compromised hub slips in one
    /// `{project_id, "/Users/dev/.ssh"}` per reconnect and the operator's `fs.readFile` reads
    /// the private key out.
    #[test]
    fn reconnect_adoption_never_widens_beyond_what_the_owner_bound_here() {
        let bound = tempfile::tempdir().expect("tmp");
        // A perfectly "bindable" directory (it exists, it is a directory, it is not a system
        // root) — the owner just never bound it on this machine. This is the shape of `~/.ssh`.
        let never_bound = tempfile::tempdir().expect("tmp");
        let mut m = Mirror::default();
        m.bind("ws-1", "p-ok", bound.path()).expect("bind");

        let refused = m.adopt(&[HubWorkspace {
            workspace_id: "ws-1".into(),
            name: "w".into(),
            projects: vec![
                HubProject {
                    project_id: "p-ok".into(),
                    local_path: bound.path().to_string_lossy().to_string(),
                },
                HubProject {
                    project_id: "p-evil".into(),
                    local_path: never_bound.path().to_string_lossy().to_string(),
                },
            ],
        }]);

        let roots = m.roots("ws-1");
        assert_eq!(
            roots.first(),
            Some(&std::fs::canonicalize(bound.path()).unwrap()),
            "the root the owner bound survives"
        );
        assert_eq!(
            roots.len(),
            1,
            "a folder the hub added on its own must not enter the allowlist: {roots:?}"
        );
        assert!(
            refused.iter().any(|(_, p, _, _)| p == "p-evil"),
            "and it must be reported, not dropped silently: {refused:?}"
        );
        assert!(
            m.project_path("ws-1", "p-evil").is_none(),
            "a refused row must not yield a path through project_path either"
        );

        // Moving a root the owner bound into **another** workspace hands it to a different
        // group of people; that is refused too.
        m.adopt(&[HubWorkspace {
            workspace_id: "ws-2".into(),
            name: "w2".into(),
            projects: vec![HubProject {
                project_id: "p-moved".into(),
                local_path: bound.path().to_string_lossy().to_string(),
            }],
        }]);
        assert!(
            m.roots("ws-2").is_empty(),
            "authorization is per (workspace, root) and does not follow a root across workspaces"
        );

        // Conversely, inside one workspace a hub that only changes project_id (a bookkeeping
        // rename) does not void the authorization.
        let mut m2 = Mirror::default();
        m2.bind("ws-1", "p-old-id", bound.path()).expect("bind");
        let refused = m2.adopt(&[HubWorkspace {
            workspace_id: "ws-1".into(),
            name: "w".into(),
            projects: vec![HubProject {
                project_id: "p-new-id".into(),
                local_path: bound.path().to_string_lossy().to_string(),
            }],
        }]);
        assert!(
            refused.is_empty(),
            "a new id over the same root is not refused: {refused:?}"
        );
        assert_eq!(m2.roots("ws-1").len(), 1);
    }

    /// A workspace the response **does not mention** is not an unbind; its owner-bind proof
    /// must survive.
    ///
    /// The "a hub row can only narrow" gate makes one missing row irreversible: once the proof
    /// is cleared, the next response listing the same root does not match `prior` and is turned
    /// away by "bind it again", and no number of reconnects heals it — the symptom is a folder
    /// still showing on the web interface while the agent says it has no permission, and a
    /// single momentary wobble at the hub (a staged rollout, a lagging replica, a feature
    /// filter) is enough to cause it.
    #[test]
    fn a_response_that_omits_a_workspace_does_not_erase_what_the_owner_bound() {
        let bound = tempfile::tempdir().expect("tmp");
        let path = bound.path().to_string_lossy().to_string();
        let listed = || {
            vec![HubWorkspace {
                workspace_id: "ws-1".into(),
                name: "w".into(),
                projects: vec![HubProject {
                    project_id: "p-1".into(),
                    local_path: path.clone(),
                }],
            }]
        };
        let mut m = Mirror::default();
        m.bind("ws-1", "p-1", bound.path()).expect("bind");
        assert!(
            m.adopt(&listed()).is_empty(),
            "precondition: the owner bound it, so it is taken"
        );

        // A response that mentions only another workspace — it says nothing about ws-1.
        m.adopt(&[HubWorkspace {
            workspace_id: "ws-9".into(),
            name: "other".into(),
            projects: vec![],
        }]);
        assert_eq!(
            m.roots("ws-1").len(),
            1,
            "not being mentioned is not an unbind: {:?}",
            m.roots("ws-1")
        );

        // And it has to survive a daemon restart: the proof never enters JSON and is rebuilt
        // from `workspaces` at the next start, so what a deletion destroys is the row on disk.
        let mut reloaded = Mirror {
            workspaces: m.workspaces.clone(),
            verified: BTreeMap::new(),
        };
        reloaded.rebuild_verified();
        let refused = reloaded.adopt(&listed());
        assert!(
            refused.is_empty(),
            "a root the owner bound is honored again when the hub lists it: {refused:?}"
        );
        assert_eq!(reloaded.roots("ws-1").len(), 1);
    }

    /// A **momentary** failure to resolve must not become a permanent unbind.
    ///
    /// When the volume is ejected, a network share is not mounted yet, or the directory is
    /// renamed, `rebuild_verified` correctly drops that row from `verified` and keeps it in
    /// `workspaces` (see that doc comment: invalid legacy entries stay reportable through
    /// `to_local`). The register response right after it still lists the project — that is not
    /// an unbind — and if `adopt` rebuilt this workspace's whole section of `workspaces` from
    /// "what was taken", the row the owner bound by hand would be wiped from disk by the
    /// unconditional `save()` further down in `pump.rs`: `prior` never matches again even after
    /// the volume returns, `to_local` cannot report even `exists:false`, and only a manual
    /// rebind recovers.
    #[test]
    fn a_folder_that_is_momentarily_unresolvable_keeps_its_owner_bound_record() {
        let home = tempfile::tempdir().expect("tmp");
        let volume = home.path().join("ext-app");
        std::fs::create_dir_all(&volume).unwrap();
        let path = volume.to_string_lossy().to_string();
        let listed = || {
            vec![HubWorkspace {
                workspace_id: "ws-1".into(),
                name: "w".into(),
                projects: vec![HubProject {
                    project_id: "p-1".into(),
                    local_path: path.clone(),
                }],
            }]
        };
        let mut m = Mirror::default();
        m.bind("ws-1", "p-1", &volume).expect("bind");
        assert!(
            m.adopt(&listed()).is_empty(),
            "precondition: the owner bound it, so it is taken"
        );

        // The volume is ejected and `agitd` restarts at login: the proof never enters JSON,
        // leaving only the `workspaces` row.
        std::fs::remove_dir_all(&volume).unwrap();
        let mut m = Mirror {
            workspaces: m.workspaces.clone(),
            verified: BTreeMap::new(),
        };
        m.rebuild_verified();
        assert!(
            m.roots("ws-1").is_empty(),
            "precondition: a root that does not verify never enters the allowlist"
        );

        // The link comes up and the hub still lists the project — it was not unbound, it just
        // cannot be mounted right now.
        let refused = m.adopt(&listed());
        assert_eq!(
            refused.len(),
            1,
            "an unmountable root must be reported: {refused:?}"
        );
        assert!(
            m.roots("ws-1").is_empty(),
            "reporting it is not honoring it; the allowlist only takes roots that verify now"
        );
        let reported = m
            .to_local()
            .into_iter()
            .find(|w| w.workspace_id == "ws-1")
            .expect("ws-1 is still there");
        let project = reported
            .projects
            .iter()
            .find(|p| p.project_id == "p-1")
            .expect("the owner-bound row must still be there for the hub to show it as absent");
        assert!(
            !project.exists,
            "the reported state is exactly `exists: false`"
        );

        // The volume is back and the hub lists the same root: it is no longer turned away by
        // "bind it again".
        std::fs::create_dir_all(&volume).unwrap();
        let refused = m.adopt(&listed());
        assert!(
            refused.is_empty(),
            "the same owner-bound root is honored again once it is back: {refused:?}"
        );
        assert_eq!(m.roots("ws-1").len(), 1);
        assert_eq!(
            m.project_path("ws-1", "p-1"),
            Some(std::fs::canonicalize(&volume).unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_mirror_is_verified_once_without_trusting_its_spelling() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        let outside = tmp.path().join("outside");
        let alias = tmp.path().join("legacy-alias");
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

        // Retargeting the legacy spelling later cannot rewrite the verified allowlist.
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        assert_eq!(mirror.roots("ws"), cached);
        assert_eq!(mirror.project_path("ws", "valid"), cached.first().cloned());
    }
}
