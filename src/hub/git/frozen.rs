//! Publication uses a private Git configuration and literal object sources after preparation.
//!
//! This binds transport, not review policy or an atomic transaction across requests. The object
//! store remains borrowed: pruning captured objects can make publication fail. Git, platform
//! trust stores and explicitly selected proxy or certificate files remain trusted dependencies.

use super::{PublicationAttempt, PublicationPhase, RemoteRefs, TransportIdentity};
use crate::domain::repo::publication::{FrozenRef, PublicationPlan};
use crate::domain::repo::{
    Repo, bounded_inspection_output, bounded_inspection_output_from, inspection_git_path_spelling,
};
use crate::hub::identity::RemoteIdentity;
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONFIG_LIMIT: usize = 64 * 1024;
const REQUEST_UNITS: usize = 24 * 1024;
const LFS_REMOTE: &str = "agit-publication";
/// Bounds one history listing used to plan staged requests.
const STAGE_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;

#[path = "captured_publication.rs"]
mod captured;
#[path = "frozen_http.rs"]
mod http;
#[path = "frozen_lfs_cache.rs"]
mod lfs_cache;
#[path = "frozen_lfs_download.rs"]
mod lfs_download;
pub use captured::CapturedPublication;
#[path = "prepared_publication.rs"]
mod prepared;
pub use prepared::{
    BlockedContentInspection, BlockedInspection, CompleteContentInspection, CompleteInspection,
    ContentInspection, InspectionReport, PreparedPayloadAvailability, PreparedPublication,
    PublicationInspection,
};

/// A prepared destination and publication plan survive ambient configuration and ref changes.
/// Local preparation creates temporary content owners; it performs no network request.
/// Callers enforce review and confirmation before push.
pub struct FrozenPublication {
    transport: TransportIdentity,
    directory: tempfile::TempDir,
    url: String,
    identity: RemoteIdentity,
    http: std::result::Result<(String, ureq::Agent), http::Failure>,
    lfs_objects: std::result::Result<PathBuf, lfs_cache::Failure>,
    lfs_preferences: std::result::Result<BTreeMap<String, String>, http::Failure>,
    plan: PublicationPlan,
    scope: crate::domain::repo::publication::InspectionScope,
    lfs_inventory: Vec<crate::domain::lfs::Pointer>,
    heads: Vec<Vec<FrozenRef>>,
    tags: Vec<Vec<FrozenRef>>,
    /// Replaces the bounds the destination Hub publishes.
    push_limits: Option<crate::hub::PushLimits>,
}

/// One receive-pack request: the refs it moves and the receipt policy it confirms.
struct ReceiveBatch<'a> {
    refs: Vec<FrozenRef>,
    policy: Option<std::borrow::Cow<'a, crate::hub::privacy::publication::PublicationPolicy>>,
}

/// How far [`FrozenPublication::fits`] goes to prove that a request fits.
#[derive(Clone, Copy)]
enum Measure {
    /// The size bound alone, which needs nothing from the destination.
    BoundOnly,
    /// The bound, then the pack the push would send.
    Pack,
}

/// A fast-forward of one branch to an ancestor of its frozen tip, and the commits it introduces.
struct Stage {
    reference: FrozenRef,
    introduced: BTreeSet<String>,
}

