//! Cross-Hub identities stay local and scoped to an immutable destination repository.

use super::*;
use crate::domain::session_privacy::{AccountIdentity, SessionPrivacy, SessionScheme};
use crate::hub::{Client, identity::RemoteIdentity};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    identity: RemoteIdentity,
    account: AccountIdentity,
    sessions: BTreeMap<String, String>,
    #[serde(skip)]
    source_accounts: BTreeMap<String, AccountIdentity>,
    #[serde(skip)]
    registrations: BTreeMap<String, SessionPrivacy>,
}

impl Delivery {
    pub fn prepare(
        source: &Repo,
        identity: &RemoteIdentity,
        client: &Client,
        explicit: bool,
        sessions: &BTreeSet<String>,
        persist: bool,
    ) -> Result<Option<Self>> {
        let (scope, directory) = Self::state(source, identity)?;
        if !explicit && !directory.exists() {
            return Ok(None);
        }
        crate::infra::config::create_state_dir(&directory)?;
        let lock = crate::infra::config::state_file_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock).context("delivery state is busy; retry")?;
        let saved = state::load(&directory, &scope)?;
        let mut delivery: Self = if let Some(bytes) = saved {
            serde_json::from_slice(&bytes).context("invalid cross-Hub delivery state")?
        } else {
            ensure!(explicit, "cross-Hub delivery state is missing");
            Self {
                identity: identity.clone(),
                account: client.session_account()?,
                sessions: BTreeMap::new(),
                source_accounts: BTreeMap::new(),
                registrations: BTreeMap::new(),
            }
        };
        delivery.account.validate()?;
        ensure!(
            delivery.identity == *identity && delivery.account.hub == identity.hub,
            "cross-Hub delivery belongs to another destination"
        );
        ensure!(
            delivery.account == client.session_account()?,
            "cross-Hub delivery belongs to another target account"
        );
        for (source, target) in &delivery.sessions {
            ensure!(
                meta::is_bare_id(source) && meta::is_bare_id(target) && source != target,
                "invalid cross-Hub session mapping"
            );
        }
        for session in sessions {
            if !delivery.sessions.contains_key(session) {
                ensure!(
                    persist,
                    "session must be delivered before a dry-run publication"
                );
                delivery
                    .sessions
                    .insert(session.clone(), meta::mint_session_id());
            }
        }
        if persist {
            state::save(&directory, &scope, &serde_json::to_vec(&delivery)?)?;
        }
        Ok(Some(delivery))
    }

    /// Whether this checkout has delivered sessions to `identity` from another Hub; publications
    /// to it then go through [`Delivery::prepare`].
    pub fn active(source: &Repo, identity: &RemoteIdentity) -> Result<bool> {
        Ok(Self::state(source, identity)?.1.exists())
    }

    fn state(source: &Repo, identity: &RemoteIdentity) -> Result<(String, std::path::PathBuf)> {
        let scope = digest_json(&json!({"version":1,"delivery":identity}))?;
        let directory = source
            .common_dir()?
            .join("agit/deliveries")
            .join(scope.trim_start_matches("sha256:"));
        Ok((scope, directory))
    }

    pub fn session(&self, source: &str) -> Result<&str> {
        self.sessions
            .get(source)
            .map(String::as_str)
            .context("session has no retained cross-Hub identity")
    }

    pub fn load_source_accounts(&mut self, hubs: &BTreeSet<String>) -> Result<()> {
        for hub in hubs {
            if let Some(account) = crate::infra::credentials::load_checked(hub)?
                .and_then(|credential| credential.account_id)
            {
                self.source_accounts
                    .insert(hub.clone(), AccountIdentity::new(hub, &account)?);
            }
        }
        Ok(())
    }

    fn mapped_account(&self, source: &AccountIdentity) -> Option<AccountIdentity> {
        if source.hub == self.account.hub {
            Some(source.clone())
        } else if self.source_accounts.get(&source.hub) == Some(source) {
            Some(self.account.clone())
        } else {
            None
        }
    }

    fn privacy(&self, source: &SessionPrivacy) -> Result<SessionPrivacy> {
        source.validate()?;
        ensure!(
            source.scheme == SessionScheme::Plain,
            "encrypted sessions cannot be delivered to another Hub"
        );
        let creator = source
            .hub
            .as_deref()
            .zip(source.creator.as_deref())
            .map(|(hub, creator)| AccountIdentity::new(hub, creator))
            .transpose()?;
        ensure!(
            creator
                .as_ref()
                .and_then(|creator| self.mapped_account(creator))
                .as_ref()
                == Some(&self.account),
            "cross-Hub delivery cannot verify this session's creator as your signed-in source account; sign in to the source Hub with that account, or exclude this session (creatorless destination registration is not supported)"
        );
        SessionPrivacy::new(Some(self.account.clone()))
    }

    pub fn admit_session(&mut self, session: &str, source: &SessionPrivacy) -> Result<()> {
        self.registrations
            .insert(session.into(), self.privacy(source)?);
        Ok(())
    }

    pub fn registration(&self, session: &str) -> Result<&SessionPrivacy> {
        self.registrations
            .get(session)
            .context("session has no admitted cross-Hub registration")
    }

    pub fn metadata(&self, source: &Meta) -> Result<Meta> {
        let mut metadata = source.clone();
        metadata.committer_account = source
            .committer_account
            .as_ref()
            .and_then(|account| self.mapped_account(account));
        metadata.fork_origin = None;
        metadata.source_ref = None;
        metadata.source_commit = None;
        if metadata.is_session_line() {
            ensure!(
                source
                    .session_privacy
                    .as_ref()
                    .is_none_or(|privacy| privacy.scheme == SessionScheme::Plain),
                "encrypted sessions cannot be delivered to another Hub"
            );
            metadata.session = self.session(&source.session)?.into();
            metadata.session_privacy = Some(self.registration(&source.session)?.clone());
        }
        Ok(metadata)
    }
}

