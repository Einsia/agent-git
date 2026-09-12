//! Project badges describe recorded code identity without selecting a session target.

use crate::domain::{link, meta, repo::Repo};
use crate::infra::{config, local_git::Deadline};
use std::path::PathBuf;

pub(super) const UNAVAILABLE: &str = "unavailable: project evidence";
const MAX_ORIGIN_BYTES: usize = 16 * 1024;
const MAX_METADATA_BYTES: usize = 1024 * 1024;

#[derive(PartialEq, Eq)]
enum Origin {
    Known(Option<String>),
    Unavailable,
}

pub(super) struct Observation {
    cwd: Option<PathBuf>,
    origin: Option<Origin>,
}

impl Observation {
    pub(super) fn new() -> Self {
        Self {
            cwd: std::env::current_dir().ok(),
            origin: None,
        }
    }

    pub(super) fn relation(
        &mut self,
        claim: &link::Link,
        inspect: bool,
        deadline: Deadline,
    ) -> &'static str {
        if self
            .cwd
            .as_ref()
            .and_then(|cwd| cwd.to_str())
            .is_some_and(|cwd| claim.cwd.as_deref() == Some(cwd))
        {
            return "here";
        }
        if !inspect || !claim.is_active() {
            return UNAVAILABLE;
        }
        if self.origin.is_none() {
            self.origin = Some(self.read_origin(deadline));
        }
        match self.origin.as_ref().unwrap() {
            Origin::Known(None) => "other",
            Origin::Known(Some(origin)) => {
                branch_relation(claim, origin, deadline).unwrap_or(UNAVAILABLE)
            }
            Origin::Unavailable => UNAVAILABLE,
        }
    }

    pub(super) fn recheck(&self, rows: &mut [Vec<String>], deadline: Deadline) {
        if let Some(before) = &self.origin
            && *before != self.read_origin(deadline)
        {
            for row in rows {
                if row[7] != "here" {
                    row[7] = UNAVAILABLE.into();
                }
            }
        }
    }

    fn read_origin(&self, deadline: Deadline) -> Origin {
        let Some(cwd) = &self.cwd else {
            return Origin::Unavailable;
        };
        let repo = Repo::at(cwd).local_objects_only();
        let read = || -> crate::Result<Option<String>> {
            let configured = repo.inspection_output_with_deadline(
                &["config", "--null", "--get", "remote.origin.url"],
                MAX_ORIGIN_BYTES,
                deadline,
            )?;
            if configured.status.code() == Some(1)
                && configured.stdout.is_empty()
                && configured.stderr.is_empty()
            {
                return Ok(None);
            }
            anyhow::ensure!(configured.status.success() && configured.stderr.is_empty());
            let inside = repo.inspection_output_with_deadline(
                &["rev-parse", "--is-inside-work-tree"],
                32,
                deadline,
            )?;
            anyhow::ensure!(inside.status.success() && inside.stderr.is_empty());
            match std::str::from_utf8(&inside.stdout)?.trim() {
                "false" => return Ok(None),
                "true" => (),
                _ => anyhow::bail!("code repository evidence is unavailable"),
            }
            let output = repo.inspection_output_with_deadline(
                &["remote", "get-url", "origin"],
                MAX_ORIGIN_BYTES,
                deadline,
            )?;
            anyhow::ensure!(output.status.success() && output.stderr.is_empty());
            let origin = std::str::from_utf8(&output.stdout)?.trim();
            anyhow::ensure!(!origin.is_empty() && !origin.chars().any(char::is_control));
            Ok(Some(origin.to_owned()))
        };
        read().map(Origin::Known).unwrap_or(Origin::Unavailable)
    }
}

fn branch_relation(
    claim: &link::Link,
    origin: &str,
    deadline: Deadline,
) -> crate::Result<&'static str> {
    let (Some(owner), Some(agent), Some(branch)) = (&claim.owner, &claim.agent, &claim.branch)
    else {
        anyhow::bail!("recorded repository identity is unavailable");
    };
    for name in [owner, agent] {
        crate::domain::repo::valid_name(name)?;
        anyhow::ensure!(name.trim() == name.as_str());
    }
    let repo = Repo::open(config::repo_dir(owner, agent)?)
        .ok_or_else(|| anyhow::anyhow!("recorded repository is unavailable"))?
        .exact_root_inspection();
    let head = super::sessions::branch_head(&repo, branch, deadline)?;
    let output = repo.inspection_output_with_deadline(
        &["show", &format!("{head}:{}", meta::FILE)],
        MAX_METADATA_BYTES,
        deadline,
    )?;
    anyhow::ensure!(output.status.success() && output.stderr.is_empty());
    let snapshot = meta::parse_strict(std::str::from_utf8(&output.stdout)?, &head)?;
    anyhow::ensure!(snapshot.is_session_line());
    let relation = match snapshot.code.as_deref() {
        None => "other",
        Some(code) => {
            let (recorded, sha) = code
                .rsplit_once('@')
                .ok_or_else(|| anyhow::anyhow!("recorded code identity is unavailable"))?;
            anyhow::ensure!(
                !recorded.is_empty()
                    && sha.len() >= 4
                    && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            );
            if crate::commands::resume::same_repo_as(code, origin) {
                "same-repo"
            } else {
                "other"
            }
        }
    };
    anyhow::ensure!(super::sessions::branch_head(&repo, branch, deadline)? == head);
    Ok(relation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_origin_invalidates_project_matches_but_not_cwd_evidence() {
        let root = tempfile::tempdir().unwrap();
        let repo = Repo::init(root.path()).unwrap();
        repo.git(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/code.git",
        ])
        .unwrap();
        let observation = Observation {
            cwd: Some(root.path().to_path_buf()),
            origin: Some(Origin::Known(None)),
        };
        let mut rows = ["other", "same-repo", "here"].map(|relation| {
            let mut row = vec![String::new(); 8];
            row[7] = relation.into();
            row
        });
        observation.recheck(&mut rows, Deadline::new());
        assert_eq!(rows[0][7], UNAVAILABLE);
        assert_eq!(rows[1][7], UNAVAILABLE);
        assert_eq!(rows[2][7], "here");
    }
}