impl FrozenPublication {
    /// Query a destination with the selected repository identity without capturing or pushing
    /// the public objects. Probes share publication's isolated routing and authentication.
    /// An unavailable or malformed advertisement remains unknown.
    pub fn advertised_refs_for(
        repo: &Repo,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Option<RemoteRefs> {
        let source = Source::new(repo).ok()?;
        let (url, client) = Self::destination(&source, canonical_url, identity).ok()?;
        let directory = captured::private_git_directory("sha1").ok()?;
        let transport = Self::prepare_transport(
            &source,
            directory.path(),
            &url,
            identity,
            Some(client),
            "http:https",
        )
        .ok()?;
        let args = super::remote_ref_args(&url, true);
        let output = super::capture_transport(directory.path(), &args, Some(&transport))?;
        super::parse_remote_refs(&output)
    }

    /// A true result means every generated public branch and tag has the exact advertised OID.
    /// Extra remote refs do not affect the selected publication and are left untouched.
    pub fn advertised_refs_match(
        repo: &Repo,
        plan: &PublicationPlan,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Option<bool> {
        let refs = Self::advertised_refs_for(repo, canonical_url, identity)?;
        Some(Self::refs_match(plan, &refs))
    }

    /// [`Self::advertised_refs_match`] against an advertisement this publication already read.
    pub fn refs_match(plan: &PublicationPlan, refs: &RemoteRefs) -> bool {
        plan.heads().iter().chain(plan.tags()).all(|reference| {
            refs.refs
                .get(reference.name())
                .is_some_and(|oid| oid == reference.oid())
        })
    }

    /// The URL must be the canonical clone URL selected alongside the immutable remote identity.
    /// Named remotes are deliberately absent because they can select multiple push destinations.
    /// Supported HTTP preferences are captured once; unsupported HTTP preferences fail explicitly.
    /// Ambient routing settings do not participate in this context.
    pub fn prepare(
        repo: &Repo,
        plan: &PublicationPlan,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Result<Self> {
        let source = Source::new(repo)?;
        let (url, client) = Self::destination(&source, canonical_url, identity)?;
        Self::from_source(
            source,
            plan,
            url,
            identity.clone(),
            Some(client),
            "http:https",
        )
    }

    fn destination(
        source: &Source,
        canonical_url: &str,
        identity: &RemoteIdentity,
    ) -> Result<(String, crate::hub::Client)> {
        Self::destination_with_client(
            source,
            canonical_url,
            identity,
            crate::hub::Client::for_stored_hub(&identity.hub),
        )
    }

    fn destination_with_client(
        source: &Source,
        canonical_url: &str,
        identity: &RemoteIdentity,
        client: crate::hub::Client,
    ) -> Result<(String, crate::hub::Client)> {
        let normalized = RemoteIdentity::new(&identity.hub, &identity.agent_id)?;
        ensure!(
            normalized == *identity,
            "frozen publication identity is not canonical"
        );
        let url = super::require_transport_url(canonical_url, identity)?
            .context("frozen publication requires a canonical HTTP Hub URL")?;
        ensure!(
            url == canonical_url,
            "frozen publication URL is not canonical"
        );
        ensure!(
            !url.chars().any(|character| character.is_control()
                || character.is_whitespace()
                || character == '\\'),
            "frozen publication URL contains non-canonical characters"
        );
        source.verify_identity(identity)?;
        ensure!(
            crate::hub::identity::normalize_hub(client.base())? == identity.hub,
            "publication client belongs to another Hub"
        );
        client.checked_access_token()?;
        Ok((url, client))
    }

    fn from_source(
        source: Source,
        plan: &PublicationPlan,
        url: String,
        identity: RemoteIdentity,
        client: Option<crate::hub::Client>,
        protocols: &str,
    ) -> Result<Self> {
        let content = captured::GitContent::capture(&source, plan)?;
        Self::from_captured(source, content, url, identity, client, protocols)
    }

    fn from_captured(
        source: Source,
        content: captured::GitContent,
        url: String,
        identity: RemoteIdentity,
        client: Option<crate::hub::Client>,
        protocols: &str,
    ) -> Result<Self> {
        let captured::GitContent {
            directory,
            format,
            lfs_objects,
            plan,
            lfs_inventory,
            scope,
            baseline: _,
        } = content;
        let lfs_url = format!("{}/info/lfs", url.trim_end_matches('/'));
        let batch_url = format!("{lfs_url}/objects/batch");
        let lfs_preferences = source
            .http_preferences(&batch_url)
            .map_err(|_| http::Failure::Configuration);
        let http = lfs_preferences
            .as_ref()
            .map_err(|error| *error)
            .and_then(|values| http::prepare(&batch_url, values, &source.environment))
            .map(|agent| (lfs_url, agent));
        let transport = Self::prepare_transport(
            &source,
            directory.path(),
            &url,
            &identity,
            client,
            protocols,
        )?;
        ensure!(
            !plan.heads().is_empty(),
            "frozen publication has no branch roots"
        );
        let oid_length = if format == "sha256" { 64 } else { 40 };
        let heads = batches(plan.heads(), &url, oid_length)?;
        let tags = batches(plan.tags(), &url, oid_length)?;
        Ok(Self {
            transport,
            directory,
            url,
            identity,
            http,
            lfs_objects,
            lfs_preferences,
            plan,
            lfs_inventory,
            scope,
            heads,
            tags,
            push_limits: None,
        })
    }

    fn prepare_transport(
        source: &Source,
        directory: &Path,
        url: &str,
        identity: &RemoteIdentity,
        client: Option<crate::hub::Client>,
        protocols: &str,
    ) -> Result<TransportIdentity> {
        let mut preferences = source.http_preferences(url)?;
        insert_execution_constraints(&mut preferences, directory)?;
        let parameters = parameters(&preferences);
        validate_prepared_parameters(&parameters)?;
        let mut environment = source.process_environment();
        for name in ["SSL_CERT_FILE", "SSL_CERT_DIR"] {
            ensure!(
                !source.environment.contains_key(OsStr::new(name)),
                "frozen publication cannot preserve backend-specific {name}; configure Git http.sslCAInfo or http.sslCAPath explicitly"
            );
        }
        for (key, value) in [
            ("HOME", directory.join("home").into_os_string()),
            ("USERPROFILE", directory.join("home").into_os_string()),
            ("XDG_CONFIG_HOME", directory.join("home").into_os_string()),
            ("GIT_DIR", directory.as_os_str().to_owned()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            (
                "GIT_CONFIG_SYSTEM",
                directory.join("empty-config").into_os_string(),
            ),
            (
                "GIT_CONFIG_GLOBAL",
                directory.join("empty-config").into_os_string(),
            ),
            ("GIT_ALLOW_PROTOCOL", protocols.into()),
            ("GIT_NO_LAZY_FETCH", "1".into()),
            ("GIT_OPTIONAL_LOCKS", "0".into()),
            ("GIT_ATTR_NOSYSTEM", "1".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
        ] {
            environment.insert(key.into(), value);
        }
        let urls = client
            .as_ref()
            .map(|_| vec![url.to_owned()])
            .unwrap_or_default();
        let transport = TransportIdentity {
            client,
            urls,
            agent_id: Some(identity.agent_id.clone()),
            accept_secret_findings: false,
            lfs: None,
            execution: Some(Execution {
                root: directory.to_owned(),
                environment,
                parameters,
            }),
        };
        transport.environment()?;
        Ok(transport)
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn identity(&self) -> &RemoteIdentity {
        &self.identity
    }

    /// The availability client is captured with this destination and account. It retains the
    /// ordinary API's WebPki baseline, explicit supported proxy/CA/header preferences and bounds.
    /// Unsupported availability preferences do not prevent Git-only publication preparation.
    /// This accessor performs no requests and does not select an LFS publication policy.
    pub fn prepared_lfs_http(&self) -> Result<(&str, &ureq::Agent)> {
        self.http
            .as_ref()
            .map(|(endpoint, agent)| (endpoint.as_str(), agent))
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// The original cache path is captured without creating or inspecting cached payloads.
    /// A missing cache is valid; errors remain deferred until local payloads are required.
    /// The path fixes lookup configuration, not file identity or bytes behind filesystem links.
    pub fn source_lfs_objects(&self) -> Result<&Path> {
        self.lfs_objects
            .as_deref()
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    /// Advertisement and publication share the prepared endpoint, account and configuration.
    pub fn advertised_refs(&self) -> Option<RemoteRefs> {
        let output = super::capture_transport(
            self.directory.path(),
            &super::remote_ref_args(&self.url, true),
            Some(&self.transport),
        )?;
        super::parse_remote_refs(&output)
    }

    /// Every attempt retains its observations, including effects preceding a failed retry.
    /// Tags are attempted only after all branch batches are affirmatively acknowledged.
    pub(super) fn push_refs(&self) -> (PublicationPhase, Option<PublicationPhase>) {
        self.push_refs_confirmed(None, None)
    }

    pub(super) fn session_privacy_policy(
        &self,
        policy_digest: &str,
        manifest: &crate::domain::session_privacy::SessionPublicationManifest,
        visibility: &str,
    ) -> Result<crate::hub::privacy::publication::PublicationPolicy> {
        manifest.validate()?;
        let declared = manifest
            .snapshots
            .iter()
            .map(|snapshot| snapshot.commit())
            .chain(manifest.preserved_commits.iter().map(String::as_str))
            .collect::<std::collections::BTreeSet<_>>();
        let captured = self
            .plan
            .commit_objects()
            .iter()
            .map(String::as_str)
            .collect();
        ensure!(
            declared == captured && manifest.repository_id == self.identity.agent_id,
            "session manifest differs from the inspected publication"
        );
        self.verify_privacy_ref_updates()?;
        let client = self
            .transport
            .client
            .as_ref()
            .context("publication needs a Hub account")?;
        let repository = self
            .url
            .strip_prefix(&format!("{}/", self.identity.hub))
            .and_then(|url| url.strip_suffix(".git"))
            .context("publication destination is not a repository URL")?;
        crate::hub::privacy::publication::PublicationPolicy::register_sessions(
            client,
            repository,
            &self.identity,
            policy_digest,
            manifest,
            visibility,
        )
    }

    /// A history larger than one request may carry is delivered first as fast-forward stages,
    /// each confirming a receipt for exactly the commits it introduces; the frozen batches then
    /// follow unchanged and acknowledge the tips. A failed stage leaves the destination at a
    /// published prefix, which the next push continues from.
    ///
    /// `held` names the advertised branch heads the publication's held history was read from;
    /// every receipt binds a listing that still has them, or has them where this publication's
    /// own requests moved them.
    pub(super) fn push_refs_confirmed(
        &self,
        policy: Option<&crate::hub::privacy::publication::PublicationPolicy>,
        held: Option<&BTreeMap<String, String>>,
    ) -> (PublicationPhase, Option<PublicationPhase>) {
        let staged = self.stages().and_then(|stages| {
            stages
                .into_iter()
                .map(|stage| {
                    let policy = policy
                        .map(|policy| policy.scoped(&stage.introduced))
                        .transpose()?
                        .map(std::borrow::Cow::Owned);
                    Ok(ReceiveBatch {
                        refs: vec![stage.reference],
                        policy,
                    })
                })
                .collect::<Result<Vec<_>>>()
        });
        let mut batches = match staged {
            Ok(batches) => batches,
            Err(error) => {
                let phase = PublicationPhase {
                    attempts: Vec::new(),
                    unattempted: self.heads.iter().flatten().cloned().collect(),
                    error: Some(format!("{error:#}")),
                    withdrawn: Vec::new(),
                };
                return (phase, None);
            }
        };
        batches.extend(self.batches(&self.heads, policy));
        // A session publication sends its tips and every tag in one receive when they fit one: its
        // receipt makes the receive atomic, so a tag still never lands without its branch, and the
        // publication costs one receipt and one receive fewer. Tags that do not fit follow their
        // branch's acknowledged receive.
        let mut tags = self.tags.clone();
        let mut alone = None;
        if let Some(last) = batches
            .last_mut()
            .filter(|last| matches!(last.policy, Some(std::borrow::Cow::Borrowed(_))))
            && let Some(together) = self.together(&last.refs, &tags)
        {
            alone = Some(std::mem::replace(&mut last.refs, together));
            tags.clear();
        }
        // Asked once, after the stages have read the limits, so every receipt of this
        // publication sends the same request.
        let planned_refs = policy.is_some()
            && self
                .transport
                .client
                .as_ref()
                .is_some_and(crate::hub::Client::accepts_planned_refs);
        let mut heads = self.push_phase(&batches, held, planned_refs);
        // A refused receipt sends nothing. When the Hub refused only tags of the shared receive as
        // withdrawn, the tips go alone and the tags follow as they would unshared, so a withdrawn
        // tag holds back itself and not its branch.
        if let Some(alone) = alone
            && !heads.withdrawn.is_empty()
            && heads
                .withdrawn
                .iter()
                .all(|name| name.starts_with("refs/tags/"))
        {
            let last = batches.len() - 1;
            batches[last].refs = alone;
            let retried = self.push_phase_from(&batches, last, held, planned_refs);
            heads.attempts.extend(retried.attempts);
            heads.unattempted = retried.unattempted;
            heads.error = retried.error;
            heads.withdrawn = retried.withdrawn;
            tags = self.tags.clone();
        }
        // A stage is a means of delivery; only the frozen tips and tags remain owed after a failure.
        heads.unattempted.retain(|reference| {
            self.plan.heads().contains(reference) || self.plan.tags().contains(reference)
        });
        let tags = heads
            .ok()
            .then(|| self.push_phase(&self.batches(&tags, policy), held, planned_refs));
        (heads, tags)
    }

    /// `heads` and every one of `tags` as one batch, when they fit one command line together.
    fn together(&self, heads: &[FrozenRef], tags: &[Vec<FrozenRef>]) -> Option<Vec<FrozenRef>> {
        if tags.is_empty() {
            return None;
        }
        let refs: Vec<FrozenRef> = heads.iter().chain(tags.iter().flatten()).cloned().collect();
        let oid_length = refs.first()?.oid().len();
        let mut batches = batches(&refs, &self.url, oid_length).ok()?;
        (batches.len() == 1).then(|| batches.remove(0))
    }

    fn batches<'a>(
        &self,
        refs: &[Vec<FrozenRef>],
        policy: Option<&'a crate::hub::privacy::publication::PublicationPolicy>,
    ) -> Vec<ReceiveBatch<'a>> {
        refs.iter()
            .map(|refs| ReceiveBatch {
                refs: refs.clone(),
                policy: policy.map(std::borrow::Cow::Borrowed),
            })
            .collect()
    }

    fn limits(&self) -> crate::hub::PushLimits {
        if let Some(limits) = self.push_limits {
            return limits;
        }
        // An unreadable configuration plans by the bounds of a Hub that publishes none.
        self.transport
            .client
            .as_ref()
            .and_then(|client| client.push_limits().ok())
            .unwrap_or(crate::hub::PushLimits::UNPUBLISHED)
    }

    /// Stages that keep every request within the destination's push limits, or none when the
    /// whole publication fits in one request. Each stage moves one branch along its first-parent
    /// line, so it is a fast-forward of what the branch published before and keeps the branch's
    /// own line as its history.
    ///
    /// The destination's own limits decide, whatever their size: a Hub may publish less than the
    /// unpublished bounds, and a request planned by those is refused there on every retry. A
    /// history that fits the limits as a whole is delivered as before without reading the
    /// destination's refs; only a longer one is planned against them.
    ///
    /// The plan's boundary is history the destination held when the plan was taken, so every
    /// walk here stops at it, as at the destination's current tips.
    ///
    /// A branch the destination already has moves only to descendants of its tip there, or the
    /// request is not a fast-forward and is refused. When that tip arrives through a merge, the
    /// first stage is the merge or a later commit; when no stage can be both a fast-forward and
    /// within the limits, the publication is refused before any request.
    fn stages(&self) -> Result<Vec<Stage>> {
        let tips: Vec<&str> = self.plan.heads().iter().map(FrozenRef::oid).collect();
        let limits = self.limits();
        let mut published = self.plan.boundary().to_vec();
        if self.fits(&tips, &published, limits, Measure::BoundOnly)? {
            return Ok(Vec::new());
        }
        let remote = self.privacy_remote_refs()?;
        let present = self.present(remote.values())?;
        published.extend(present.iter().cloned());
        if self.fits(&tips, &published, limits, Measure::Pack)? {
            return Ok(Vec::new());
        }
        let mut stages = Vec::new();
        for head in self.plan.heads() {
            let branch = head
                .name()
                .strip_prefix("refs/heads/")
                .unwrap_or(head.name());
            let mut line =
                self.rev_list(&["--first-parent", "--reverse"], &[head.oid()], &published)?;
            if let Some(tip) = remote.get(head.name())
                && !line.is_empty()
            {
                // Along a first-parent line every commit after a descendant of the tip is one too,
                // so the cuts that fast-forward the branch are a suffix of the line.
                let first = if present.contains(tip) {
                    let (mut low, mut high) = (0, line.len());
                    while low < high {
                        let middle = low + (high - low) / 2;
                        if self.is_ancestor(tip, &line[middle])? {
                            high = middle;
                        } else {
                            low = middle + 1;
                        }
                    }
                    low
                } else {
                    line.len()
                };
                ensure!(
                    first < line.len(),
                    "{branch}: the hub's branch is at {}, which this history does not continue; \
                     it cannot be published as a fast-forward",
                    &tip[..tip.len().min(12)]
                );
                line.drain(..first);
            }
            let mut start = 0;
            while start < line.len() {
                ensure!(
                    self.fits(&[line[start].as_str()], &published, limits, Measure::Pack)?,
                    "{branch}: the smallest fast-forward step, to {}, is more than the hub accepts \
                     in one push ({} new commits, {} MiB); it cannot be split further. Ask the hub \
                     operator to raise AGIT_PUSH_MAX_NEW_COMMITS or AGIT_PUSH_TRANSPORT_LIMIT_MB.",
                    &line[start][..12],
                    limits.max_new_commits,
                    limits.max_request_bytes / (1024 * 1024)
                );
                // A first-parent step introduces at least itself, so no stage reaches further than
                // the commit budget along the line; within that, the furthest cut that fits.
                let (mut low, mut high) =
                    (start, (start + limits.max_new_commits).min(line.len()) - 1);
                while low < high {
                    let middle = low + (high - low).div_ceil(2);
                    if self.fits(&[line[middle].as_str()], &published, limits, Measure::Pack)? {
                        low = middle;
                    } else {
                        high = middle - 1;
                    }
                }
                let cut = &line[low];
                let introduced = self.rev_list(&[], &[cut.as_str()], &published)?;
                stages.push(Stage {
                    reference: head.staged(cut),
                    introduced: introduced.into_iter().collect(),
                });
                published.push(cut.clone());
                start = low + 1;
            }
        }
        Ok(stages)
    }

    /// Whether one request moving refs to `tips` stays within `limits` against `published`.
    ///
    /// The objects' uncompressed sizes, with what compression can add to each, bound the pack from
    /// above whatever is deltified, so a request under that bound needs no pack. Stored sizes do
    /// not: an object stored as a delta against a base the destination lacks is sent whole. A
    /// larger request is sized by building the thin pack the push would send, which stops as soon
    /// as it crosses the bound, unless `measure` only asks for the bound. The request keeps a
    /// sixteenth of the limit for its commands and framing.
    fn fits(
        &self,
        tips: &[&str],
        published: &[String],
        limits: crate::hub::PushLimits,
        measure: Measure,
    ) -> Result<bool> {
        let commits = self.rev_list(&[], tips, published)?;
        if commits.len() > limits.max_new_commits {
            return Ok(false);
        }
        let bound = limits.max_request_bytes - limits.max_request_bytes / 16;
        let objects = self.git_with_input(
            &[
                "rev-list",
                "--objects",
                "--no-object-names",
                "--stdin",
                "--",
            ],
            &revisions(tips, published),
        )?;
        let sizes = self.git_with_input(&["cat-file", "--batch-check=%(objectsize)"], &objects)?;
        let mut ceiling = 0u64;
        for line in sizes.lines() {
            let size = line
                .parse::<u64>()
                .context("publication object sizes are malformed")?;
            // A deflate stream grows incompressible input by a few bytes per block; the entry
            // header adds a few more.
            ceiling = ceiling.saturating_add(size + size / 1024 + 64);
        }
        if ceiling <= bound {
            return Ok(true);
        }
        match measure {
            Measure::BoundOnly => Ok(false),
            Measure::Pack => self.pack_within(tips, published, bound),
        }
    }

    /// Whether the thin pack for `tips` against `published` is at most `bound` bytes.
    fn pack_within(&self, tips: &[&str], published: &[String], bound: u64) -> Result<bool> {
        use std::io::Read as _;
        let input = self.planning_input(&revisions(tips, published))?;
        let mut command = self
            .transport
            .execution
            .as_ref()
            .context("publication has no captured Git context")?
            .command();
        command
            .env("GIT_ALLOW_PROTOCOL", "")
            .args([
                "pack-objects",
                "--revs",
                "--thin",
                "--stdout",
                "--delta-base-offset",
                "-q",
            ])
            .stdin(input)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command
            .spawn()
            .context("publication pack size could not be measured")?;
        let mut stdout = child.stdout.take().expect("piped pack output");
        let mut buffer = vec![0; 1 << 16];
        let mut size = 0u64;
        let within = loop {
            let read = match stdout.read(&mut buffer) {
                Ok(read) => read,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error).context("publication pack size could not be measured");
                }
            };
            if read == 0 {
                break true;
            }
            size += read as u64;
            if size > bound {
                break false;
            }
        };
        if !within {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(false);
        }
        let status = child
            .wait()
            .context("publication pack size could not be measured")?;
        ensure!(
            status.success(),
            "publication pack size could not be measured"
        );
        Ok(true)
    }

    /// The `oids` present in the captured repository. A remote target absent locally has no local
    /// history, so leaving it out of an exclusion changes nothing.
    fn present<'a>(&self, oids: impl Iterator<Item = &'a String>) -> Result<Vec<String>> {
        let oids: BTreeSet<&String> = oids.collect();
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = oids.iter().map(|oid| format!("{oid}\n")).collect();
        let listing = self.git_with_input(
            &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
            &input,
        )?;
        Ok(listing
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, kind)| *kind != "missing")
            .map(|(oid, _)| oid.to_owned())
            .collect())
    }

    /// Commits reachable from `tips` and not from `published`, oldest first when `options` asks.
    fn rev_list(
        &self,
        options: &[&str],
        tips: &[&str],
        published: &[String],
    ) -> Result<Vec<String>> {
        let mut args = vec!["rev-list"];
        args.extend(options);
        args.extend(["--stdin", "--"]);
        let listing = self.git_with_input(&args, &revisions(tips, published))?;
        listing
            .lines()
            .map(|oid| {
                ensure!(
                    matches!(oid.len(), 40 | 64)
                        && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "publication history listing is malformed"
                );
                Ok(oid.to_owned())
            })
            .collect()
    }

    /// Whether `ancestor` is reachable from `descendant` in the captured repository; both are
    /// present there.
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let mut command = self
            .transport
            .execution
            .as_ref()
            .context("publication has no captured Git context")?
            .command();
        command.env("GIT_ALLOW_PROTOCOL", "").args([
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ]);
        let output = bounded_inspection_output(command, CONFIG_LIMIT)?;
        ensure!(
            output.stderr.is_empty() && matches!(output.status.code(), Some(0 | 1)),
            "publication history cannot be planned within the hub's push limits"
        );
        Ok(output.status.success())
    }

