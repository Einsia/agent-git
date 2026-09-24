//! Captured publication inspection shares the scanner's rules, provenance and report cap.

use super::*;
use crate::domain::lfs::{Pointer, inspection};
use crate::domain::repo::{ObjectBody, Repo, publication::PublicationPlan};
use std::cell::Cell;
use std::io::Read;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InspectionFailure {
    #[error("publication inspection configuration is invalid")]
    Configuration,
    #[error("publication inspection local policy is unavailable")]
    LocalState,
    #[error("publication inspection cannot read its captured content")]
    Content,
    #[error("publication inspection is incomplete")]
    Incomplete,
}

pub(crate) struct CapturedPolicy {
    allowlist: HashSet<String>,
    registered: RegisteredMatcher,
}

impl CapturedPolicy {
    pub(crate) fn capture(git_dir: &Path) -> Result<Self, InspectionFailure> {
        let home = crate::infra::config::agit_home().map_err(|_| InspectionFailure::LocalState)?;
        let allowlist = match std::fs::read_to_string(home.join(ALLOWLIST_FILE)) {
            Ok(text) => text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_owned)
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(InspectionFailure::Configuration);
            }
            Err(_) => return Err(InspectionFailure::LocalState),
        };
        let capture = || -> crate::Result<RegisteredMatcher> {
            let registered = load_registered_matcher()?;
            #[cfg(feature = "secret-vault")]
            {
                let dictionary =
                    crate::domain::secret_filter::RepositoryDictionary::open_at_git_dir(git_dir)
                        .context(ScanPreparationFailure::Configuration)?
                        .active_matcher()
                        .context(ScanPreparationFailure::LocalState)?;
                registered
                    .merged(&dictionary)
                    .context(ScanPreparationFailure::LocalState)
            }
            #[cfg(not(feature = "secret-vault"))]
            {
                let _ = git_dir;
                Ok(registered)
            }
        };
        let registered = capture().map_err(|error| {
            #[cfg(feature = "secret-vault")]
            if matches!(
                error.downcast_ref::<ScanPreparationFailure>(),
                Some(ScanPreparationFailure::Configuration)
            ) {
                return InspectionFailure::Configuration;
            }
            let _ = error;
            InspectionFailure::LocalState
        })?;
        Ok(Self {
            allowlist,
            registered,
        })
    }
}

pub(crate) struct Inspector<'a> {
    policy: &'a CapturedPolicy,
    limits: ScanLimits,
    remaining: u64,
    trusted: TrustedEnvelopeIdentities,
    out: HitCollector,
    unscanned: Unscanned,
    binary_git: u64,
}

impl<'a> Inspector<'a> {
    pub(crate) fn new(policy: &'a CapturedPolicy, limits: ScanLimits) -> Self {
        Self {
            policy,
            limits,
            remaining: limits.budget_bytes,
            trusted: TrustedEnvelopeIdentities::new(),
            out: HitCollector::new(),
            unscanned: Unscanned::default(),
            binary_git: 0,
        }
    }

    pub(crate) fn git(
        &mut self,
        repo: &Repo,
        plan: &PublicationPlan,
    ) -> Result<(), InspectionFailure> {
        match self.git_inner(repo, plan) {
            Ok(()) => self.require_complete(),
            Err(error) if error.downcast_ref::<BudgetSpent>().is_some() => {
                self.unscanned.over_budget = Some((
                    self.limits.budget_bytes.saturating_add(1),
                    self.limits.budget_bytes,
                ));
                Err(InspectionFailure::Incomplete)
            }
            Err(_) => Err(InspectionFailure::Content),
        }
    }

