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
        let lineage = super::super::capture::inspect(
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
        ensure!(
            claim.is_active()
                && claim
                    .owner
                    .as_ref()
                    .zip(claim.agent.as_ref())
                    .map(|(owner, name)| format!("{owner}/{name}"))
                    .as_deref()
                    == Some(lineage.slug().as_str())
                && claim.branch.as_deref() == Some(lineage.branch()),
            "Archive capture route changed"
        );
        // The newest evidence first: the current checkpoint, the materialized baseline, then the
        // earlier settled prefixes. Only evidence recorded for the published tip itself counts.
        let baseline = claim
            .materialized_from
            .as_deref()
            .zip(claim.baseline_bytes)
            .zip(claim.baseline_hash.as_deref())
            .map(|((tip, bytes), sha256)| (tip, bytes, sha256));
        let (bytes, hash) = claim
            .native_checkpoint
            .iter()
            .map(|checkpoint| {
                (
                    checkpoint.tip.as_str(),
                    checkpoint.bytes,
                    checkpoint.sha256.as_str(),
                )
            })
            .chain(baseline)
            .chain(
                claim
                    .settled_prefixes
                    .iter()
                    .map(|prefix| (prefix.tip.as_str(), prefix.bytes, prefix.sha256.as_str())),
            )
            .find(|(tip, ..)| *tip == publication.source)
            .map(|(_, bytes, hash)| (bytes, hash))
            .context("Archive native boundary has advanced")?;
        let (last, parents) = parts
            .split_last_mut()
            .context("Archive has no native file")?;
        ensure!(
            claim.resolve().as_ref() == Some(&last.source),
            "Archive native file changed"
        );
        let end = last.end;
        verify_prefix(last.file()?, end, bytes, hash)?;
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
    let stamp = proofs::stamp(file);
    if stamp.is_some_and(|stamp| proofs::holds(stamp, bytes, expected)) {
        return Ok(());
    }
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let copied = std::io::copy(&mut file.take(bytes), &mut hash)?;
    ensure!(
        copied == bytes && format!("{:x}", hash.finalize()) == expected,
        "Archive native prefix changed"
    );
    if let Some(stamp) = stamp {
        proofs::remember(stamp, bytes, expected);
    }
    Ok(())
}

/// Prefixes already proven in this process. A proof is reused only while its file has not been
/// written since it was hashed: the same device and inode, with the length, modification time and
/// change time read before hashing. Any write misses, an append included: an append can't be told
/// from a rewrite of earlier bytes without reading them, and a proof that outlived a rewrite would
/// keep serving the published copy for records that no longer match it. A file modified less than
/// `QUIET` before the read is neither reused nor remembered, because a filesystem may give two
/// writes that close together the same times.
mod proofs {
    use std::sync::Mutex;
    #[cfg(unix)]
    use std::time::{Duration, SystemTime};

    /// Proofs kept at once; the oldest is dropped first.
    const KEPT: usize = 32;
    /// How long a file must have gone unmodified for its times to tell a later write apart.
    #[cfg(unix)]
    const QUIET: Duration = Duration::from_secs(1);

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct Stamp {
        device: u64,
        inode: u64,
        len: u64,
        modified: (i64, i64),
        changed: (i64, i64),
    }

    struct Proof {
        stamp: Stamp,
        bytes: u64,
        digest: String,
    }

    static PROOFS: Mutex<Vec<Proof>> = Mutex::new(Vec::new());

    #[cfg(unix)]
    pub(super) fn stamp(file: &std::fs::File) -> Option<Stamp> {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().ok()?;
        let age = SystemTime::now()
            .duration_since(metadata.modified().ok()?)
            .ok()?;
        (age >= QUIET).then(|| Stamp {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }

    #[cfg(not(unix))]
    pub(super) fn stamp(_: &std::fs::File) -> Option<Stamp> {
        None
    }

    pub(super) fn holds(stamp: Stamp, bytes: u64, digest: &str) -> bool {
        PROOFS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|proof| proof.stamp == stamp && proof.bytes == bytes && proof.digest == digest)
    }

    pub(super) fn remember(stamp: Stamp, bytes: u64, digest: &str) {
        let mut proofs = PROOFS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        proofs.retain(|proof| {
            !(proof.stamp.device == stamp.device
                && proof.stamp.inode == stamp.inode
                && proof.bytes == bytes)
        });
        if proofs.len() >= KEPT {
            proofs.remove(0);
        }
        proofs.push(Proof {
            stamp,
            bytes,
            digest: digest.to_owned(),
        });
    }
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
        // The published prefix remains readable while settlement owns the mutable branch.
        let branch_writer = link::lock_branch(&store, &route.slug(), route.branch()).unwrap();
        let claim_writer = link::lock(&store, "claude-code", &native).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let reading = params.clone();
        let worker = std::thread::spawn(move || sent.send(read(reading)).unwrap());
        let result = received.recv_timeout(std::time::Duration::from_secs(5));
        drop(claim_writer);
        drop(branch_writer);
        worker.join().unwrap();
        let tail = result
            .expect("history must not wait for the archive writer")
            .unwrap();
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
        // A client pages a fallback chain without the anchor it was not given back.
        let mut earlier = params.clone();
        earlier.as_object_mut().unwrap().remove("after_archive");
        earlier["snapshot"] = full["snapshot"].clone();
        earlier["before"] = full["before"].clone();
        read(earlier).expect("the next page of a fallback chain must not expire");
        wrong["snapshot"] = tail["snapshot"].clone();
        wrong["before"] = json!(0);
        assert!(read(wrong).is_err());
        let mut advanced = claim.clone();
        advanced.native_checkpoint.as_mut().unwrap().tip = "b".repeat(40);
        link::write(&store, &advanced).unwrap();
        assert!(read(params.clone()).unwrap()["after_archive"].is_null());
        // A later settlement keeps the published tip's own evidence, so a reader whose saved copy
        // is that tip still reads only what follows it; evidence with another digest proves
        // nothing, and the reader gets the whole page instead.
        advanced.retain_settled_prefix(claim.native_checkpoint.clone().unwrap());
        link::write(&store, &advanced).unwrap();
        assert_eq!(read(params.clone()).unwrap()["after_archive"], anchor);
        advanced.settled_prefixes[0].sha256 = format!("{:x}", Sha256::digest(b"other\n"));
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

        // A prefix longer than the boundary window, proven while its file was quiet so the proof
        // is kept, then rewritten in place outside that window and appended to: the boundary
        // bytes, the length asked for and the digest all still match, and only rereading the
        // prefix shows the change.
        let mut file = tempfile::tempfile().unwrap();
        let saved = format!(
            "{}\nsaved\n",
            "a".repeat(super::super::FENCE_BYTES as usize)
        );
        file.write_all(saved.as_bytes()).unwrap();
        file.write_all(b"latest\n").unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        let hash = format!("{:x}", Sha256::digest(saved.as_bytes()));
        let bytes = saved.len() as u64;
        let end = file.metadata().unwrap().len();
        verify_prefix(&mut file, end, bytes, &hash).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"b").unwrap();
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"more\n").unwrap();
        let end = file.metadata().unwrap().len();
        assert!(verify_prefix(&mut file, end, bytes, &hash).is_err());
    }
}