    /// Planning input on a file, so a revision list of any length stays off the command line.
    fn planning_input(&self, input: &str) -> Result<std::process::Stdio> {
        let mut file = tempfile::tempfile_in(self.directory.path())
            .context("publication planning input could not be staged")?;
        std::io::Write::write_all(&mut file, input.as_bytes())
            .and_then(|()| std::io::Seek::rewind(&mut file))
            .context("publication planning input could not be staged")?;
        Ok(file.into())
    }

    fn git_with_input(&self, args: &[&str], input: &str) -> Result<String> {
        let input = self.planning_input(input)?;
        let mut command = self
            .transport
            .execution
            .as_ref()
            .context("publication has no captured Git context")?
            .command();
        command.env("GIT_ALLOW_PROTOCOL", "").args(args);
        let output = bounded_inspection_output_from(command, input, STAGE_OUTPUT_LIMIT)?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "publication history cannot be planned within the hub's push limits"
        );
        String::from_utf8(output.stdout).context("publication history listing is not valid UTF-8")
    }

    /// `delivered` are the batches of this phase acknowledged before `batch`.
    fn receive_receipt(
        &self,
        policy: &crate::hub::privacy::publication::PublicationPolicy,
        batch: &[FrozenRef],
        held: Option<&BTreeMap<String, String>>,
        delivered: &[ReceiveBatch<'_>],
        planned_refs: bool,
        before: BTreeMap<String, String>,
    ) -> Result<crate::hub::privacy::publication::ReceiveReceipt> {
        if let Some(held) = held {
            self.verify_held_heads(held, &before, delivered)?;
        }
        let mut after = before.clone();
        for reference in batch {
            after.insert(reference.name().into(), reference.oid().into());
        }
        policy.confirm(
            self.transport
                .client
                .as_ref()
                .context("publication needs a Hub account")?,
            &before,
            &after,
            planned_refs,
        )
    }

    /// A publication declares as new only what the Hub lacked when its held history was read. If
    /// a branch head of that advertisement has since moved or gone, other than by this
    /// publication's own update, the declaration may leave out commits the Hub no longer holds,
    /// and their content would reach it without inspection; such a receipt is refused before
    /// anything is sent. Heads that appeared since only make the declaration larger than needed,
    /// which the Hub refuses itself.
    ///
    /// A head this publication moved itself is at its frozen tip or at a stage of it that an
    /// earlier request in `delivered` acknowledged. Every request is a fast-forward, so such a
    /// head still holds everything the advertisement had; refusing it would refuse every staged
    /// request after the first.
    fn verify_held_heads(
        &self,
        held: &BTreeMap<String, String>,
        before: &BTreeMap<String, String>,
        delivered: &[ReceiveBatch<'_>],
    ) -> Result<()> {
        for (name, oid) in held {
            let current = before.get(name);
            let own = current.is_some_and(|current| {
                self.plan
                    .heads()
                    .iter()
                    .chain(delivered.iter().flat_map(|batch| &batch.refs))
                    .any(|reference| reference.name() == name && reference.oid() == current)
            });
            ensure!(
                current == Some(oid) || own,
                "the destination's published history changed during this publication; publish again"
            );
        }
        Ok(())
    }

    pub(super) fn privacy_remote_refs(&self) -> Result<BTreeMap<String, String>> {
        let delays = [
            std::time::Duration::from_millis(250),
            std::time::Duration::from_secs(1),
        ];
        for attempt in 0..=delays.len() {
            if attempt > 0
                && let Some(client) = &self.transport.client
                && let Some(expected) = client.credential_snapshot()
            {
                let current = crate::infra::credentials::load_checked(client.base())?
                    .context("publication account is unavailable during destination-ref retry")?;
                ensure!(
                    expected.username == current.username
                        && expected
                            .account_id
                            .as_ref()
                            .is_none_or(|id| current.account_id.as_ref() == Some(id)),
                    "publication account changed during destination-ref retry"
                );
            }
            match super::ref_read::capture(
                self.directory.path(),
                &["ls-remote", "--refs", "--", &self.url],
                Some(&self.transport),
            ) {
                Ok(output) => {
                    return crate::hub::privacy::publication::parse_refs(&output)
                        .context("publication destination returned malformed refs");
                }
                Err(error) if error.transient && attempt < delays.len() => {
                    std::thread::sleep(delays[attempt])
                }
                Err(error) => {
                    let context = if error.transient {
                        "cannot read the complete publication destination refs after bounded retries: temporary Hub or network failure"
                    } else {
                        "cannot read the complete publication destination refs"
                    };
                    return Err(anyhow::Error::new(error).context(context));
                }
            }
        }
        unreachable!()
    }

    /// Divergent projections cannot replace the server strategy before an explicit migration.
    fn verify_privacy_ref_updates(&self) -> Result<()> {
        let before = self.privacy_remote_refs()?;
        for reference in self.plan.heads() {
            let Some(previous) = before.get(reference.name()) else {
                continue;
            };
            if previous == reference.oid() {
                continue;
            }
            let mut command = self
                .transport
                .execution
                .as_ref()
                .context("privacy publication has no captured Git context")?
                .command();
            command.env("GIT_ALLOW_PROTOCOL", "").args([
                "merge-base",
                "--is-ancestor",
                previous,
                reference.oid(),
            ]);
            let output = bounded_inspection_output(command, CONFIG_LIMIT)?;
            ensure!(
                output.status.success() && output.stderr.is_empty(),
                "privacy publication cannot fast-forward {}; choose a new branch or destination, or explicitly migrate the published history",
                reference.name()
            );
        }
        for reference in self.plan.tags() {
            ensure!(
                before
                    .get(reference.name())
                    .is_none_or(|previous| previous == reference.oid()),
                "privacy publication would replace published tag {}; explicit migration is required",
                reference.name()
            );
        }
        Ok(())
    }

    fn lfs_execution(&self, storage: &Path) -> Result<Execution> {
        let mut preferences = self
            .lfs_preferences
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .clone();
        let endpoint = &self
            .http
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
            .0;
        insert_execution_constraints(&mut preferences, self.directory.path())?;
        for (key, value) in [
            (format!("remote.{LFS_REMOTE}.url"), self.url.clone()),
            (format!("remote.{LFS_REMOTE}.pushurl"), self.url.clone()),
            ("lfs.storage".into(), path_text(storage)?),
        ] {
            preferences.insert(key, value);
        }
        let mut parameters = parameters(&preferences);
        super::constrain_lfs_parameters(&mut parameters, LFS_REMOTE, endpoint);
        validate_prepared_parameters(&parameters)?;
        let original = self
            .transport
            .execution
            .as_ref()
            .context("frozen publication execution is unavailable")?;
        Ok(Execution {
            root: original.root.clone(),
            environment: original.environment.clone(),
            parameters,
        })
    }

    /// A receipt the Hub refuses because the batch names refs withdrawn from the caller ends the
    /// phase like any other refused receipt, with those refs in `withdrawn`.
    fn push_phase(
        &self,
        batches: &[ReceiveBatch<'_>],
        held: Option<&BTreeMap<String, String>>,
        planned_refs: bool,
    ) -> PublicationPhase {
        self.push_phase_from(batches, 0, held, planned_refs)
    }

    /// [`Self::push_phase`] for the batches from `start` on; the ones before it count as delivered.
    fn push_phase_from(
        &self,
        batches: &[ReceiveBatch<'_>],
        start: usize,
        held: Option<&BTreeMap<String, String>>,
        planned_refs: bool,
    ) -> PublicationPhase {
        let mut phase = PublicationPhase::default();
        let owed = |from: usize| -> Vec<FrozenRef> {
            batches[from..]
                .iter()
                .flat_map(|batch| batch.refs.iter().cloned())
                .collect()
        };
        for (
            index,
            ReceiveBatch {
                refs: batch,
                policy,
            },
        ) in batches.iter().enumerate().skip(start)
        {
            let heads = batch
                .iter()
                .filter(|reference| reference.name().starts_with("refs/heads/"))
                .count();
            let batch_label = format!(
                "batch {}/{} ({heads} branches, {} tags)",
                index + 1,
                batches.len(),
                batch.len() - heads
            );
            let receipt_progress =
                crate::ui::publication::stage(format!("Getting receipt for {batch_label}"));
            // A receipt binds the listing it was read from, so refs that listing already holds at
            // their frozen object are acknowledged from it and never sent. Sending them makes the
            // Hub check the whole selection again for refs it cannot change.
            let mut current = Vec::new();
            let receipt = match policy
                .as_deref()
                .map(|policy| {
                    let before = self.privacy_remote_refs()?;
                    let (held_refs, pending): (Vec<_>, Vec<_>) =
                        batch.iter().cloned().partition(|reference| {
                            before.get(reference.name()).map(String::as_str)
                                == Some(reference.oid())
                        });
                    current = held_refs;
                    let receipt = (!pending.is_empty())
                        .then(|| {
                            self.receive_receipt(
                                policy,
                                &pending,
                                held,
                                &batches[..index],
                                planned_refs,
                                before,
                            )
                        })
                        .transpose()?;
                    Ok::<_, anyhow::Error>((pending, receipt))
                })
                .transpose()
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    if let Some(refused) =
                        error.downcast_ref::<crate::hub::privacy::publication::RefsWithdrawn>()
                    {
                        phase.withdrawn = refused.refs.clone();
                    }
                    phase.error = Some(format!("{error:#}"));
                    phase.unattempted = owed(index);
                    break;
                }
            };
            receipt_progress.finish();
            if !current.is_empty() {
                phase
                    .attempts
                    .push(PublicationAttempt::already_published(index, &current));
            }
            let (batch, receipt) = match receipt {
                Some((pending, receipt)) => (pending, receipt),
                None => (batch.clone(), None),
            };
            if batch.is_empty() {
                continue;
            }
            let sent_heads = batch
                .iter()
                .filter(|reference| reference.name().starts_with("refs/heads/"))
                .count();
            let uploading = crate::ui::publication::stage(format!(
                "Uploading and awaiting acknowledgement for batch {}/{} ({sent_heads} branches, {} tags; {} already published)",
                index + 1,
                batches.len(),
                batch.len() - sent_heads,
                current.len()
            ));
            let specs: Vec<_> = batch.iter().map(FrozenRef::refspec).collect();
            let mut args = vec![
                "push",
                "--porcelain",
                "--no-follow-tags",
                "--recurse-submodules=no",
                "--",
                &self.url,
            ];
            args.extend(specs.iter().map(String::as_str));
            if receipt.is_some() {
                args.insert(1, "--atomic");
            }
            let run = super::execute_transport_receipt(
                None,
                &args,
                &self.transport,
                super::OutputMode::Captured,
                None,
                receipt.as_ref(),
            );
            let attempted = !run.attempts.is_empty();
            phase.attempts.extend(
                run.attempts
                    .into_iter()
                    .map(|attempt| PublicationAttempt::parse(index, &batch, attempt)),
            );
            phase.error = run.error.map(|error| format!("{error:#}"));
            if phase.error.is_some() || !phase.attempts.last().is_some_and(PublicationAttempt::ok) {
                let start = if attempted { index + 1 } else { index };
                phase.unattempted = owed(start);
                break;
            }
            uploading.finish();
        }
        phase
    }
}

pub(super) struct Execution {
    root: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    pub(super) parameters: OsString,
}

impl Execution {
    #[cfg(all(test, feature = "cli"))]
    pub(super) fn fixture(
        root: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        parameters: OsString,
    ) -> Self {
        Self {
            root,
            environment,
            parameters,
        }
    }

    pub(super) fn validate_parameters(&self, parameters: &OsStr) -> Result<()> {
        ensure!(
            parameters
                .to_str()
                .context("frozen transport configuration is not valid UTF-8")?
                .encode_utf16()
                .count()
                <= 28 * 1024,
            "frozen transport configuration exceeds its environment limit"
        );
        Ok(())
    }

    pub(super) fn command(&self) -> Command {
        let mut command = super::transport_command();
        command
            .env_clear()
            .envs(&self.environment)
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .arg("--no-replace-objects")
            .current_dir(&self.root);
        crate::infra::git_runtime::configure(&mut command);
        command
    }
}

fn insert_execution_constraints(
    preferences: &mut BTreeMap<String, String>,
    directory: &Path,
) -> Result<()> {
    for (key, value) in [
        ("core.hooksPath", path_text(&directory.join("hooks"))?),
        ("core.fsmonitor", "false".into()),
        ("credential.helper", String::new()),
        ("credential.interactive", "false".into()),
        ("http.sslVerify", "true".into()),
        ("http.followRedirects", "false".into()),
        ("push.followTags", "false".into()),
        ("push.recurseSubmodules", "no".into()),
        ("push.negotiate", "false".into()),
        ("submodule.recurse", "false".into()),
    ] {
        preferences.insert(key.into(), value);
    }
    Ok(())
}

fn validate_prepared_parameters(parameters: &OsStr) -> Result<()> {
    ensure!(
        parameters.len() <= CONFIG_LIMIT,
        "frozen HTTP configuration exceeds its limit"
    );
    ensure!(
        parameters
            .to_str()
            .context("frozen HTTP configuration is not valid UTF-8")?
            .encode_utf16()
            .count()
            <= 16 * 1024,
        "frozen HTTP configuration exceeds its environment limit"
    );
    Ok(())
}

/// `rev-list --stdin` input: the tips, then every published tip excluded.
fn revisions(tips: &[&str], published: &[String]) -> String {
    let mut input = String::new();
    for tip in tips {
        input.push_str(tip);
        input.push('\n');
    }
    for oid in published {
        input.push('^');
        input.push_str(oid);
        input.push('\n');
    }
    input
}

fn batches(refs: &[FrozenRef], url: &str, oid_length: usize) -> Result<Vec<Vec<FrozenRef>>> {
    let overhead = argument_units(url)
        .checked_add(256)
        .context("publication URL is too long")?;
    ensure!(
        overhead < REQUEST_UNITS,
        "publication URL exceeds the command limit"
    );
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut units = overhead;
    for reference in refs {
        ensure!(
            reference.oid().len() == oid_length,
            "publication object format differs from its source"
        );
        let spec = reference.refspec();
        let size = argument_units(&spec);
        ensure!(
            size + overhead <= REQUEST_UNITS,
            "publication ref exceeds the command limit"
        );
        if units + size > REQUEST_UNITS {
            batches.push(std::mem::take(&mut batch));
            units = overhead;
        }
        batch.push(reference.clone());
        units += size;
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    Ok(batches)
}

fn argument_units(value: &str) -> usize {
    // Quoting can expand backslashes and embedded quotes on Windows, even in valid Git refs.
    value
        .encode_utf16()
        .count()
        .saturating_mul(2)
        .saturating_add(3)
}

struct Source {
    root: PathBuf,
    gitdir: &'static str,
    environment: BTreeMap<OsString, OsString>,
}

impl Source {
    fn verify_identity(&self, observed: &RemoteIdentity) -> Result<()> {
        let Some(expected) = self
            .environment
            .get(OsStr::new(crate::hub::identity::EXPECTED_AGENT_ID_ENV))
        else {
            return Ok(());
        };
        let expected = expected
            .to_str()
            .context("expected remote identity is not valid UTF-8")?;
        let mut command = self.command();
        // The pin belongs to the selected source's local configuration, not injected parameters.
        for key in ["GIT_CONFIG", "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"] {
            command.env_remove(key);
        }
        command.args(["config", "--local", "--get", "agit.remoteIdentity"]);
        let output = bounded_inspection_output(command, CONFIG_LIMIT)?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "the publication source has no readable immutable remote identity; re-clone it before supervised publication"
        );
        let pinned: RemoteIdentity = serde_json::from_slice(&output.stdout)
            .context("the publication source remote identity is malformed")?;
        let pinned = RemoteIdentity::new(&pinned.hub, &pinned.agent_id)?;
        let pinned =
            crate::hub::identity::constrain_expected(pinned, &observed.hub, Some(expected))?;
        crate::hub::identity::verify_expected_target(Some(&pinned), observed)
    }

    fn new(repo: &Repo) -> Result<Self> {
        #[cfg(windows)]
        let environment = std::env::vars_os()
            .map(|(key, value)| {
                let key = key
                    .to_str()
                    .map(|key| OsString::from(key.to_ascii_uppercase()))
                    .unwrap_or(key);
                (key, value)
            })
            .collect();
        #[cfg(not(windows))]
        let environment = std::env::vars_os().collect();
        Self::at(repo, environment)
    }

    fn at(repo: &Repo, environment: BTreeMap<OsString, OsString>) -> Result<Self> {
        let root = inspection_git_path_spelling(
            repo.root()
                .canonicalize()
                .context("publication source is unavailable")?,
        );
        ensure!(root.is_dir(), "publication source is not a directory");
        let gitdir = if root.join(".git").exists() {
            ".git"
        } else {
            ensure!(
                root.join("HEAD").is_file() && root.join("objects").is_dir(),
                "publication source is not an exact Git repository"
            );
            "."
        };
        let source = Self {
            root,
            gitdir,
            environment,
        };
        if gitdir == "." {
            ensure!(
                source.text(&["rev-parse", "--is-bare-repository"])? == "true",
                "publication source is not a bare Git repository"
            );
        }
        Ok(source)
    }

    fn command(&self) -> Command {
        let mut command = super::transport_command();
        command.env_clear().envs(self.process_environment());
        for (key, value) in &self.environment {
            let Some(name) = key.to_str() else { continue };
            if matches!(
                name,
                "HOME"
                    | "USERPROFILE"
                    | "XDG_CONFIG_HOME"
                    | "GIT_CONFIG_NOSYSTEM"
                    | "GIT_CONFIG_SYSTEM"
                    | "GIT_CONFIG_GLOBAL"
                    | "GIT_CONFIG_COUNT"
                    | "GIT_CONFIG_PARAMETERS"
            ) || name.starts_with("GIT_CONFIG_KEY_")
                || name.starts_with("GIT_CONFIG_VALUE_")
            {
                command.env(key, value);
            }
        }
        command
            .env("LC_ALL", "C")
            .env("LANGUAGE", "C")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(&self.root)
            .args(["--git-dir", self.gitdir, "-c", "core.fsmonitor=false"]);
        crate::infra::git_runtime::configure(&mut command);
        command
    }

    fn output(&self, args: &[&str]) -> Result<Output> {
        let mut command = self.command();
        command.args(args);
        bounded_inspection_output(command, CONFIG_LIMIT)
    }

    fn text(&self, args: &[&str]) -> Result<String> {
        let output = self.output(args)?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "publication source inspection failed"
        );
        let text = std::str::from_utf8(&output.stdout)
            .context("publication source path is not valid UTF-8")?;
        Ok(text
            .strip_suffix('\n')
            .context("publication source inspection is incomplete")?
            .to_owned())
    }

    fn process_environment(&self) -> BTreeMap<OsString, OsString> {
        self.environment
            .iter()
            .filter(|(key, _)| {
                key.to_str().is_some_and(|name| {
                    matches!(
                        name,
                        "PATH"
                            | "SystemRoot"
                            | "SYSTEMROOT"
                            | "WINDIR"
                            | "ComSpec"
                            | "COMSPEC"
                            | "PATHEXT"
                            | "TEMP"
                            | "TMP"
                            | "TMPDIR"
                            | "HTTP_PROXY"
                            | "HTTPS_PROXY"
                            | "ALL_PROXY"
                            | "NO_PROXY"
                            | "http_proxy"
                            | "https_proxy"
                            | "all_proxy"
                            | "no_proxy"
                            | "CURL_SSL_BACKEND"
                    )
                })
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn http_preferences(&self, url: &str) -> Result<BTreeMap<String, String>> {
        let output = self.output(&[
            "config",
            "--includes",
            "--null",
            "--get-urlmatch",
            "http",
            url,
        ])?;
        ensure!(
            output.stderr.is_empty()
                && (output.status.success()
                    || (output.status.code() == Some(1) && output.stdout.is_empty())),
            "cannot capture frozen HTTP configuration"
        );
        let mut values = BTreeMap::new();
        ensure!(
            output.stdout.is_empty() || output.stdout.ends_with(&[0]),
            "HTTP configuration framing is incomplete"
        );
        for entry in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|entry| !entry.is_empty())
        {
            let entry =
                std::str::from_utf8(entry).context("HTTP configuration is not valid UTF-8")?;
            let (key, value) = entry.split_once('\n').unwrap_or((entry, "true"));
            if matches!(key, "http.extraheader" | "http.followredirects") {
                continue;
            }
            ensure!(
                supported_http_key(key),
                "frozen publication does not support HTTP preference {key}"
            );
            values.insert(key.to_owned(), value.to_owned());
        }
        for (name, key) in [
            ("GIT_HTTP_USER_AGENT", "http.useragent"),
            ("GIT_HTTP_LOW_SPEED_LIMIT", "http.lowspeedlimit"),
            ("GIT_HTTP_LOW_SPEED_TIME", "http.lowspeedtime"),
            ("GIT_HTTP_MAX_REQUESTS", "http.maxrequests"),
            ("GIT_SSL_CAINFO", "http.sslcainfo"),
            ("GIT_SSL_CAPATH", "http.sslcapath"),
            ("GIT_SSL_CERT", "http.sslcert"),
            ("GIT_SSL_KEY", "http.sslkey"),
            ("GIT_SSL_CERT_TYPE", "http.sslcerttype"),
            ("GIT_SSL_KEY_TYPE", "http.sslkeytype"),
            ("GIT_SSL_VERSION", "http.sslversion"),
            ("GIT_SSL_CIPHER_LIST", "http.sslcipherlist"),
            ("GIT_PROXY_SSL_CAINFO", "http.proxysslcainfo"),
            ("GIT_PROXY_SSL_CERT", "http.proxysslcert"),
            ("GIT_PROXY_SSL_KEY", "http.proxysslkey"),
            ("GIT_HTTP_PROXY_AUTHMETHOD", "http.proxyauthmethod"),
        ] {
            if let Some(value) = self.environment.get(OsStr::new(name)) {
                values.insert(
                    key.into(),
                    value
                        .to_str()
                        .context("HTTP preference environment is not valid UTF-8")?
                        .into(),
                );
            }
        }
        for name in [
            "GIT_SSL_CERT_PASSWORD_PROTECTED",
            "GIT_PROXY_SSL_CERT_PASSWORD_PROTECTED",
        ] {
            ensure!(
                !self.environment.contains_key(OsStr::new(name)),
                "frozen publication cannot preserve interactive certificate preference {name}"
            );
        }
        if let Some(value) = self.environment.get(OsStr::new("GIT_SSL_NO_VERIFY")) {
            ensure!(
                !git_bool(
                    value
                        .to_str()
                        .context("invalid TLS verification preference")?
                )?,
                "frozen publication requires TLS certificate verification"
            );
        }
        for key in ["http.sslverify", "http.proxysslverify"] {
            if let Some(value) = values.get(key) {
                ensure!(
                    git_bool(value)?,
                    "frozen publication requires TLS certificate verification"
                );
            }
        }
        for (key, value) in &values {
            ensure!(
                !value.chars().any(char::is_control),
                "frozen publication HTTP preference {key} contains control characters"
            );
        }
        for (key, allowed) in [
            ("http.sslcerttype", ["PEM", "DER", "P12"].as_slice()),
            ("http.sslkeytype", ["PEM", "DER"].as_slice()),
        ] {
            if let Some(value) = values.get(key) {
                ensure!(
                    allowed
                        .iter()
                        .any(|allowed| value.eq_ignore_ascii_case(allowed)),
                    "frozen publication requires file-based certificate and key types; unsupported {key}"
                );
            }
        }
        for key in [
            "http.sslcainfo",
            "http.sslcapath",
            "http.sslcert",
            "http.sslkey",
            "http.proxysslcainfo",
            "http.proxysslcert",
            "http.proxysslkey",
        ] {
            if let Some(value) = values.get_mut(key) {
                ensure!(
                    !value.is_empty(),
                    "frozen publication needs an explicit certificate path for {key}"
                );
                *value = path_text(&self.preference_path(value)?)?;
            }
        }
        if let Some(value) = values.get_mut("http.pinnedpubkey")
            && !value.starts_with("sha256//")
        {
            *value = path_text(&self.preference_path(value)?)?;
        }
        Ok(values)
    }

    fn preference_path(&self, value: &str) -> Result<PathBuf> {
        if let Some(value) = value.strip_prefix("~/") {
            let home = self
                .environment
                .get(OsStr::new("HOME"))
                .or_else(|| self.environment.get(OsStr::new("USERPROFILE")))
                .context("certificate path needs a home directory")?;
            return absolute_path(
                &self.root,
                path_text(&PathBuf::from(home).join(value))?.as_str(),
            );
        }
        ensure!(
            !value.starts_with('~') && !value.starts_with("%(prefix)"),
            "frozen publication requires an explicit certificate path"
        );
        let lower = value.to_ascii_lowercase();
        ensure!(
            !lower.starts_with("pkcs11:")
                && ![
                    "currentuser/",
                    "currentuser\\",
                    "localmachine/",
                    "localmachine\\"
                ]
                .iter()
                .any(|prefix| lower.starts_with(prefix)),
            "frozen publication requires certificate files rather than engine or certificate-store selectors"
        );
        absolute_path(&self.root, value)
    }
}

fn supported_http_key(key: &str) -> bool {
    matches!(
        key,
        "http.proxy"
            | "http.proxyauthmethod"
            | "http.proxysslcert"
            | "http.proxysslkey"
            | "http.proxysslcainfo"
            | "http.proxysslverify"
            | "http.sslverify"
            | "http.sslcainfo"
            | "http.sslcapath"
            | "http.sslcert"
            | "http.sslkey"
            | "http.sslcerttype"
            | "http.sslkeytype"
            | "http.sslbackend"
            | "http.sslversion"
            | "http.sslcipherlist"
            | "http.pinnedpubkey"
            | "http.schannelusesslcainfo"
            | "http.schannelcheckrevoke"
            | "http.useragent"
            | "http.lowspeedlimit"
            | "http.lowspeedtime"
            | "http.version"
            | "http.maxrequests"
            | "http.minsessions"
            | "http.postbuffer"
            | "http.keepaliveidle"
            | "http.keepaliveinterval"
            | "http.keepalivecount"
            | "http.suppressconnectheaders"
    )
}

fn git_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" | "" => Ok(false),
        _ => anyhow::bail!("invalid TLS verification preference"),
    }
}

fn absolute_path(root: &Path, value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "publication path is empty or contains control characters"
    );
    let path = PathBuf::from(value);
    Ok(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn path_text(path: &Path) -> Result<String> {
    let path = inspection_git_path_spelling(path.to_owned());
    let text = path
        .to_str()
        .context("frozen publication paths must be valid UTF-8")?;
    ensure!(
        !text.chars().any(char::is_control),
        "publication path contains control characters"
    );
    Ok(text.to_owned())
}

fn parameters(values: &BTreeMap<String, String>) -> OsString {
    values
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                super::quote_git_parameter(key),
                super::quote_git_parameter(value)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
        .into()
}

#[cfg(test)]
#[path = "frozen_tests.rs"]
mod tests;