    fn git_inner(&mut self, repo: &Repo, plan: &PublicationPlan) -> crate::Result<()> {
        let commits = plan.commit_objects();
        let tags = plan.tag_objects();
        anyhow::ensure!(!commits.is_empty(), "prepared commit inventory is empty");
        checked_objects(repo, commits, Some("commit"))?;
        checked_objects(repo, tags, Some("tag"))?;
        let history = HistorySelection::Snapshots(commits);
        let reserved = self.remaining.min(TRUSTED_PROVENANCE_MAX_BYTES);
        let mut provenance = ProvenanceReadBudget {
            remaining: reserved,
            exhausted: false,
        };
        self.trusted =
            trusted_envelope_identities_with_budget(repo, commits, history, &mut provenance);
        self.remaining -= reserved - provenance.remaining;
        let spent = self.limits.budget_bytes - self.remaining;
        let estimate = estimate_object_bytes(
            repo,
            history,
            TagSelection::Objects(tags),
            &self.limits,
            spent,
        )?;
        if estimate > self.limits.budget_bytes {
            self.unscanned.over_budget = Some((estimate, self.limits.budget_bytes));
            return Ok(());
        }
        let binary = Cell::new(0);
        let remaining = Cell::new(self.remaining);
        let context = BlobScanContext {
            repo,
            limits: &self.limits,
            allowlist: &self.policy.allowlist,
            registered: &self.policy.registered,
            trusted_identities: &self.trusted,
            lfs_remaining: Cell::new(0),
            prepared_binary: Some(&binary),
            prepared_remaining: Some(&remaining),
        };
        let blobs = scan_publish_blobs(&context, history, &mut self.out, &mut self.unscanned);
        self.binary_git = binary.get();
        self.remaining = remaining.get();
        blobs?;
        self.raw_objects(repo, commits, "commit", Source::CommitObject)?;
        self.raw_objects(repo, tags, "tag", Source::TagObject)
    }

    fn raw_objects(
        &mut self,
        repo: &Repo,
        objects: &[String],
        kind: &str,
        source: Source,
    ) -> crate::Result<()> {
        for batch in objects.chunks(OBJECT_BATCH) {
            if self.out.is_full() {
                break;
            }
            let expected = checked_objects(repo, batch, Some(kind))?;
            let remaining = Cell::new(self.remaining);
            reserve_objects(&expected, self.limits.max_object_bytes, &remaining)?;
            self.remaining = remaining.get();
            let mut at = 0;
            repo.git_cat_file_batch(
                batch.to_vec(),
                usize::try_from(self.limits.max_object_bytes).unwrap_or(usize::MAX),
                |oid, received_kind, body| {
                    check_body(&expected, at, oid, received_kind, &body)?;
                    at += 1;
                    if self.out.is_full() {
                        return Ok(());
                    }
                    let bytes = match body {
                        ObjectBody::Read(bytes) => bytes,
                        ObjectBody::TooLarge(size) => {
                            self.unscanned
                                .oversized
                                .push((oid[..8].to_owned(), size as u64));
                            return Ok(());
                        }
                    };
                    // Raw headers and bodies remain covered even when they contain non-text bytes.
                    let original = String::from_utf8_lossy(bytes);
                    let text = crate::domain::secrets::mask_verified_git_headers(
                        repo,
                        &original,
                        received_kind,
                    );
                    let report = super::scan_text_capped_registered_views(
                        &original,
                        &text,
                        &self.policy.allowlist,
                        Policy::CLIENT,
                        self.out.remaining(),
                        &self.policy.registered,
                    );
                    if report.truncated {
                        self.out.mark_truncated();
                    }
                    self.out.extend(report.hits.into_iter().map(|mut hit| {
                        hit.file = Some(format!("{kind} object {}", &oid[..8]));
                        hit.source = source;
                        hit
                    }));
                    Ok(())
                },
            )?;
            anyhow::ensure!(
                at == expected.len(),
                "prepared raw object response is incomplete"
            );
        }
        Ok(())
    }

