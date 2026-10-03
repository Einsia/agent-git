//! A published commit can omit native bytes only when private settlement evidence proves them.

use super::*;
use crate::domain::{link, privacy_receipt::outbox::Entry, store::Store};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Anchor {
    repository_id: String,
    branch: String,
    commit: String,
}

impl Anchor {
    pub(super) fn parse(params: &Value) -> crate::Result<Option<Self>> {
        params
            .get("after_archive")
            .map(|value| {
                let anchor: Self = serde_json::from_value(value.clone())?;
                ensure!(
                    uuid::Uuid::parse_str(&anchor.repository_id).is_ok(),
                    Failure::InvalidCursor
                );
                crate::domain::repo::valid_branch_name(&anchor.branch)?;
                ensure!(
                    anchor.commit.len() == 40
                        && anchor.commit.bytes().all(|b| b.is_ascii_hexdigit()),
                    Failure::InvalidCursor
                );
                Ok(anchor)
            })
            .transpose()
    }

    pub(super) fn floor(&self, target: &Target<'_>, parts: &mut [Segment]) -> crate::Result<u64> {
        let prior = target
            .entry
            .and_then(|row| {
                row.agit_session
                    .as_deref()
                    .zip(row.expected_agent_id.as_deref())
            })
            .map(|(session, identity)| super::super::lineage::AgitSession::parse(session, identity))
            .transpose()?;
        let native = target
            .context
            .as_ref()
            .map(|context| context.source.session_ref(target.native))
            .unwrap_or_else(|| target.native.to_owned());
        let kind = prior
            .as_ref()
            .map(super::super::capture::RepositoryKind::discover)
            .transpose()?;
        let lineage = super::super::capture::resolve(
            target.runtime,
            &native,
            std::path::Path::new(target.cwd),
            prior.as_ref(),
            kind.as_ref(),
        )?
        .context("Archive capture claim is missing")?;
        let repo = super::super::capture::require(&lineage)?;
        let publication = Entry::records(&repo, lineage.branch(), target.native, target.runtime)?
            .into_iter()
            .filter_map(|entry| entry.publication)
            .find(|receipt| {
                receipt.destination.agent_id == self.repository_id
                    && receipt.branch == self.branch
                    && receipt.published == self.commit
            })
            .context("Archive publication has no local receipt")?;
        let store = Store::open()?.context("Archive capture store is missing")?;
        let claim = link::get_checked(&store, target.runtime, &native)?
            .context("Archive claim is missing")?;
        let (bytes, hash) = if let Some(checkpoint) = claim
            .native_checkpoint
            .as_ref()
            .filter(|checkpoint| checkpoint.tip == publication.source)
        {
            (checkpoint.bytes, checkpoint.sha256.as_str())
        } else {
            ensure!(
                claim.materialized_from.as_deref() == Some(&publication.source),
                "Archive native boundary has advanced"
            );
            (
                claim
                    .baseline_bytes
                    .context("Archive native boundary is missing")?,
                claim
                    .baseline_hash
                    .as_deref()
                    .context("Archive native digest is missing")?,
            )
        };
        let (last, parents) = parts
            .split_last_mut()
            .context("Archive has no native file")?;
        ensure!(
            claim.resolve().as_ref() == Some(&last.source),
            "Archive native file changed"
        );
        verify_prefix(&mut last.file, last.end, bytes, hash)?;
        parents.iter().try_fold(bytes, |total, part| {
            total.checked_add(part.end).context(Failure::Limit)
        })
    }
}

