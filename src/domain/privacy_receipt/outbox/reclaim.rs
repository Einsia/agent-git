use super::*;

const RECLAIM_LIMIT: usize = 64;

impl Entry {
    /// A durable dominating receipt survives every unlink; uncertain entries are never candidates.
    pub fn reclaim_acknowledged(repo: &Repo, request: &SupervisorPushRequest) -> Result<usize> {
        let latest = Self::load(repo, request)?.context("publication intent is missing")?;
        if latest.acknowledged.is_none() {
            return Ok(0);
        }
        let head = branch_head(repo, &request.branch)?;
        let candidates: Vec<_> = Self::records(
            repo,
            &request.branch,
            &latest.capture.native_session_id,
            &latest.capture.runtime,
        )?
        .into_iter()
        .filter(|previous| previous.request.source != head && latest.covers(previous))
        .collect();
        if candidates.is_empty() {
            return Ok(0);
        }
        // Git traversal runs outside the mutation lease so new local turns can retain their intent.
        let sources = crate::domain::repo::publication::raw_ancestors(repo, &request.source)?;
        let public = latest
            .publication
            .as_ref()
            .context("acknowledged publication is missing")?
            .ancestors(repo)?;
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|entry| {
                sources.contains(&entry.request.source)
                    && entry
                        .publication
                        .as_ref()
                        .is_some_and(|receipt| public.contains(&receipt.published))
            })
            .take(RECLAIM_LIMIT)
            .collect();
        if candidates.is_empty() {
            return Ok(0);
        }
        let (latest_path, _lock) = locked_path(repo, request)?;
        if Self::load(repo, request)?.as_ref() != Some(&latest)
            || branch_head(repo, &request.branch)? != head
        {
            return Ok(0);
        }
        let directory = latest_path
            .parent()
            .context("publication directory is missing")?;
        let mut removed = 0;
        for entry in candidates {
            let path = entry.record_path(directory)?;
            if Self::load(repo, &entry.request)?.as_ref() == Some(&entry) {
                fs::remove_file(path)?;
                removed += 1;
            }
        }
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(removed)
    }

    fn covers(&self, previous: &Self) -> bool {
        let (Some(new), Some(old), Some(new_notification), Some(old_notification)) = (
            self.publication.as_ref(),
            previous.publication.as_ref(),
            self.notification.as_ref(),
            previous.notification.as_ref(),
        ) else {
            return false;
        };
        previous.acknowledged.is_some()
            && self.request.source != previous.request.source
            && self.request.repository == previous.request.repository
            && self.request.branch == previous.request.branch
            && self.request.destination == previous.request.destination
            && new.url == old.url
            && new.mode == old.mode
            && new.recipient == old.recipient
            && new.policy_digest == old.policy_digest
            && new_notification.executor == old_notification.executor
            && new_notification.projected_session_id == old_notification.projected_session_id
            && self.capture.session_id == previous.capture.session_id
            && self.capture.native_session_id == previous.capture.native_session_id
            && self.capture.runtime == previous.capture.runtime
            && self.capture.incarnation == previous.capture.incarnation
            && self.capture.generation == previous.capture.generation
            && previous
                .capture
                .through_seq
                .is_none_or(|old| self.capture.through_seq.is_some_and(|new| new >= old))
    }
}

fn branch_head(repo: &Repo, branch: &str) -> Result<String> {
    let output = repo.inspection_output(
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        128,
    )?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "publication source branch is unavailable"
    );
    let head = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(
        super::super::valid_oid(&head),
        "publication source branch has an invalid commit"
    );
    Ok(head)
}