    pub(crate) fn lfs(
        &mut self,
        reader: impl Read,
        pointer: &Pointer,
    ) -> Result<bool, InspectionFailure> {
        self.require_complete()?;
        if self.out.is_full() {
            return Err(InspectionFailure::Incomplete);
        }
        if pointer.size > self.remaining {
            self.unscanned.over_budget = Some((
                self.limits
                    .budget_bytes
                    .saturating_sub(self.remaining)
                    .saturating_add(pointer.size),
                self.limits.budget_bytes,
            ));
            return Err(InspectionFailure::Incomplete);
        }
        match inspection::read(
            reader,
            pointer.size,
            Some(pointer),
            self.limits.max_object_bytes,
            &mut self.remaining,
        )
        .map_err(|_| InspectionFailure::Content)?
        {
            inspection::Payload::TooLarge => {
                self.unscanned
                    .oversized
                    .push((format!("lfs:{}", pointer.oid), pointer.size));
                Err(InspectionFailure::Incomplete)
            }
            inspection::Payload::Binary => {
                self.out.binary_carriers += 1;
                Ok(true)
            }
            inspection::Payload::Text(text) => {
                let report = scan_repository_payload_capped(
                    &text,
                    &self.policy.allowlist,
                    &self.trusted,
                    Policy::CLIENT,
                    self.out.remaining(),
                    &self.policy.registered,
                );
                if report.truncated {
                    self.out.mark_truncated();
                }
                self.out.extend(report.hits.into_iter().map(|mut hit| {
                    hit.file = Some(format!("lfs object {}", pointer.oid));
                    hit.source = Source::BlobObject;
                    hit
                }));
                self.require_complete()?;
                Ok(false)
            }
        }
    }