fn verify_prefix(
    file: &mut std::fs::File,
    end: u64,
    bytes: u64,
    expected: &str,
) -> crate::Result<()> {
    ensure!(
        bytes > 0 && bytes <= end,
        "Archive boundary exceeds native history"
    );
    file.seek(SeekFrom::Start(bytes - 1))?;
    let mut delimiter = [0];
    file.read_exact(&mut delimiter)?;
    ensure!(
        delimiter == *b"\n",
        "Archive boundary splits a native record"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let copied = std::io::copy(&mut file.take(bytes), &mut hash)?;
    ensure!(
        copied == bytes && format!("{:x}", hash.finalize()) == expected,
        "Archive native prefix changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn confirmed_publication_skips_only_its_verified_native_prefix() {
        if crate::rc::in_isolated_test(
            "rc::local_history::archive::tests::confirmed_publication_skips_only_its_verified_native_prefix",
        ) {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
            std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
        }
        crate::rc::select_local_authority();
        use crate::{
            adapter::Adapter,
            domain::{
                privacy_receipt::{PublicationReceipt, SupervisorPushRequest},
                repo::Repo,
            },
            hub::identity::RemoteIdentity,
            rc::lineage::AgitSession,
        };
        let cwd = root.path().canonicalize().unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        let record = |text: &str| {
            format!(
                "{}\n",
                json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":text}]}})
            )
        };
        let prefix = record("Saved answer");
        let text = prefix.clone() + &record("Uncached answer");
        let adapter = crate::adapter::claude_code::ClaudeCode;
        adapter.install(&text, &native, &cwd).unwrap();
        let route = AgitSession::new(
            "alice/history",
            "00000000-0000-0000-0000-000000000001",
            "conversation",
        )
        .unwrap();
        let repo = Repo::init(&route.repo_dir().unwrap()).unwrap();
        repo.git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "Record fixture",
        ])
        .unwrap();
        repo.git(&["branch", route.branch()]).unwrap();
        let commit = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        let destination = RemoteIdentity::new("https://hub.invalid", route.agent_id()).unwrap();
        crate::hub::identity::pin(&repo, &destination).unwrap();
        let store = Store::open_or_init().unwrap();
        let mut claim = link::Link::new("claude-code", &native, Some(&cwd));
        claim.owner = Some("alice".into());
        claim.agent = Some("history".into());
        claim.branch = Some(route.branch().into());
        claim.native_checkpoint = Some(link::NativeCheckpoint {
            tip: commit.clone(),
            bytes: prefix.len() as u64,
            sha256: format!("{:x}", Sha256::digest(prefix.as_bytes())),
        });
        link::write(&store, &claim).unwrap();
        let mut request = SupervisorPushRequest {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            repository: route.slug(),
            branch: route.branch().into(),
            source: commit.clone(),
            destination: destination.clone(),
            notification_id: None,
        };
        Entry::begin(
            &repo,
            &mut request,
            crate::domain::privacy_receipt::outbox::Capture {
                generation: 1,
                incarnation: None,
                through_seq: None,
                session_id: "logical".into(),
                native_session_id: native.clone(),
                runtime: "claude-code".into(),
            },
        )
        .unwrap();
        let anchor =
            json!({"repository_id":route.agent_id(),"branch":route.branch(),"commit":commit});
        let params =
            json!({"session_id":native,"runtime":"claude-code","cwd":cwd,"after_archive":anchor});
        let pending = read(params.clone()).unwrap();
        assert!(pending["after_archive"].is_null());
        assert_eq!(pending["items"].as_array().unwrap().len(), 2);
        let receipt: PublicationReceipt = serde_json::from_value(json!({"version":2,"mode":"ordinary","repository":route.slug(),"branch":route.branch(),"source":commit,"published":commit,"destination":destination,"url":"https://hub.invalid/alice/history"})).unwrap();
        Entry::complete(&repo, &request, &receipt).unwrap();
        let tail = read(params.clone()).unwrap();
        assert_eq!(tail["after_archive"], anchor);
        assert_eq!(tail["before"], 0);
        assert_eq!(tail["archived_sources"][0]["before"], prefix.len());
        assert!(
            tail["items"][0]["source_id"]
                .as_str()
                .unwrap()
                .starts_with(tail["archived_sources"][0]["prefix"].as_str().unwrap())
        );
        assert_eq!(tail["items"].as_array().unwrap().len(), 1);
        assert_eq!(tail["items"][0]["event"]["text"], "Uncached answer");
        let mut wrong = params.clone();
        wrong["after_archive"]["commit"] = json!("a".repeat(40));
        let full = read(wrong.clone()).unwrap();
        assert!(full["after_archive"].is_null());
        assert_eq!(full["items"].as_array().unwrap().len(), 2);
        wrong["snapshot"] = tail["snapshot"].clone();
        wrong["before"] = json!(0);
        assert!(read(wrong).is_err());
        let mut advanced = claim.clone();
        advanced.native_checkpoint.as_mut().unwrap().tip = "b".repeat(40);
        link::write(&store, &advanced).unwrap();
        assert!(read(params).unwrap()["after_archive"].is_null());
    }

    #[test]
    fn a_native_rewrite_or_partial_boundary_cannot_hide_unarchived_messages() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"saved\nlatest\n").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"saved\n"));
        verify_prefix(&mut file, 13, 6, &hash).unwrap();
        assert!(verify_prefix(&mut file, 13, 5, &hash).is_err());
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"other\n").unwrap();
        assert!(verify_prefix(&mut file, 13, 6, &hash).is_err());
    }
}