/// Snapshot projection owns the outgoing bytes; no native source identity is a public alias.
pub(super) struct Snapshot<'a> {
    pub source: &'a Repo,
    pub destination: &'a Repo,
    pub oid: &'a str,
    pub parents: &'a [String],
    pub metadata: &'a Meta,
    pub policy: &'a PrivacyPolicy,
    pub aliases: &'a mut PathAliasStore,
    pub branch: &'a str,
    pub redactor: &'a Redactor,
}

impl Delivery {
    pub(super) fn project_snapshot(
        &self,
        snapshot: Snapshot<'_>,
    ) -> Result<(String, ProjectionReport)> {
        let Snapshot {
            source,
            destination,
            oid,
            parents,
            metadata,
            policy,
            aliases,
            branch,
            redactor,
        } = snapshot;
        let limit = super::super::privacy_publication::MAX_INPUT_BYTES;
        let (log, view) = if metadata.is_session_line() {
            super::super::storage::materialize_pair_local(source.root(), oid, limit, limit)?
        } else {
            (String::new(), String::new())
        };
        let mut projection = PublicationSnapshot::capture(source, &log, &view, metadata)?.project(
            policy,
            aliases,
            Some(branch),
            redactor,
        )?;
        let target = self.metadata(metadata)?;
        let mut files = projection.prepare_git(&target.session)?;
        let mut public: Meta = serde_json::from_slice(&files[meta::FILE])?;
        public.committer_account = target.committer_account.clone();
        public.session_privacy = target.session_privacy;
        if public.is_session_line() {
            public.cwd = "/workspace".into();
            // The target Hub labels the session by this field; its parsers key on each envelope's
            // source, so the projected records stay readable under the producing runtime's name.
            public.runtime = crate::adapter::normalize(&metadata.runtime)?.into();
        }
        public.title = metadata
            .title
            .as_deref()
            .map(|title| redactor.try_scrub(title).map(|report| report.text))
            .transpose()?;
        public.milestone = metadata
            .milestone
            .as_deref()
            .map(|text| redactor.try_scrub(text).map(|report| report.text))
            .transpose()?;
        files.insert(meta::FILE.into(), serde_json::to_vec_pretty(&public)?);
        let projected_tree = write_tree(destination, files)?;
        let mut entries = destination.git_bytes_result(&["ls-tree", "-z", &projected_tree])?;
        for entry in source
            .git_bytes_result(&["ls-tree", "-z", oid])?
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let separator = entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid delivery tree entry")?;
            let name = &entry[separator + 1..];
            if [
                b"session".as_slice(),
                b"events",
                b"LOG",
                b"VIEW",
                b"privacy",
            ]
            .contains(&name)
            {
                continue;
            }
            // Shared files keep their modes and bytes; session trees contain only new public objects.
            entries.extend_from_slice(entry);
            entries.push(0);
        }
        let tree = input(destination, &["mktree", "-z", "--missing"], &entries)?;
        let original = read(source, &["cat-file", "commit", oid], MAX_COMMIT)?;
        let body =
            attributed_commit_body(&tree, parents, &original, target.committer_account.as_ref())?;
        Ok((
            write_object(destination, "commit", &body)?,
            projection.report().clone(),
        ))
    }
}