    pub(crate) fn require_complete(&self) -> Result<(), InspectionFailure> {
        if self.out.was_truncated() || !self.unscanned.is_empty() {
            return Err(InspectionFailure::Incomplete);
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> (ScanReport, u64) {
        (
            ScanReport {
                binary_carriers: self.binary_git + self.out.binary_carriers,
                truncated: self.out.was_truncated(),
                hits: self.out.into_hits(),
                unscanned: self.unscanned,
            },
            self.binary_git,
        )
    }
}

pub(super) fn checked_objects(
    repo: &Repo,
    objects: &[String],
    kind: Option<&str>,
) -> crate::Result<Vec<(String, String, u64)>> {
    let mut checked = Vec::with_capacity(objects.len());
    repo.git_cat_file_batch_check(objects.to_vec(), |oid, actual, size| {
        anyhow::ensure!(
            objects
                .get(checked.len())
                .is_some_and(|expected| expected == oid)
                && kind.map_or_else(
                    || matches!(actual, "blob" | "tree"),
                    |expected| expected == actual
                ),
            "prepared object header is invalid"
        );
        checked.push((oid.to_owned(), actual.to_owned(), size));
        Ok(())
    })?;
    anyhow::ensure!(
        checked.len() == objects.len(),
        "prepared object header response is incomplete"
    );
    Ok(checked)
}

pub(super) fn reserve_objects(
    objects: &[(String, String, u64)],
    limit: u64,
    remaining: &Cell<u64>,
) -> crate::Result<()> {
    let mut needed = 0u64;
    for (_, _, size) in objects {
        if *size <= limit {
            needed = needed.checked_add(*size).ok_or(BudgetSpent)?;
        }
    }
    if needed > remaining.get() {
        return Err(anyhow::Error::new(BudgetSpent));
    }
    remaining.set(remaining.get() - needed);
    Ok(())
}

pub(super) fn check_body(
    expected: &[(String, String, u64)],
    at: usize,
    oid: &str,
    kind: &str,
    body: &ObjectBody<'_>,
) -> crate::Result<()> {
    let size = match body {
        ObjectBody::Read(bytes) => bytes.len() as u64,
        ObjectBody::TooLarge(size) => *size as u64,
    };
    anyhow::ensure!(
        expected
            .get(at)
            .is_some_and(|(want_oid, want_kind, want_size)| want_oid == oid
                && want_kind == kind
                && *want_size == size),
        "prepared object body differs from its header"
    );
    Ok(())
}

pub(super) fn scan_blob_payload(
    context: &BlobScanContext<'_>,
    oid: &str,
    bytes: &[u8],
    labels: &HashMap<String, String>,
    binary: &Cell<u64>,
    out: &mut HitCollector,
) -> crate::Result<()> {
    // The strict batch headers reserve this complete body in the shared aggregate budget.
    let mut reserved = bytes.len() as u64;
    match inspection::read(
        bytes,
        reserved,
        None,
        context.limits.max_object_bytes,
        &mut reserved,
    )? {
        inspection::Payload::Text(text) => {
            let report = if let Some(view) = protocol_blob_view(context, oid, &text, labels) {
                scan_text_capped_registered_views(
                    &text,
                    &view,
                    context.allowlist,
                    Policy::CLIENT,
                    out.remaining(),
                    context.registered,
                )
            } else {
                scan_repository_payload_capped(
                    &text,
                    context.allowlist,
                    context.trusted_identities,
                    Policy::CLIENT,
                    out.remaining(),
                    context.registered,
                )
            };
            if report.truncated {
                out.mark_truncated();
            }
            let file = labels
                .get(oid)
                .map(|path| format!("blob object {}/{path}", &oid[..8]));
            out.extend(report.hits.into_iter().map(|mut hit| {
                hit.file.clone_from(&file);
                hit.source = Source::BlobObject;
                hit
            }));
        }
        inspection::Payload::Binary => binary.set(binary.get().saturating_add(1)),
        inspection::Payload::TooLarge => {
            anyhow::bail!("prepared Git text exceeds its inspection bound")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;

    fn pointer(bytes: &[u8]) -> Pointer {
        Pointer {
            oid: hex::encode(sha2::Sha256::digest(bytes)),
            size: bytes.len() as u64,
        }
    }

    fn policy() -> CapturedPolicy {
        CapturedPolicy {
            allowlist: HashSet::new(),
            #[cfg(feature = "secret-vault")]
            registered: RegisteredMatcher::for_test(&[]),
            #[cfg(not(feature = "secret-vault"))]
            registered: RegisteredMatcher,
        }
    }

    #[test]
    fn lfs_identity_mask_is_scoped_to_the_validated_pointer_field() {
        let value = pointer(b"published content");
        let text = format!(
            "version {}\noid sha256:{}\nsize {}\n",
            crate::domain::lfs::VERSION,
            value.oid,
            value.size
        );
        let policy = policy();
        let masked = pointer_scan_view(&text, &policy.registered).unwrap();
        assert!(!masked.contains(&value.oid));
        let adjacent = format!("{text}token {}\n", value.oid);
        assert!(
            pointer_scan_view(&adjacent, &policy.registered)
                .unwrap()
                .contains(&value.oid)
        );
        assert!(
            pointer_scan_view(&format!("oid sha256:{}\n", value.oid), &policy.registered).is_none()
        );
        #[cfg(feature = "secret-vault")]
        assert!(
            pointer_scan_view(
                &text,
                &RegisteredMatcher::for_test(&[("explicit", &value.oid)])
            )
            .is_none()
        );
    }

    #[test]
    fn staged_readers_share_the_remaining_byte_budget() {
        let policy = policy();
        let bytes = b"plain content";
        let pointer = pointer(bytes);
        let mut inspector = Inspector::new(
            &policy,
            ScanLimits {
                budget_bytes: pointer.size * 2 - 1,
                max_object_bytes: pointer.size,
            },
        );
        assert!(!inspector.lfs(bytes.as_slice(), &pointer).unwrap());
        assert_eq!(
            inspector.lfs(bytes.as_slice(), &pointer),
            Err(InspectionFailure::Incomplete)
        );
        let (report, _) = inspector.finish();
        assert!(report.unscanned.over_budget.is_some());
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn staged_registered_json_values_honor_allowlists_but_ignore_inline_waivers() {
        let secret = "blue \"horse\" battery";
        let mut policy = CapturedPolicy {
            allowlist: HashSet::new(),
            registered: RegisteredMatcher::for_test(&[("sec_owned", secret)]),
        };
        let text =
            serde_json::json!({"message": format!("{secret} agit:allow-secret")}).to_string();
        assert!(!text.contains(secret));
        let pointer = pointer(text.as_bytes());
        let mut inspector = Inspector::new(&policy, ScanLimits::DEFAULT);
        assert!(!inspector.lfs(text.as_bytes(), &pointer).unwrap());
        let (report, _) = inspector.finish();
        assert!(!report.truncated);
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].rule, "registered-secret");
        assert_eq!(report.hits[0].redacted, "[redacted:registered-secret]");

        policy.allowlist.insert(secret.into());
        let mut inspector = Inspector::new(&policy, ScanLimits::DEFAULT);
        assert!(!inspector.lfs(text.as_bytes(), &pointer).unwrap());
        let (report, _) = inspector.finish();
        assert!(!report.truncated);
        assert!(report.hits.is_empty());
    }

    #[test]
    fn fresh_batch_sizes_share_reservations_before_any_body_read() {
        let remaining = Cell::new(10);
        let first = vec![
            ("1".repeat(40), "blob".into(), 3),
            ("2".repeat(40), "tree".into(), 4),
        ];
        reserve_objects(&first, 10, &remaining).unwrap();
        assert_eq!(remaining.get(), 3);
        let later = vec![
            ("3".repeat(40), "commit".into(), 2),
            ("4".repeat(40), "tag".into(), 2),
        ];
        let error = reserve_objects(&later, 10, &remaining).unwrap_err();
        assert!(error.downcast_ref::<BudgetSpent>().is_some());
        assert_eq!(
            remaining.get(),
            3,
            "a refused batch must not begin reading bodies"
        );
        let oversized = vec![("5".repeat(40), "blob".into(), 11)];
        reserve_objects(&oversized, 10, &remaining).unwrap();
        assert_eq!(
            remaining.get(),
            3,
            "oversized bodies remain unread and enter the unscanned ledger"
        );
    }

    #[test]
    fn prepared_body_headers_cannot_silently_substitute_or_drop_carriers() {
        let oid = "1".repeat(40);
        let expected = vec![(oid.clone(), "commit".into(), 4)];
        assert!(check_body(&expected, 0, &oid, "commit", &ObjectBody::Read(b"body")).is_ok());
        assert!(
            check_body(
                &expected,
                0,
                &"2".repeat(40),
                "commit",
                &ObjectBody::Read(b"body")
            )
            .is_err()
        );
        assert!(check_body(&expected, 0, &oid, "blob", &ObjectBody::Read(b"body")).is_err());
        assert!(check_body(&expected, 0, &oid, "commit", &ObjectBody::Read(b"short")).is_err());
        assert!(check_body(&expected, 1, &oid, "commit", &ObjectBody::Read(b"body")).is_err());
    }

    #[test]
    fn captured_snapshots_do_not_rewalk_commit_parents() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        for message in ["parent", "snapshot"] {
            repo.git(&[
                "-c",
                "user.name=Inspection fixture",
                "-c",
                "user.email=inspection@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--no-gpg-sign",
                "--allow-empty",
                "-m",
                message,
            ])
            .unwrap();
        }
        let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_owned();
        let commits = vec![head.clone()];
        let local = repo.exact_root_inspection();
        let mut captured = Vec::new();
        HistorySelection::Snapshots(&commits)
            .stream(&local, &["rev-list"], |record| {
                captured.push(String::from_utf8(record.to_vec())?);
                Ok(())
            })
            .unwrap();
        assert_eq!(captured, vec![head]);
        let mut ordinary = Vec::new();
        HistorySelection::Frozen(&commits)
            .stream(&local, &["rev-list"], |record| {
                ordinary.push(String::from_utf8(record.to_vec())?);
                Ok(())
            })
            .unwrap();
        assert_eq!(ordinary.len(), 2);
    }
}
