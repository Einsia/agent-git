//! Local activity reads use stored objects after an explicit authenticated fetch.

use super::history::{Blob, Store};
use crate::domain::repo::Repo;
use crate::domain::repo::object_reader::{Body, ObjectReaders};
use crate::infra::local_git::Deadline;
use anyhow::{Context, Result, ensure};

const MAX_REFS: usize = 1 << 16;

pub(crate) struct LocalStore<'a> {
    repo: &'a Repo,
    deadline: Deadline,
    objects: ObjectReaders<'a>,
}

impl<'a> LocalStore<'a> {
    pub fn new(repo: &'a Repo) -> Self {
        let deadline = Deadline::new();
        Self {
            repo,
            deadline,
            objects: ObjectReaders::new(repo, deadline),
        }
    }

    fn bodies(&mut self, specs: &[String], maximum: usize) -> Result<Vec<Blob>> {
        Ok(self
            .objects
            .bodies(specs, maximum)?
            .into_iter()
            .map(|body| match body {
                Body::Missing => Blob::Missing,
                Body::Large => Blob::Large,
                Body::Read(raw) => Blob::Body(raw),
            })
            .collect())
    }

    fn specs(objects: &[(&str, &str)]) -> Vec<String> {
        objects
            .iter()
            .map(|(commit, path)| format!("{commit}:{path}"))
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
#[error("activity history walk stopped")]
struct Stopped;

impl Store for LocalStore<'_> {
    fn refs(&mut self) -> Result<Vec<(String, String)>> {
        let output = self.repo.inspection_output_in_repository(
            &[
                "for-each-ref",
                &format!("--count={}", MAX_REFS + 1),
                "--format=%(refname)%00%(objectname)",
                "refs/heads/",
                "refs/remotes/origin/",
            ],
            16 * 1024 * 1024,
            self.deadline,
        )?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "local activity history is unavailable"
        );
        let mut refs = Vec::new();
        for line in std::str::from_utf8(&output.stdout)?.lines() {
            let (name, oid) = line.split_once('\0').context("invalid ref framing")?;
            if name != "refs/remotes/origin/HEAD" {
                refs.push((name.to_owned(), oid.to_owned()));
            }
        }
        ensure!(
            refs.len() <= MAX_REFS,
            "activity ref inventory exceeds the read budget"
        );
        Ok(refs)
    }

    fn branches(&self) -> &'static [&'static str] {
        &["--branches", "--remotes=origin"]
    }

    fn rev_list(
        &mut self,
        args: &[&str],
        visit: &mut dyn FnMut(&str) -> Result<bool>,
    ) -> Result<()> {
        let streamed =
            self.repo
                .git_stream_split_inspection(args, b'\n', self.deadline, |record| {
                    if visit(std::str::from_utf8(record)?)? {
                        Ok(())
                    } else {
                        Err(Stopped.into())
                    }
                });
        match streamed {
            Err(error) if error.is::<Stopped>() => Ok(()),
            other => other,
        }
    }

    fn locate(&mut self, objects: &[(&str, &str)]) -> Result<Vec<Option<(String, u64)>>> {
        self.objects.locate(&Self::specs(objects))
    }

    fn read(&mut self, objects: &[(&str, &str)], maximum: usize) -> Result<Vec<Blob>> {
        self.bodies(&Self::specs(objects), maximum)
    }

    fn blobs(&mut self, ids: &[&str], maximum: usize) -> Result<Vec<Blob>> {
        let specs: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
        self.bodies(&specs, maximum)
    }
}