fn attributed_commit_body(
    tree: &str,
    parents: &[String],
    original: &[u8],
    account: Option<&AccountIdentity>,
) -> Result<Vec<u8>> {
    let body = commit_body(tree, parents, original)?;
    let boundary = body
        .windows(2)
        .position(|bytes| bytes == b"\n\n")
        .context("projected commit has no header boundary")?;
    let mut rewritten = Vec::with_capacity(body.len());
    let committer = body[..boundary]
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(b"committer "))
        .and_then(|line| {
            line.iter()
                .rposition(|byte| *byte == b'>')
                .map(|end| &line[..=end])
        });
    for line in body[..boundary].split(|byte| *byte == b'\n') {
        if line.starts_with(b"author ") || line.starts_with(b"committer ") {
            let prefix = if line.starts_with(b"author ") {
                b"author " as &[u8]
            } else {
                b"committer " as &[u8]
            };
            let suffix = line
                .iter()
                .rposition(|byte| *byte == b'>')
                .map(|position| &line[position + 1..])
                .context("projected commit signature is malformed")?;
            rewritten.extend_from_slice(prefix);
            rewritten.extend_from_slice(b"AgentGit <");
            // A committer account proves no identity for a different Git author.
            let same_author = prefix != b"author "
                || committer == Some(&line[prefix.len()..line.len() - suffix.len()]);
            rewritten.extend_from_slice(
                account
                    .filter(|_| same_author)
                    .map_or(b"unmapped".as_slice(), |account| {
                        account.account_id.as_bytes()
                    }),
            );
            rewritten.extend_from_slice(b"@agentgit.local>");
            rewritten.extend_from_slice(suffix);
        } else {
            rewritten.extend_from_slice(line);
        }
        rewritten.push(b'\n');
    }
    rewritten.push(b'\n');
    rewritten.extend_from_slice(&body[boundary + 2..]);
    Ok(rewritten)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{storage, transcript};

    /// The target Hub labels a delivered session by the public meta's runtime, while the projected
    /// records always use one envelope format; a projection that names that format instead of the
    /// producing runtime labels every delivered Codex session as Claude Code.
    #[test]
    fn delivered_meta_names_the_source_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let source = Repo::init(&temp.path().join("source")).unwrap();
        source.git(&["config", "user.name", "Writer"]).unwrap();
        source
            .git(&["config", "user.email", "writer@example.invalid"])
            .unwrap();
        let destination = Repo::init(&temp.path().join("destination")).unwrap();
        let session = format!("agit-{}", "a".repeat(40));
        let metadata = Meta::new(session.clone(), "codex".into(), "/work".into());
        let log = transcript::wrap_lines(
            &format!(
                "{}\n",
                json!({"type":"response_item","payload":{"type":"message","role":"user",
                    "content":[{"type":"input_text","text":"Delivered turn"}]}})
            ),
            "codex",
            &session,
        );
        storage::write_snapshot(source.root(), &log, &log).unwrap();
        meta::write(source.root(), &metadata).unwrap();
        source.add_all().unwrap();
        source.commit("Codex turn").unwrap();
        let oid = source.git(&["rev-parse", "HEAD"]).unwrap();
        let target = "https://target.invalid";
        let account = AccountIdentity::new(target, "account-1").unwrap();
        let delivery = Delivery {
            identity: RemoteIdentity::new(target, "00000000-0000-0000-0000-000000000001").unwrap(),
            account: account.clone(),
            sessions: BTreeMap::from([(session.clone(), meta::mint_session_id())]),
            source_accounts: BTreeMap::new(),
            registrations: BTreeMap::from([(session, SessionPrivacy::new(Some(account)).unwrap())]),
        };
        let (commit, _) = delivery
            .project_snapshot(Snapshot {
                source: &source,
                destination: &destination,
                oid: oid.trim(),
                parents: &[],
                metadata: &metadata,
                policy: &PrivacyPolicy::default(),
                aliases: &mut PathAliasStore::default(),
                branch: "work",
                redactor: &Redactor::new(Default::default()),
            })
            .unwrap();
        let delivered = meta::read_at_ref_result(&destination, &commit)
            .unwrap()
            .unwrap();
        assert_eq!(delivered.runtime, "codex");
    }
}
