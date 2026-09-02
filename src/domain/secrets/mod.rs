//! Secret scanning.
//!
//! # Why the gate is at push
//!
//! A session transcript records what the agent actually read and ran, so `cat .env`,
//! `export TOKEN=...` and an API key pasted into the conversation are all in it. `push` is the
//! moment **content first leaves this machine**, so the gate sits here.
//!
//! The client-side gate can be bypassed (edit the code, or run git push directly), so whatever
//! the server truly guarantees the server has to scan and refuse for itself. But the sentence the
//! server guarantees is **narrow**:
//!
//! > the hub is not a distribution channel for leaked credentials
//!
//! not "the hub keeps your secrets for you". So it scans only at the moment content becomes
//! readable by a third party — a push to a public agent, and a private-to-public transition — and
//! not a push to a private agent. That is your own storage, nothing is distributed, the server
//! has no standing there, and it must not wedge your own work with a false positive.
//!
//! The client-side gate is therefore worth more than "learn early that the server will refuse":
//! **a private push has only this one**. It cannot stop you (`AGIT_ALLOW_SECRETS=1` is yours),
//! but it is the only thing that warns you before a secret enters history. Once in, it does not
//! go away — it blocks you the day you want to make this agent public.
//!
//! Not at `agit commit`: a commit is a purely local action, a secret staying on this machine is
//! not a leak, and a commit that refuses to record a version because of a secret only teaches
//! people to stop recording versions. Same division of labour as git — commit freely, stop it at
//! push.
//!
//! # Err toward the false positive
//!
//! A miss costs a leaked, irreversible secret; a false positive costs one allowlist entry. So the
//! rule set is the aggressive gitleaks set of 222 rules (see [`rules`]), with two levels of
//! allowlist to absorb the false positives.
//!
//! # Same source on both sides, different authority
//!
//! The rule engine lives in `domain` (not behind the `cli` feature), so a backend depending on
//! this crate with `default-features = false` runs the same rules. But **the authority differs**:
//! inline `agit:allow-secret` and the local allowlist are "do not stop me" switches that hold
//! locally, and the server-side gate honours neither — otherwise anyone could push a secret by
//! writing an annotation on their own line. The switch is [`Policy`]: the client uses
//! [`Policy::CLIENT`], the server uses [`Policy::STRICT`].
//!
//! # Upgrading the vendored rules
//!
//! Copy gitleaks' `config/gitleaks.toml` over `gitleaks.toml`, keep the provenance / MIT comment
//! at the top of the file and update the version, then run `cargo test --lib domain::secrets` —
//! `every_rule_compiles` tells you whether there is a new rule Rust's `regex` cannot compile.

pub mod rules;

use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(feature = "secret-vault")]
use zeroize::Zeroizing;

#[cfg(feature = "secret-vault")]
pub type RegisteredMatcher = crate::domain::secret_filter::Matcher;

#[cfg(not(feature = "secret-vault"))]
#[derive(Default)]
pub struct RegisteredMatcher;

#[cfg(feature = "secret-vault")]
fn load_registered_matcher() -> crate::Result<RegisteredMatcher> {
    crate::domain::secret_filter::VaultStore::open_default()?.matcher()
}

#[cfg(not(feature = "secret-vault"))]
fn load_registered_matcher() -> crate::Result<RegisteredMatcher> {
    Ok(RegisteredMatcher)
}

/// The allowlist file, under `$AGIT_HOME`.
///
/// Not inside the repo: that repo gets pushed, and publishing the allowlist along with it is both
/// pointless and a leak of "we know there is something secret-shaped here". Under `$AGIT_HOME` it
/// applies per machine, and it is not swept away with a staging area once a push succeeds.
pub const ALLOWLIST_FILE: &str = ".agit-allow-secrets";

/// The line-level allow annotation.
pub const INLINE_PRAGMA: &str = "agit:allow-secret";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub rule: String,
    pub file: Option<String>,
    /// **Which kind of carrier** this hit sits in.
    ///
    /// The report's wording branches on the carrier (a working-tree file can be annotated with
    /// `agit:allow-secret`, a commit object can only be rewritten out of history, a tag object is
    /// simply retagged), and the test has to be this field, **not** parsing the human-readable
    /// `file` string. For a working-tree hit `file` is the user's own path — a directory actually
    /// named `commit object x/…` flips the decision, and that is input the user can create.
    pub source: Source,
    pub line: usize,
    /// The redacted fragment. **Never the whole secret** — scan reports go into CI logs, and
    /// printing the whole secret leaks it a second time.
    pub redacted: String,
    /// An irreversible fingerprint of the **raw bytes** of the match. It has one use: deciding
    /// "are these two the same thing".
    ///
    /// [`Self::redacted`] cannot stand in for content identity: redaction keeps the first four
    /// and the last two characters, two different AWS keys both start with `AKIA` by
    /// construction, and once the last two collide the fragments are identical. The second
    /// **real credential** on the same path and the same line is then silently collapsed away
    /// while `truncated` stays false — the report is short one real hit and claims to be
    /// complete. That is exactly the failure this gate must not have.
    ///
    /// What is stored is a hash, not the plaintext: `Hit` is serialized into the `--json` report
    /// and goes into CI logs, so carrying the raw secret leaks it a second time. The display
    /// layer keeps looking only at `redacted`.
    pub fingerprint: u64,
}

/// Internal-only exact values found by heuristic rules while settling a local
/// session. Unlike [`Hit`], this type is deliberately neither printable nor
/// serializable, and its owned plaintext is wiped on drop.
#[cfg(feature = "secret-vault")]
pub(crate) struct SecretCandidateBatch {
    pub(crate) values: Vec<Zeroizing<String>>,
    pub(crate) truncated: bool,
}

/// An irreversible fingerprint of a matched span.
///
/// One property is needed: **two different byte sequences almost never collide**.
/// `DefaultHasher` is enough — the fingerprint is a dedupe key inside a single scan, never
/// persisted, never sent over the network, never compared across processes, so "not guaranteed
/// stable across versions" costs nothing here.
///
/// Deliberately returns only a `u64`: once the raw secret is stored in a [`Hit`] it leaves with
/// the `--json` report and with the CI logs.
fn fingerprint(found: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    found.as_bytes().hash(&mut h);
    h.finish()
}

/// How many hits one scan collects at most.
///
/// Hits are a **function** of the input, not a subset of it: a history that matches on every line
/// produces `Hit`s on the order of its line count, each carrying its own `String`. Streaming the
/// input bounds only the input side.
///
/// Stopping when full **does not change the verdict** — one hit is enough to refuse, and this
/// report is "a refusal plus a few examples", never a complete list.
const MAX_REPORTED_HITS: usize = 200;

/// Collects hits and stops when full.
///
/// # Why one type and not three `if`s
///
/// A cap that lives on the commit path alone leaves the other two (tag objects, working-tree
/// files) unbounded, and the same boundary written once per path is enforced by remembering,
/// which is not a contract that holds. Every path goes through the same collector, so a path
/// added later is bounded by construction.
///
/// Stopping when full **does not change the verdict**: one hit is enough to refuse, and this
/// report is "a refusal plus a few examples", never a complete list.
///
/// # Why it also deduplicates
///
/// The same hit can be recognized twice inside one carrier — the same secret written twice on one
/// line has the same rule, path, line number and content; to the user it is one thing, and saying
/// it twice is noise.
///
/// The test is the hit itself (rule + position + **fingerprint of the raw bytes**), not "which
/// path it arrived by". That last field cannot be the redacted fragment: redaction keeps the
/// first four and the last two characters, two different AWS keys both start with `AKIA`, and
/// once the last two collide they yield the same key and the second real credential is silently
/// swallowed. See [`Hit::fingerprint`].
///
/// Different carriers never collapse into each other: `file` for commit / tag / blob is
/// `<kind> object <sha8>…`, all distinct. That is deliberate — the line in the working tree and
/// the blob in history are **two things to handle separately** (an inline annotation only reaches
/// the first, the second needs history rewritten), and collapsing them into one drops one of the
/// two ways out.
struct HitCollector {
    hits: Vec<Hit>,
    seen: HashSet<String>,
    truncated: bool,
}

impl HitCollector {
    fn new() -> Self {
        Self {
            hits: Vec::new(),
            seen: HashSet::new(),
            truncated: false,
        }
    }

    /// The dedupe key: "what a hit is, and where".
    ///
    /// `file` can hold any character (it is the user's path), so the separator is a byte that
    /// cannot occur in a path, which rules out concatenation ambiguities such as `a\nb` + line 1
    /// versus `a` + line `b\n1`.
    ///
    /// Path separators are normalized: the working-tree pass gets its label from
    /// `Path::strip_prefix` (`\` on Windows) and the object pass gets it from git (always `/`).
    /// Without normalizing, the same hit in the same file counts as two on Windows — the exact
    /// case this key exists to stop. Normalization applies to the key only; the `file` shown to
    /// the user is kept unchanged.
    ///
    /// The last field is **the fingerprint of the raw matched bytes**, not the redacted fragment.
    /// Redaction keeps the first four and the last two characters, so as content identity two
    /// different AWS keys (both starting with `AKIA`, same last two) on the same path and the
    /// same line yield exactly the same key and the second real credential is silently collapsed
    /// away. See [`Hit::fingerprint`].
    fn key(h: &Hit) -> String {
        format!(
            "{}\x00{}\x00{}\x00{}",
            h.rule,
            h.file.as_deref().unwrap_or("").replace('\\', "/"),
            h.line,
            h.fingerprint
        )
    }

    /// Whether there is room for more. The caller uses it to **skip the remaining scanning
    /// work**, not just to skip a push.
    ///
    /// # Why it sets `truncated` itself (hence `&mut self`)
    ///
    /// Because "I skipped the work behind me because I am full" and "I recorded that this list is
    /// incomplete" have to be **one action**; half of it is not a thing. With only `push` setting
    /// the flag, the caller's fast path (`if out.is_full() { ... }`) never reaches `push` — so
    /// when the working tree fills the collector exactly and one tag hit sits behind it, the
    /// report claims `truncated = false`, and the user fixes the list entry by entry, pushes
    /// again, and is stopped by the same gate.
    ///
    /// The direction is conservative: when it is full and there happen to be no more hits, the
    /// worst case is one extra "there may be more"; failing to say "there is more" is a lie.
    fn is_full(&mut self) -> bool {
        if self.hits.len() >= MAX_REPORTED_HITS {
            self.truncated = true;
            return true;
        }
        false
    }

    /// How many more it can take.
    ///
    /// It goes into [`scan_text_capped`] as the bound before a single carrier is scanned: that
    /// layer's output has to be bounded too, since capping the final list does not stop a single
    /// dense carrier from materializing `Hit`s on the order of its own size first.
    fn remaining(&self) -> usize {
        MAX_REPORTED_HITS.saturating_sub(self.hits.len())
    }

    /// Take one. When full it only records `truncated` (`is_full` does that) and stops growing.
    ///
    /// A hit already taken is dropped and **costs no budget**: it carries no new information, and
    /// letting it eat a slot pushes a hit that was never reported out of this report.
    fn push(&mut self, h: Hit) {
        if self.is_full() {
            return;
        }
        if !self.seen.insert(Self::key(&h)) {
            return;
        }
        self.hits.push(h);
    }

    /// Take a batch.
    fn extend(&mut self, hs: impl IntoIterator<Item = Hit>) {
        for h in hs {
            self.push(h);
        }
    }

    /// Record "this list is incomplete" — but **not** because this collector is full.
    ///
    /// # Why `is_full` cannot be used to infer it
    ///
    /// Scanning a single carrier is bounded on its own ([`scan_text_capped`]), and once that
    /// bound bites there are hits that were never scanned. "Is the collector full" does not
    /// answer that: the miss happens **inside** the carrier, and the collector sees only the few
    /// hits handed to it.
    ///
    /// With the budget counted in unique spans ([`SpanBudget`]), hitting the budget happens to
    /// mean the collector is full and `is_full` sets the flag along with it — but that is **two
    /// independent decisions coinciding**: the budget's counting key, and the caller passing
    /// `remaining()` as `cap`. Change either side and the inference silently stops holding,
    /// always in the direction of "the report claims to be complete". (Were the budget counted
    /// **before** dedupe, the inference fails outright: two rules recognizing one span each eat a
    /// slot, the deduplicated list handed over is shorter than the budget, and the collector is
    /// not full.)
    ///
    /// So completeness is stated directly by **the side that knows** ([`ScanReport::truncated`])
    /// and booked here, not left for the next person to infer.
    fn mark_truncated(&mut self) {
        self.truncated = true;
    }

    fn into_hits(self) -> Vec<Hit> {
        self.hits
    }

    fn was_truncated(&self) -> bool {
        self.truncated
    }
}

/// What one scan produces: the hits, plus **whether that list is complete**.
///
/// # Why the flag travels with the result
///
/// When truncation happens, the hit list on its own looks exactly like "this is all of it". The
/// user then fixes the report entry by entry, pushes again, and is stopped by the same gate — a
/// loop with no way out. The verdict is unaffected (non-empty is a refusal), but "there is more"
/// has to reach the layer that prints the report.
pub struct ScanReport {
    pub hits: Vec<Hit>,
    /// The cap filled up and there are unreported hits behind it.
    pub truncated: bool,
    /// The part of the scan surface that **was never read at all**. See [`Unscanned`].
    pub unscanned: Unscanned,
}

/// The part of the scan surface that **was not read** — not "clean", but "not looked at".
///
/// # Why it has to travel out with the result
///
/// The same reasoning as [`ScanReport::truncated`], one notch harder: truncation at least
/// reported a few hits, whereas these objects never had a single byte enter the scanning engine.
/// Giving the verdict on "no hits = clean" anyway is fail open — a gate letting through the input
/// it could not reach, and saying nothing.
///
/// So all three kinds of "not looked at" are booked explicitly here and said out loud by
/// `agit push` / `agit scan`:
///
/// * **The whole scan surface is over budget** ([`Unscanned::over_budget`]): the cumulative byte
///   count exceeds [`ScanLimits::budget_bytes`], so it **stops where it stands**. Better to say
///   so at once than to grind through tens of gigabytes first.
/// * **A single object is over the line** ([`Unscanned::oversized`]): an object larger than
///   [`ScanLimits::max_object_bytes`] is not read at all (see
///   [`crate::domain::repo::Repo::git_cat_file_batch`]); everything else is scanned as usual.
/// * **A single working-tree file is over the line** ([`Unscanned::oversized_files`]): the same
///   bound, with a working-tree file as the carrier.
///
/// # Why the working-tree case gets a field of its own
///
/// Because **the way to locate it and the way to fix it both differ**, and this list is printed
/// straight to the user. The handle in the object ledger is an oid: `git cat-file -p <oid>` for
/// the content, `git log --find-object=` for where it came from, and the fix is rewriting
/// history. The handle in the working-tree ledger is a path, and that file may well **not be in
/// history at all** (the observed case was an uncommitted `artifact.log`) — what it needs is
/// deleting, moving, or adding to ignore, with no history rewrite at all. Mixed into one `Vec`,
/// the report would suggest `git cat-file -p artifact.log`, which is advice that goes nowhere,
/// and the sentence that should have been said has no place to be said.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Unscanned {
    /// `(bytes counted so far, the budget)`. Set = **a whole block of the scan surface went
    /// unread**; from the moment the line was crossed, nothing behind it ran.
    ///
    /// Both passes set it, because they are two halves of one thing:
    ///
    /// * the working-tree pass adds up the sizes metadata reports file by file, stops before the
    ///   file that crosses the line, and the object pass does not even estimate
    ///   ([`scan_agent_repo`]);
    /// * when the working tree stays under, the object pass first estimates the total from
    ///   headers alone, and reads no object at all once that crosses ([`scan_publish_objects`]).
    ///
    /// The first number **is a lower bound**, not a total: at the moment the line is crossed what
    /// remains has never been counted — the working tree stops halfway and the files behind it
    /// are never sized, and the object pass stops enumerating as soon as its estimate reaches the
    /// budget (see [`estimate_object_bytes`]). The verdict only cares whether it went over, for
    /// which a lower bound is enough; when the number is printed it has to be said as "at least"
    /// (see `crate::commands::report_unscanned`).
    pub over_budget: Option<(u64, u64)>,
    /// git objects over the line that were not read: `(first eight of the oid, byte count)`.
    pub oversized: Vec<(String, u64)>,
    /// **Working-tree files** over the line that were not read: `(path relative to the repo,
    /// byte count)`.
    pub oversized_files: Vec<(String, u64)>,
}

impl Unscanned {
    /// Nothing went unread.
    pub fn is_empty(&self) -> bool {
        self.over_budget.is_none() && self.oversized.is_empty() && self.oversized_files.is_empty()
    }
}

/// How much work one local scan is willing to do.
///
/// # Why the work itself has to be bounded
///
/// None of the four layers that bound cost counts **work**: the first two only keep the input out
/// of memory in one piece, the fourth counts `Hit`s, and the third (stop enumerating once the hit
/// cap is full) **never fires on a clean repo** — the only early exit is dead on the most common
/// path.
///
/// And work is **superlinear** in this product's repo shape: `session/log.jsonl` is append-only
/// with one commit per turn, so "the sum of bytes over all reachable blobs" is on the order of
/// turns². Observed (about 1.2 KB per turn): 500 turns 148.8 MiB / 5.04 s, 1500 turns
/// **1337 MiB / 38.72 s**, and extrapolating by the square, 6000 turns is about 21 GiB / 76 s.
/// Unbounded, this gate's duration is decided by how much work the user did, with no ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// How many bytes of body this object scan reads at most (estimated, cumulative).
    pub budget_bytes: u64,
    /// The byte cap for a **single** object. An object over the line is not read at all and is
    /// recorded in [`Unscanned::oversized`].
    pub max_object_bytes: u64,
}

impl ScanLimits {
    /// A cumulative budget of 4 GiB, 64 MiB per object.
    ///
    /// Both numbers are the defaults of the server-side whole-repo scan (`repo_bytes_mb = 4096`,
    /// `blob_mb = 64`) — **because it is the same problem**: the object set a first publish has
    /// to get through and the one the server's private-to-public transition has to get through
    /// are the same bytes. Picking a number on each side separately only adds one more cause of
    /// "the local machine let it through, the server refused it".
    ///
    /// 4 GiB leaves a real agent threefold headroom: 1500 turns is 1337 MiB observed, so reaching
    /// this bound takes about 2600 turns, and by then what the user needs is not another two
    /// minutes of waiting but to know this history is past what the local machine can scan.
    ///
    /// 64 MiB bounds the peak of a single allocation: with plenty of cumulative budget left, one
    /// 300 MiB blob still drives memory up (observed maxRSS 52 MB to 682 MB).
    ///
    /// This bound covers **the three paths that read bodies, plus the working-tree pass** — only
    /// `object` appears in the name `max_object_bytes`, yet the same 682 MB can be driven by an
    /// uncommitted 300 MiB `artifact.log` down the working-tree path (observed), and that pass
    /// runs **before** the object scan, so the "read no object at all when over budget" fast path
    /// cannot save it. Splitting the test by carrier therefore only leaves gaps: 300 MiB of bytes
    /// refused as a blob but let through as a tag or in the working tree is not three paths, it
    /// is two holes.
    pub const DEFAULT: ScanLimits = ScanLimits {
        budget_bytes: 4 * 1024 * 1024 * 1024,
        max_object_bytes: 64 * 1024 * 1024,
    };
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits::DEFAULT
    }
}

/// The **destination** of this publish — which commits it already has.
///
/// # Why this is a parameter and not inferred in place
///
/// The scan surface answers "what will actually be sent this time", which is the difference
/// between what is local and what the destination has. Only the destination can say what the
/// subtracted half is.
///
/// Subtracting `--not --remotes=origin` uses the local remote-tracking refs instead. Those refs
/// describe **the remote of the last fetch/push**, not the destination of this push, and the two
/// come apart along real paths: `AGIT_HUB_URL=… agit push` switches hub ([`crate::hub`]
/// recommends exactly that), and `agit repo delete` defaults "delete the local copy too?" to
/// false, so the tracking refs stay local. The cleanest observed case: after a commit holding a
/// secret is pushed, the remote is deleted and recreated empty, and
/// `rev-list --objects --branches --not --remotes=origin` prints nothing (the scan surface is
/// empty) while `git push` actually sends 4 objects, that secret blob among them.
///
/// So what is stored here is **the answer after asking the destination** (the branch tips
/// `git ls-remote` reports, filtered down to the ones that exist locally), not a local guess. If
/// it cannot be asked, [`Destination::Unknown`] — scan the whole history, which is the safe
/// direction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Destination {
    /// The destination was not asked (never published, offline, hub switched, `agit scan` with
    /// no remote).
    ///
    /// Scan **every** reachable object. This is not a conservative approximation but the
    /// **exact** answer: with the destination unknown, what may be sent this time is the whole
    /// history.
    #[default]
    Unknown,
    /// The branch tips the destination itself reported, restricted to objects the local repo
    /// actually has. An empty `Vec` is equivalent to [`Destination::Unknown`] (an empty repo has
    /// nothing, so this push sends everything).
    Advertised(Vec<String>),
}

impl Destination {
    /// Build one from the tips the destination reported, **filtered down to the ones the local
    /// repo really has**.
    ///
    /// An OID the local repo lacks cannot be an argument to `--not` (`rev-list` reports a bad
    /// object outright), and a commit the destination has but the local repo does not is
    /// ordinary (somebody else pushed it). Dropping those only makes the scan surface larger,
    /// which is the safe direction.
    pub fn advertised(
        repo: &crate::domain::repo::Repo,
        tips: Vec<String>,
    ) -> crate::Result<Destination> {
        if tips.is_empty() {
            return Ok(Destination::Unknown);
        }
        let mut have: Vec<String> = Vec::with_capacity(tips.len());
        repo.git_cat_file_batch_check(tips, |oid, kind, _| {
            if kind != "missing" {
                have.push(oid.to_string());
            }
            Ok(())
        })?;
        Ok(if have.is_empty() {
            Destination::Unknown
        } else {
            Destination::Advertised(have)
        })
    }

    /// Whether the destination narrowed this scan surface.
    ///
    /// `agit push` uses it to decide whether to rescan when the destination changed after the
    /// scan: a pass that was never narrowed already covered everything, so switching destination
    /// misses nothing.
    pub fn narrows(&self) -> bool {
        matches!(self, Destination::Advertised(t) if !t.is_empty())
    }

    /// The rev range to enumerate: every local branch, minus what the destination has.
    fn revs(&self) -> Vec<String> {
        let mut v = vec!["--branches".to_string()];
        if let Destination::Advertised(tips) = self
            && !tips.is_empty()
        {
            v.push("--not".to_string());
            v.extend(tips.iter().cloned());
        }
        v
    }
}

/// Which surface a scan covers and how much work it does.
///
/// Gathering it into one type is what lets `agit push` and `agit scan` **pass the same thing**:
/// the two local gates have to reach the same verdict, and the verdict depends on these two
/// fields. As two separate parameters, the next person changing only one of them costs "the two
/// gates drift apart quietly".
#[derive(Debug, Clone, Default)]
pub struct ScanPlan {
    /// The destination of this publish.
    pub dest: Destination,
    pub limits: ScanLimits,
}

impl ScanPlan {
    /// Destination unknown: scan every reachable object with the default budget.
    pub fn full() -> ScanPlan {
        ScanPlan::default()
    }

    /// Destination known (`git ls-remote` was asked).
    pub fn to(dest: Destination) -> ScanPlan {
        ScanPlan {
            dest,
            limits: ScanLimits::DEFAULT,
        }
    }
}

/// The carrier a hit sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A file in the working tree.
    File,
    /// The raw body of a commit object.
    CommitObject,
    /// The raw body of an annotated tag object.
    TagObject,
    /// The raw content of a blob object — a file payload **in history**, not necessarily in the
    /// working tree.
    ///
    /// This variant has to stay separate from [`Source::File`] rather than reuse it for
    /// convenience: a blob hit can come from a history that already deleted the file, or from the
    /// real tree hidden behind `refs/replace/*`. Reported as `file`, the user follows the path
    /// into the working tree where that secret is simply not there, and the one way out that
    /// works (locate the blob, rewrite history) goes unmentioned.
    BlobObject,
}

impl Source {
    /// A stable machine-readable name, for `--json`.
    ///
    /// With it, a JSON consumer never has to parse the human-readable `file` string to tell the
    /// carriers apart — which is exactly the test [`Hit::source`] names as wrong: a directory
    /// actually called `commit object ab12cd34/…` flips it, and that is input the user can
    /// create.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::File => "file",
            Source::CommitObject => "commit_object",
            Source::TagObject => "tag_object",
            Source::BlobObject => "blob_object",
        }
    }
}

/// Whether this scan honours the two local "do not stop me" switches.
///
/// It exists because client and server run the same engine and must not have the same tolerance:
/// a server-side gate an inline annotation can switch off is not a gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Honour inline `agit:allow-secret`.
    pub inline_pragma: bool,
    /// Honour `$AGIT_HOME/.agit-allow-secrets`.
    pub allowlist: bool,
}

impl Policy {
    /// Local: both switches are honoured — a false positive needs a way out, or the gate ends up
    /// switched off entirely.
    pub const CLIENT: Policy = Policy {
        inline_pragma: true,
        allowlist: true,
    };
    /// The authoritative gate (server side): nobody's annotation is honoured.
    pub const STRICT: Policy = Policy {
        inline_pragma: false,
        allowlist: false,
    };
}

impl Default for Policy {
    fn default() -> Self {
        Policy::CLIENT
    }
}

pub fn load_allowlist(dir: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(dir.join(ALLOWLIST_FILE)) else {
        return HashSet::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// An equal-length view of JSON escaping.
///
/// A newline inside a jsonl body is two **characters** (`\` + `n`), and `n` is a word character —
/// a token immediately after the escape therefore loses its left `\b` boundary and the rule
/// misses it silently. Replacing `\\n \\r \\t \\"` with two spaces (equal length, producing a
/// non-word boundary) before matching keeps span offsets in one-to-one correspondence with the
/// original text.
///
/// One pass rather than four chained `String::replace` calls: that would be four megabyte-scale
/// allocations plus four full rewrites, which on a 2 MB transcript costs more than running the
/// regexes themselves.
///
/// Going byte by byte is safe: all four patterns are ASCII, and a UTF-8 continuation byte is
/// always at least 0x80, so it can never be mistaken for a backslash. The four patterns also
/// cannot overlap each other (none has a backslash as its second character), so one pass gives
/// byte-for-byte the same result as replacing them in turn.
pub fn view_of(s: &str) -> String {
    let b = s.as_bytes();
    if !b.contains(&b'\\') {
        return s.to_string();
    }
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && matches!(b.get(i + 1), Some(b'n' | b'r' | b't' | b'"')) {
            out.extend_from_slice(b"  ");
            i += 2;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// A line-number index: built on demand, so a clean scan pays nothing.
struct Lines<'t> {
    text: &'t str,
    starts: Vec<usize>,
}

impl<'t> Lines<'t> {
    fn new(text: &'t str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Lines { text, starts }
    }

    /// Byte offset → 1-based line number.
    fn number_at(&self, off: usize) -> usize {
        self.starts.partition_point(|&s| s <= off)
    }

    fn text_at(&self, off: usize) -> &'t str {
        let i = self.number_at(off) - 1;
        let start = self.starts[i];
        let end = self.starts.get(i + 1).map_or(self.text.len(), |&e| e);
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// Where a hit sits in the text (byte offsets into the **view**, which corresponds to the
/// original text at equal length).
struct Raw {
    rule: &'static str,
    start: usize,
    end: usize,
}

/// The scanning core: every public entry point ends up here.
///
/// The order is the whole of the performance and must not change:
/// 1. the escape view (equal length, so offsets still map back to the original text)
/// 2. keyword prefilter, one automaton over the text, selecting the rules that still need a regex
/// 3. lazy compilation — only the rules that survived are compiled
/// 4. match, take the secret span, entropy, the rule's own allowlist
///
/// Matching the whole text at once (rather than line by line) has two reasons: performance (line
/// by line multiplies every rule's regex startup cost by the line count), and fidelity — rules
/// such as `private-key` and `curl-auth-user` span lines by nature, and a line-by-line scan
/// misses them.
fn raw_hits(view: &str) -> Vec<Raw> {
    // A budget of `usize::MAX` is never used up, so the "the bound was reached" flag is false.
    raw_hits_capped(view, usize::MAX, |_, _| true).0
}

/// A budget counted in **unique spans**.
///
/// # Why the budget cannot count the `Raw`s produced
///
/// `raw_hits_capped` produces `Raw`s, while what is finally reported is the list **after**
/// [`dedupe_same_span`]: when two rules recognize the same characters (`token: npm_…` matches
/// both `generic-api-key` and `npm-access-token`), the two `Raw`s dedupe into one hit.
///
/// Counting `Raw`s makes that one hit eat two slots. At `cap = 2` the budget is exhausted on the
/// spot, dedupe leaves **one** hit, and the **other kind** of secret behind it was never scanned
/// — while the collector is not full, so the report's `truncated` is still `false`. **A hit went
/// unscanned and the report claims to be complete.**
///
/// So the budget counts distinct spans: the same `(start, end)` recognized again by a second rule
/// costs nothing (after dedupe it was only ever one hit), `out` takes it as usual, and dedupe
/// happens later.
///
/// # The counting key has to match the dedupe key
///
/// [`dedupe_same_span`]'s test is that `start` and `end` are **both equal**, so the key here is
/// `(start, end)`. Out of step, what is counted here and what is finally reported are not the
/// same set of things — and the fix itself would drift into the very shape it removes.
struct SpanBudget {
    cap: usize,
    /// Spans that already cost budget. `None` = uncapped, see [`SpanBudget::new`].
    counted: Option<HashSet<(usize, usize)>>,
    /// The budget ran out while a reportable hit **really was** still waiting outside.
    exhausted: bool,
}

impl SpanBudget {
    /// `cap == usize::MAX` is [`raw_hits`]'s **uncapped** path: since the budget can never run
    /// out, the bookkeeping set is pure waste — that path wants the full output anyway, and the
    /// `agit share` / masking it serves never goes through this bound. So in that case the set is
    /// not even built.
    fn new(cap: usize) -> Self {
        SpanBudget {
            cap,
            counted: (cap < usize::MAX).then(HashSet::new),
            exhausted: false,
        }
    }

    /// Charge a hit that has already passed every filter. Returns **whether scanning may
    /// continue**.
    ///
    /// The moment it returns `false` it records [`Self::exhausted`]: what it holds right then is
    /// a hit over the line, so "I stopped because of the budget" and "there really is an
    /// unreported hit" are **one fact**, not an inference.
    ///
    /// Conversely, reaching exactly `cap` spans does **not** stop early: at that point whether
    /// there is more behind is unknown. Scanning on until span `cap + 1` is actually met keeps
    /// the flag honest — and the cost is bounded, since the first hit over the line stops it.
    fn charge(&mut self, start: usize, end: usize) -> bool {
        let Some(counted) = self.counted.as_mut() else {
            return true; // Uncapped: nothing is charged and it never stops.
        };
        if counted.contains(&(start, end)) {
            return true;
        }
        if counted.len() >= self.cap {
            self.exhausted = true;
            return false;
        }
        counted.insert((start, end));
        true
    }
}

/// The bounded version of [`raw_hits`]: stop once `cap` hits **that pass `keep`** are found.
///
/// # Why this bound has to live **in the engine**
///
/// Hits are a **function** of the input, not a subset of it: in text made of one repeated token
/// the hit count is on the order of the length, and every `Raw` takes space of its own. Having
/// the caller `truncate` the `Vec` afterwards achieves nothing — the allocation over the line
/// already happened before `truncate` runs. Observed: 64 MiB of dense input produces about
/// 3.2 million entries and about 600 MB, of which the report shows a few dozen.
///
/// So stopping has to happen **at the layer that pushes**, which is here.
///
/// # Why the filter is passed in too
///
/// What is counted has to be the hits that **will finally be reported**, not the raw ones. `keep`
/// is the caller's post-filter (allowlist, inline pragma) moved forward into the loop: counting
/// raw hits, an allowlist that swallows the first `cap` of them returns an empty list while a
/// real hit sits right behind — a bound added purely to save memory flipping the verdict from
/// dirty to clean. See [`scan_text_capped`].
///
/// `keep` receives **the matched span itself** and its start offset in the view, the same pair
/// the post-filter looks at. Filtering before [`dedupe_same_span`] does not change the result:
/// when two rules recognize the same characters, both have the same matched text and the same
/// line, so filtering keeps both or drops both.
///
/// # Why it does not change the verdict
///
/// This bound presses on the **number** produced, not on "is there any": at `cap >= 1`, as long
/// as the text holds any hit **not stopped by `keep`**, the returned list is non-empty and the
/// caller's verdict (clean / dirty) is unchanged.
///
/// # The budget counts unique spans, not the `Raw`s produced
///
/// See [`SpanBudget`]: what is produced is `Raw`s and what is reported is the hits after
/// [`dedupe_same_span`], which are not the same thing.
///
/// # The second return value: did it stop **because it reached `cap`**
///
/// The caller cannot infer that from "is my collector full" — the miss happens **inside a single
/// carrier**, and the collector sees only the few hits handed to it. So this signal has to be
/// passed explicitly out of the producing layer, all the way to
/// [`HitCollector::mark_truncated`].
fn raw_hits_capped(
    view: &str,
    cap: usize,
    mut keep: impl FnMut(&str, usize) -> bool,
) -> (Vec<Raw>, bool) {
    let mut out: Vec<Raw> = vec![];
    if cap == 0 {
        // No budget at all = not one byte was scanned, so this list cannot claim to be complete.
        return (out, true);
    }
    let mut budget = SpanBudget::new(cap);
    // The line text is only needed to decide `regexTarget = "line"`, so it is built on demand.
    let mut lines: Option<Lines> = None;
    'rules: for rule in rules::candidates(view) {
        let Some(re) = rule.regex() else { continue };
        if rule.has_groups() {
            for caps in re.captures_iter(view) {
                let whole = caps.get(0).expect("group 0 always exists");
                let (s, e, secret) = rule.secret_span(&caps);
                let line = lines
                    .get_or_insert_with(|| Lines::new(view))
                    .text_at(whole.start());
                if !rule.accepts(secret, whole.as_str(), line) {
                    continue;
                }
                if !keep(secret, s) {
                    continue;
                }
                // The budget is gone and a reportable hit is right here: stop where it stands
                // and leave both loops.
                if !budget.charge(s, e) {
                    break 'rules;
                }
                out.push(Raw {
                    rule: rule.id.as_str(),
                    start: s,
                    end: e,
                });
            }
        } else {
            for m in re.find_iter(view) {
                let line = lines
                    .get_or_insert_with(|| Lines::new(view))
                    .text_at(m.start());
                if !rule.accepts(m.as_str(), m.as_str(), line) {
                    continue;
                }
                if !keep(m.as_str(), m.start()) {
                    continue;
                }
                if !budget.charge(m.start(), m.end()) {
                    break 'rules;
                }
                out.push(Raw {
                    rule: rule.id.as_str(),
                    start: m.start(),
                    end: m.end(),
                });
            }
        }
    }
    out.sort_by_key(|h| (h.start, h.end));
    dedupe_same_span(&mut out);
    (out, budget.exhausted)
}

/// When two rules recognize the same characters, keep one.
///
/// In practice this is nearly always "`generic-api-key` colliding with a specialized rule" —
/// `token: npm_…` matches both the generic assignment and the npm token. Reporting it twice is
/// not wrong, but the report has a budget (push prints only the first twenty lines), and letting
/// one leak take two lines pushes another leak off the screen. So the more specific rule is the
/// one kept: `generic-api-key` is the only catch-all in the rule set, and whatever it says
/// another rule says more precisely.
fn dedupe_same_span(hits: &mut Vec<Raw>) {
    const CATCH_ALL: &str = "generic-api-key";
    let mut i = 0;
    while i < hits.len() {
        let mut j = i + 1;
        while j < hits.len() && hits[j].start == hits[i].start && hits[j].end == hits[i].end {
            j += 1;
        }
        if j - i > 1 {
            // Within a group prefer a rule that is not the catch-all; if all are, keep the
            // first.
            let keep = (i..j).find(|&k| hits[k].rule != CATCH_ALL).unwrap_or(i);
            hits.swap(i, keep);
            hits.drain(i + 1..j);
        }
        i += 1;
    }
}

pub fn scan_text(text: &str, allowlist: &HashSet<String>) -> Vec<Hit> {
    scan_text_with(text, allowlist, Policy::CLIENT)
}

/// How many hits one piece of text yields: purely a memory cap for dense input. Any single hit
/// is enough for the caller to refuse the text, so the cap does not change the verdict.
const TEXT_SCAN_HIT_CAP: usize = 1024;

/// [`scan_text`] plus the literals the user registered through `agit secrets`.
///
/// A registered value can be low-entropy enough to match no heuristic rule; writing text into
/// history that will be pushed and inherited (collecting and distilling memory) has to reach its
/// verdict under the same rules as the publish gate.
pub fn scan_text_registered(text: &str, allowlist: &HashSet<String>) -> crate::Result<Vec<Hit>> {
    let registered = load_registered_matcher()?;
    Ok(scan_text_registered_with(text, allowlist, &registered))
}

/// The explicit-matcher version of [`scan_text_registered`] (tests, and callers that already
/// hold a matcher).
pub fn scan_text_registered_with(
    text: &str,
    allowlist: &HashSet<String>,
    registered: &RegisteredMatcher,
) -> Vec<Hit> {
    scan_text_capped_registered(
        text,
        allowlist,
        Policy::CLIENT,
        TEXT_SCAN_HIT_CAP,
        registered,
    )
    .hits
}

/// The explicit-policy version of [`scan_text`]. The server passes [`Policy::STRICT`].
pub fn scan_text_with(text: &str, allowlist: &HashSet<String>, policy: Policy) -> Vec<Hit> {
    let view = view_of(text);
    let raw = raw_hits(&view);
    if raw.is_empty() {
        return vec![];
    }
    let lines = Lines::new(&view);
    // With no line carrying the annotation, not even one contains call is needed.
    let any_pragma = policy.inline_pragma && view.contains(INLINE_PRAGMA);
    raw.into_iter()
        .filter_map(|h| {
            let found = &view[h.start..h.end];
            if policy.allowlist && is_allowlisted(found, allowlist) {
                return None;
            }
            if any_pragma && lines.text_at(h.start).contains(INLINE_PRAGMA) {
                return None;
            }
            Some(Hit {
                rule: h.rule.to_string(),
                file: None,
                source: Source::File,
                line: lines.number_at(h.start),
                redacted: redact(found),
                fingerprint: fingerprint(found),
            })
        })
        .collect()
}

/// The **bounded** version of [`scan_text_with`]: stop once `cap` reportable hits are found.
///
/// # Why the bound is here and not a truncation by the caller
///
/// `scan_text` materializes every hit of **a single carrier** (one file, one commit body, one tag
/// body) into a complete `Vec<Hit>` before handing it to the collector. Capping the final list
/// does not stop that stretch: a large file matching on every line still produces `Raw`s / `Hit`s
/// on the order of its line count before `extend` takes over, each carrying its own `String`. So
/// stopping has to happen at the producing layer, see [`raw_hits_capped`].
///
/// # Why the filter moved into the producing loop
///
/// This bound is only safe while **what it counts is the hits that will finally be reported**.
///
/// [`Policy::CLIENT`] honours the allowlist and inline `agit:allow-secret`. With the bound on the
/// **raw output** and the filter left behind it, this happens: the allowlist swallows the first
/// `cap` hits, an empty list comes back, and **a real hit sits right behind them** — a bound
/// added purely to save memory flipping the verdict from "dirty" to "clean". That is fail open,
/// exactly what this gate must not do.
///
/// So the filter goes into the loop together with the bound: what is counted is the hits **that
/// pass the filter**. That makes CLIENT safe too, with no need to hard-code [`Policy::STRICT`] to
/// dodge the flip (hard-coded it is usable only by the server, while the three local scan paths
/// are precisely the ones that need a bound).
///
/// At `cap >= 1` the verdict is unchanged: as long as the text holds one hit that is not allowed,
/// the returned list is non-empty.
///
/// # `cap` counts **reported** hits, not the `Raw`s produced
///
/// The same characters recognized by two rules (`token: npm_…` → `generic-api-key` +
/// `npm-access-token`) dedupe into **one**. Counting `Raw`s makes it eat two slots, so at
/// `cap = 2` the budget is exhausted on the spot and the other kind of secret behind it is never
/// scanned. See [`SpanBudget`].
///
/// # Why it returns a [`ScanReport`] and not a `Vec<Hit>`
///
/// "I stopped early because I reached `cap`" is something the caller **cannot infer**: the miss
/// happens inside this one carrier, and the collector sees only the few hits handed to it — it
/// may not be full at all. Without this signal you get "a hit went unscanned and the report
/// claims to be complete". So the flag travels with the result and is caught by
/// [`HitCollector::mark_truncated`].
pub fn scan_text_capped(
    text: &str,
    allowlist: &HashSet<String>,
    policy: Policy,
    cap: usize,
) -> ScanReport {
    if cap == 0 {
        // No budget at all = not one byte was scanned. The verdict is unaffected (no hit could
        // be reported from here anyway), but "this is all of it" must not be said.
        return ScanReport {
            hits: vec![],
            truncated: true,
            unscanned: Unscanned::default(),
        };
    }
    let view = view_of(text);
    // With no line carrying the annotation, not even the line index is built.
    let any_pragma = policy.inline_pragma && view.contains(INLINE_PRAGMA);
    let pragma_lines = any_pragma.then(|| Lines::new(&view));
    let (raw, truncated) = raw_hits_capped(&view, cap, |found, start| {
        if policy.allowlist && is_allowlisted(found, allowlist) {
            return false;
        }
        match &pragma_lines {
            Some(l) => !l.text_at(start).contains(INLINE_PRAGMA),
            None => true,
        }
    });
    if raw.is_empty() {
        return ScanReport {
            hits: vec![],
            truncated,
            unscanned: Unscanned::default(),
        };
    }
    let lines = pragma_lines.unwrap_or_else(|| Lines::new(&view));
    ScanReport {
        hits: raw
            .into_iter()
            .map(|h| Hit {
                rule: h.rule.to_string(),
                file: None,
                source: Source::File,
                line: lines.number_at(h.start),
                redacted: redact(&view[h.start..h.end]),
                fingerprint: fingerprint(&view[h.start..h.end]),
            })
            .collect(),
        truncated,
        unscanned: Unscanned::default(),
    }
}

/// Fold the literals the user registered explicitly into the client scan.
///
/// The registered rules run first and honour neither the gitleaks allowlist nor the inline
/// pragma: the user has already said "this value is a secret", and project content must not
/// switch that local policy off on the user's behalf. The built-in rules keep the [`Policy`] they
/// were given, and the two share one `cap`, so the memory ceiling for dense input is unchanged.
fn scan_text_capped_registered(
    text: &str,
    allowlist: &HashSet<String>,
    policy: Policy,
    cap: usize,
    registered: &RegisteredMatcher,
) -> ScanReport {
    #[cfg(not(feature = "secret-vault"))]
    {
        let _ = registered;
        return scan_text_capped(text, allowlist, policy, cap);
    }

    #[cfg(feature = "secret-vault")]
    {
        if cap == 0 {
            return ScanReport {
                hits: vec![],
                truncated: true,
                unscanned: Unscanned::default(),
            };
        }

        let (mut hits, registered_truncated) =
            registered_hits_semantic_capped(text, cap, registered);

        if registered_truncated {
            return ScanReport {
                hits,
                truncated: true,
                unscanned: Unscanned::default(),
            };
        }

        let remaining = cap - hits.len();
        if remaining == 0 {
            // When the registered rules fill the budget exactly, probe for one built-in hit so
            // the report can still answer whether it is complete.
            let more = scan_text_capped(text, allowlist, policy, 1);
            return ScanReport {
                hits,
                truncated: more.truncated || !more.hits.is_empty(),
                unscanned: Unscanned::default(),
            };
        }

        let built_in = scan_text_capped(text, allowlist, policy, remaining);
        hits.extend(built_in.hits);
        ScanReport {
            hits,
            truncated: built_in.truncated,
            unscanned: Unscanned::default(),
        }
    }
}

#[cfg(feature = "secret-vault")]
pub(crate) fn registered_hits_semantic_capped(
    text: &str,
    cap: usize,
    registered: &crate::domain::secret_filter::Matcher,
) -> (Vec<Hit>, bool) {
    // Both representations are scanned, and the results are merged.
    //
    // Neither one alone is a gate. The wire bytes miss `"`, `\` and `\n` inside
    // a registered literal, because JSONL stores those escaped. The decoded
    // strings miss a literal that spans a line break, and they miss every byte
    // of a line that is not JSON at all.
    //
    // Deciding between them per *file* — the earlier `is_jsonl` test — is the
    // worst of the two: one half-written trailing line, which is exactly what a
    // crashed harness leaves behind, silently downgraded the whole file to
    // wire-byte matching and let an escaped registered value through the push
    // and share gates. Overlap costs a second pass and a dedupe; a miss costs
    // the secret.
    if cap == 0 {
        return (vec![], true);
    }
    let mut hits: Vec<Hit> = Vec::with_capacity(cap.min(16));
    let mut seen: std::collections::HashSet<(usize, u64)> = std::collections::HashSet::new();
    let mut truncated = false;

    // ── 1. the bytes exactly as they will be published ──
    let (matches, more) = registered.find_capped(text, cap);
    truncated |= more;
    if !matches.is_empty() {
        let lines = Lines::new(text);
        for found in matches {
            let line = lines.number_at(found.start);
            let fingerprint = fingerprint(&text[found.start..found.end]);
            if seen.insert((line, fingerprint)) {
                hits.push(Hit {
                    rule: "registered-secret".to_string(),
                    file: None,
                    source: Source::File,
                    line,
                    redacted: "[redacted:registered-secret]".to_string(),
                    fingerprint,
                });
            }
        }
    }

    // ── 2. the decoded value of every line that is JSON ──
    for (index, line) in text.lines().enumerate() {
        if truncated {
            break;
        }
        // A line that does not parse was already scanned literally above; a
        // parse failure is not permission to stop scanning the rest.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        visit_json_strings(&value, &mut |string| {
            if truncated {
                return;
            }
            let remaining = cap.saturating_sub(hits.len());
            if remaining == 0 {
                // Full. Probe for one more match so the report can still say
                // whether it is complete; a match already recorded by the pass
                // above only over-reports truncation, which is the safe way to
                // be wrong here.
                let (probe, more) = registered.find_capped(string, 1);
                truncated |= more || !probe.is_empty();
                return;
            }
            let (matches, more) = registered.find_capped(string, remaining);
            truncated |= more;
            for found in matches {
                let line = index + 1;
                let fingerprint = fingerprint(&string[found.start..found.end]);
                if seen.insert((line, fingerprint)) {
                    hits.push(Hit {
                        rule: "registered-secret".to_string(),
                        file: None,
                        source: Source::File,
                        line,
                        redacted: "[redacted:registered-secret]".to_string(),
                        fingerprint,
                    });
                }
            }
        });
    }
    (hits, truncated)
}

#[cfg(feature = "secret-vault")]
fn visit_json_strings(value: &serde_json::Value, visit: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::String(text) => visit(text),
        serde_json::Value::Array(values) => {
            for value in values {
                visit_json_strings(value, visit);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                visit(key);
                visit_json_strings(value, visit);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Byte ranges of heuristic findings too long to become a reversible record.
///
/// Ranges, not values. The caller needs to know which regions of this string to
/// copy verbatim, and a finding this large is exactly the thing not to clone,
/// not to deduplicate by content comparison, and above all not to hand to a
/// pattern automaton: Aho–Corasick sizes itself on total pattern length, so a
/// transcript carrying many long private keys would build an automaton several
/// times the input it was meant to protect.
///
/// The output is bounded without needing a budget: every range is longer than
/// `threshold`, so a string yields at most `len / threshold` of them per rule.
/// Callers can skip the scan outright when `text.len() <= threshold`, which is
/// an exact test rather than a heuristic — a longer match cannot fit.
///
/// Ranges are in `text` coordinates. `view_of` preserves length, so they line
/// up with the same string as it is seen by [`secret_candidates_jsonl`].
#[cfg(feature = "secret-vault")]
pub(crate) fn oversized_finding_spans(text: &str, threshold: usize) -> Vec<(usize, usize)> {
    let view = view_of(text);
    let mut spans: Vec<(usize, usize)> = vec![];
    // `keep` always refuses, so no `Raw` is materialized and no span budget is
    // charged: this pass exists only to observe where the long findings are.
    let (_, _) = raw_hits_capped(&view, usize::MAX, |found, start| {
        if found.len() > threshold {
            spans.push((start, start + found.len()));
        }
        false
    });
    spans.sort_unstable();
    spans
}

/// Extract exact heuristic matches from user-authored JSONL values for the
/// repository-local dictionary. Public scan reports stay redacted; only this
/// settlement-local path is allowed to own the matched plaintext.
/// The predicate lets the repository dictionary count only candidates that
/// are not already represented locally. It executes while the scanner owns the
/// borrowed plaintext; rejected values are never copied into the returned
/// batch.
#[cfg(feature = "secret-vault")]
pub(crate) fn secret_candidates_jsonl(
    text: &str,
    cap: usize,
    mut include: impl FnMut(&str) -> bool,
) -> SecretCandidateBatch {
    let mut values: Vec<Zeroizing<String>> = Vec::with_capacity(cap.min(16));
    let mut truncated = false;

    for line in text.lines() {
        if truncated || line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(value) => visit_candidate_values(&value, None, &mut |candidate| {
                collect_candidate(candidate, cap, &mut values, &mut truncated, &mut include)
            }),
            // A half-written or legacy plaintext line is still publishable
            // content. Scan it literally instead of treating parse failure as
            // permission to bypass automatic protection.
            Err(_) => collect_candidate(line, cap, &mut values, &mut truncated, &mut include),
        }
    }

    SecretCandidateBatch { values, truncated }
}

#[cfg(feature = "secret-vault")]
fn collect_candidate(
    text: &str,
    cap: usize,
    values: &mut Vec<Zeroizing<String>>,
    truncated: &mut bool,
    include: &mut impl FnMut(&str) -> bool,
) {
    if *truncated || text.is_empty() {
        return;
    }
    let view = view_of(text);
    let remaining = cap.saturating_sub(values.len());
    let mut newly_seen: Vec<Zeroizing<String>> = Vec::with_capacity(remaining.min(16));
    // Filter duplicate literal values inside the producer. Its budget then
    // counts distinct dictionary records rather than repeated occurrences of
    // the same token in a large transcript.
    let (_, more) = raw_hits_capped(&view, remaining.saturating_add(1), |found, start| {
        let literal = &text[start..start + found.len()];
        if values.iter().any(|known| known.as_str() == literal)
            || newly_seen.iter().any(|known| known.as_str() == literal)
            || !include(literal)
        {
            return false;
        }
        newly_seen.push(Zeroizing::new(literal.to_string()));
        true
    });
    for literal in newly_seen {
        if values.len() == cap {
            *truncated = true;
            break;
        }
        values.push(literal);
    }
    *truncated |= more;
}

#[cfg(feature = "secret-vault")]
fn visit_candidate_values(
    value: &serde_json::Value,
    field: Option<&str>,
    visit: &mut impl FnMut(&str),
) {
    if field.is_some_and(is_protocol_identity_field) {
        return;
    }
    match value {
        serde_json::Value::String(text) => visit(text),
        serde_json::Value::Array(values) => {
            for value in values {
                visit_candidate_values(value, field, visit);
            }
        }
        serde_json::Value::Object(map) => {
            // Keys are protocol structure, not session content. Only values
            // are eligible for heuristic insertion.
            for (key, value) in map {
                visit_candidate_values(value, Some(key), visit);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

#[cfg(feature = "secret-vault")]
fn is_protocol_identity_field(field: &str) -> bool {
    matches!(
        field.to_ascii_lowercase().as_str(),
        "_session_id"
            | "session_id"
            | "_object_hash"
            | "object_hash"
            | "event_id"
            | "commit_id"
            | "tree_id"
            | "blob_id"
            | "oid"
            | "sha"
            | "ref"
            | "ref_name"
            | "schema_version"
            | "layout_version"
            | "projection_version"
            | "signature"
            | "created_at"
            | "updated_at"
            | "timestamp"
            | "provenance"
    )
}

/// Scan a repository file or blob while moving the envelope identity fields AgentGit generates
/// itself out of the rules' view.
///
/// The gitleaks Sourcegraph rule treats any high-entropy 40-hex string as a token, and an event
/// envelope's `_session_id` / `_object_hash` are exactly 40-hex strings AgentGit generated. The
/// scan result is then written into the next commit, closing the loop "report my own hash →
/// produce another hash → report it again".
///
/// Files are not skipped by path here, and whole JSON documents are not skipped either. Only when
/// **the entire payload** consists of valid envelopes, and every line passes the shape, session
/// id and `object_hash(content)` integrity checks of
/// [`crate::domain::storage::parse_legacy_envelope_line`], is the `_object_hash` that content can
/// recompute masked; and `_session_id` additionally has to match the `(session, runtime)`
/// declared by a safely read `session/meta.json`, or by an AgentGit merge source validated
/// through Git parents and meta continuity. Envelope shape alone cannot earn the exemption — any
/// file can forge this JSON and stuff a real 40-hex credential into `agit-<credential>`.
/// `_source`, `content` and every other raw byte are scanned as usual; one corrupt line, one
/// extra field, or one hash that does not match sends the whole payload back to scanning the raw
/// text (fail closed).
///
/// The replacement happens on field spans of the original JSON rather than putting the internal
/// value on a global allowlist: the same bytes appearing inside `content` still have to match.
/// The output is still one scan-view line per envelope line, so reported line numbers do not
/// drift.
fn scan_repository_payload_capped(
    text: &str,
    allowlist: &HashSet<String>,
    trusted_identities: &TrustedEnvelopeIdentities,
    policy: Policy,
    cap: usize,
    registered: &RegisteredMatcher,
) -> ScanReport {
    // The mask replaces only the value spans of the two identity fields with quoted sentinels,
    // so every line stays valid JSON and the registered rules keep matching semantic strings on
    // the masked view. The two masked fields are identities AgentGit generated itself, and the
    // repository dictionary excludes them from its candidates in `is_protocol_identity_field` —
    // both sites keep one boundary for "which fields are not session content".
    let Some(view) = mask_valid_envelope_stream(text, trusted_identities) else {
        return scan_text_capped_registered(text, allowlist, policy, cap, registered);
    };
    scan_text_capped_registered(&view, allowlist, policy, cap, registered)
}

/// The `(session id, runtime)` pairs the repository metadata has declared.
///
/// A session id's format alone is not a trusted source: any file can forge an Envelope whose hash
/// is self-consistent. Only the safe meta of the current working tree, a `session/meta.json` that
/// passed strict validation on a local branch tip, or the second-parent meta of a reachable
/// AgentGit merge whose LOG delta is a complete, verifiable AgentGit merge block can authorize
/// masking `_session_id`. The runtime is bound along with it, so borrowing a session id alone
/// cannot impersonate a different producer.
type TrustedEnvelopeIdentities = HashMap<String, HashSet<String>>;

fn trust_meta_identity(trusted: &mut TrustedEnvelopeIdentities, meta: &crate::domain::meta::Meta) {
    if !meta.is_session_line()
        || meta.session.is_empty()
        || crate::domain::meta::validate(meta).is_err()
    {
        return;
    }
    trusted
        .entry(meta.session.clone())
        .or_default()
        .insert(meta.runtime.clone());
}

fn worktree_trusted_envelope_identities(
    repo: &crate::domain::repo::Repo,
    branch_identities: &TrustedEnvelopeIdentities,
) -> TrustedEnvelopeIdentities {
    let mut trusted = branch_identities.clone();
    // `resolve` refuses symlink and special-file meta and validates the field invariants; if it
    // cannot be read, no exemption is granted and the scan stays on the raw text. When this
    // helper path fails, the direction has to be over-reporting, never under-reporting.
    if let Ok(meta) = crate::domain::meta::resolve(repo.root()) {
        trust_meta_identity(&mut trusted, &meta);
    }
    trusted
}

fn branch_trusted_envelope_identities(
    repo: &crate::domain::repo::Repo,
    branches: &[String],
) -> TrustedEnvelopeIdentities {
    let mut trusted = TrustedEnvelopeIdentities::new();
    let mut budget = ProvenanceReadBudget::new();
    let tip_specs: Vec<String> = branches
        .iter()
        .map(|branch| format!("refs/heads/{branch}:{}", crate::domain::meta::FILE))
        .collect();
    for meta in read_trusted_meta_batch(repo, &tip_specs, &mut budget)
        .into_iter()
        .flatten()
    {
        // Missing, corrupt, oversized, or a failed git read all mean only "this identity cannot
        // be proven to be AgentGit's": no exemption for `_session_id`, and the blob scan behind
        // it still reaches its verdict on the raw text.
        if meta.is_session_line() {
            trust_meta_identity(&mut trusted, &meta);
        }
    }
    trust_merge_source_identities(repo, &mut trusted, &mut budget);
    trusted
}

/// Recover source identities from AgentGit merges still reachable from a local branch.
///
/// A merge folds the source events' original envelopes into the target LOG/VIEW; even after the
/// source branch is deleted, the source head is still the merge commit's second parent, so its
/// meta remains part of the provenance. Walking two-parent commits, or even checking `meta.kind`,
/// is not enough: after one real merge, an ordinary `git merge -s ours` inherits the first
/// parent's `kind: merge` unchanged. This also requires the resulting LOG to add, strictly
/// relative to the first parent, `start + the complete source LOG + summary + end`, with all
/// three synthetic events belonging to the target identity and self-consistent in body shape and
/// event id; anything missing, corrupt or discontinuous grants the second parent's identity no
/// exemption.
///
/// The meta cannot be read one at a time with `read_at_ref_result`: that path starts three
/// processes (`rev-parse`, `ls-tree`, `cat-file`) per call, and three metas times the number of
/// historical merges amplifies without bound before the budget is even consulted. This enumerates
/// as a stream and reads meta, LOG sequences and synthetic events in batches of
/// [`MERGE_META_BATCH`] merges; memory and process count are both capped per batch.
fn trust_merge_source_identities(
    repo: &crate::domain::repo::Repo,
    trusted: &mut TrustedEnvelopeIdentities,
    budget: &mut ProvenanceReadBudget,
) {
    let before = trusted.clone();
    let mut batch = Vec::with_capacity(MERGE_META_BATCH);
    note_trusted_provenance_git_process();
    let read = repo.git_stream_split(
        &[
            "rev-list",
            "--parents",
            "--min-parents=2",
            "--max-parents=2",
            "--branches",
        ],
        b'\n',
        |record| {
            let Ok(line) = std::str::from_utf8(record) else {
                return Ok(());
            };
            let mut fields = line.split_ascii_whitespace();
            let (Some(merge_commit), Some(first_parent), Some(source_parent)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Ok(());
            };
            if fields.next().is_some() {
                return Ok(());
            }

            batch.push(MergeMetaRefs {
                merge_commit: merge_commit.to_string(),
                first_parent: first_parent.to_string(),
                source_parent: source_parent.to_string(),
            });
            if batch.len() == MERGE_META_BATCH {
                trust_merge_source_batch(repo, trusted, &batch, budget);
                batch.clear();
            }
            Ok(())
        },
    );
    if read.is_err() {
        // When enumeration only got through part of the history, that part must not be called
        // complete provenance. Restore the tip identity set from before this function ran; every
        // merge source falls back to scanning the raw text.
        *trusted = before;
        return;
    }
    trust_merge_source_batch(repo, trusted, &batch, budget);
}

const MERGE_META_BATCH: usize = 256;
const TRUSTED_META_MAX_BYTES: u64 = 1024 * 1024;
/// Merge provenance is only an optimization that suppresses false positives. Once its bounded
/// validation budget is exhausted we simply stop granting historical identities; the repository
/// bytes are still scanned normally, so the failure direction remains safe.
const TRUSTED_PROVENANCE_MAX_BYTES: u64 = 512 * 1024 * 1024;
const TRUSTED_PROVENANCE_OBJECT_MAX_BYTES: u64 = 64 * 1024 * 1024;

struct ProvenanceReadBudget {
    remaining: u64,
    exhausted: bool,
}

impl ProvenanceReadBudget {
    fn new() -> Self {
        Self {
            remaining: TRUSTED_PROVENANCE_MAX_BYTES,
            exhausted: false,
        }
    }

    fn reserve(&mut self, bytes: u64) -> bool {
        if self.exhausted || bytes > self.remaining {
            self.remaining = 0;
            self.exhausted = true;
            return false;
        }
        self.remaining -= bytes;
        true
    }
}

struct MergeMetaRefs {
    merge_commit: String,
    first_parent: String,
    source_parent: String,
}

struct MergeProvenanceCandidate {
    merge_commit: String,
    first_layout: crate::domain::meta::LayoutVersion,
    source_layout: crate::domain::meta::LayoutVersion,
    target_session: String,
    target_runtime: String,
    source_meta: crate::domain::meta::Meta,
}

struct ValidatedMergeLog {
    merge_commit: String,
    source_parent: String,
    source_log_oid: String,
    target_session: String,
    target_runtime: String,
    source_meta: crate::domain::meta::Meta,
    source_event_ids: Option<std::sync::Arc<Vec<String>>>,
    marker_ids: [String; 3],
}

fn trust_merge_source_batch(
    repo: &crate::domain::repo::Repo,
    trusted: &mut TrustedEnvelopeIdentities,
    batch: &[MergeMetaRefs],
    budget: &mut ProvenanceReadBudget,
) {
    if batch.is_empty() || budget.exhausted {
        return;
    }
    let mut specs = Vec::with_capacity(batch.len() * 3);
    for refs in batch {
        specs.push(format!(
            "{}:{}",
            refs.merge_commit,
            crate::domain::meta::FILE
        ));
        specs.push(format!(
            "{}:{}",
            refs.first_parent,
            crate::domain::meta::FILE
        ));
        specs.push(format!(
            "{}:{}",
            refs.source_parent,
            crate::domain::meta::FILE
        ));
    }
    let metas = read_trusted_meta_batch(repo, &specs, budget);
    let mut candidates = Vec::with_capacity(batch.len());
    for (index, refs) in batch.iter().enumerate() {
        let offset = index * 3;
        let triple = &metas[offset..offset + 3];
        let (Some(merge_meta), Some(first_meta), Some(source_meta)) =
            (triple[0].as_ref(), triple[1].as_ref(), triple[2].as_ref())
        else {
            continue;
        };
        if merge_meta.kind != crate::domain::meta::Kind::Merge
            || merge_meta.layout != crate::domain::meta::LayoutVersion::V1
            || !merge_meta.is_session_line()
            || !first_meta.is_session_line()
            || !source_meta.is_session_line()
            || merge_meta.session != first_meta.session
            || merge_meta.runtime != first_meta.runtime
        {
            continue;
        }
        candidates.push(MergeProvenanceCandidate {
            merge_commit: refs.merge_commit.clone(),
            first_layout: first_meta.layout,
            source_layout: source_meta.layout,
            target_session: merge_meta.session.clone(),
            target_runtime: merge_meta.runtime.clone(),
            source_meta: source_meta.clone(),
        });
    }

    let validated = validate_merge_logs_batch(repo, batch, &candidates, budget);
    let validated = validate_merge_source_events_batch(repo, validated, budget);
    for merge in validate_merge_markers_batch(repo, validated, budget) {
        trust_meta_identity(trusted, &merge.source_meta);
    }
}

#[derive(Clone)]
struct SequenceFingerprint {
    normalized_len: usize,
    digest: [u8; 32],
}

/// Validate that each candidate merge's LOG really is the first parent's LOG plus one complete
/// source block.
///
/// v1 digests the canonical event-id sequence directly; v0 normalizes line by line into event ids
/// first and digests those. So three materialized transcripts of up to 512 MiB each never have to
/// sit in memory at once — only a fixed-length digest outlives the callback. The resulting LOG has
/// to be v1 (a real merge writes the current layout), its prefix and source segment have to equal
/// the canonical sequence digests of the two parents, and the only additional event ids allowed
/// are start / summary / end.
fn validate_merge_logs_batch(
    repo: &crate::domain::repo::Repo,
    refs: &[MergeMetaRefs],
    candidates: &[MergeProvenanceCandidate],
    budget: &mut ProvenanceReadBudget,
) -> Vec<ValidatedMergeLog> {
    if candidates.is_empty() {
        return vec![];
    }

    let mut specs = Vec::with_capacity(candidates.len() * 3);
    for candidate in candidates {
        let Some(refs) = refs
            .iter()
            .find(|refs| refs.merge_commit == candidate.merge_commit)
        else {
            continue;
        };
        specs.push(format!(
            "{}:{}",
            refs.first_parent,
            log_path(candidate.first_layout)
        ));
        specs.push(format!(
            "{}:{}",
            refs.source_parent,
            log_path(candidate.source_layout)
        ));
        specs.push(format!(
            "{}:{}",
            refs.merge_commit,
            crate::domain::meta::LOG_FILE
        ));
    }
    if specs.len() != candidates.len() * 3 {
        return vec![];
    }

    let Some(resolved) =
        resolve_provenance_blobs(repo, &specs, TRUSTED_PROVENANCE_OBJECT_MAX_BYTES, budget)
    else {
        return vec![];
    };
    let Some(blobs) = read_resolved_provenance_blobs(repo, &resolved) else {
        return vec![];
    };

    let mut source_sequences = HashMap::<
        (String, bool, String, String),
        Option<Option<std::sync::Arc<Vec<String>>>>,
    >::new();
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let refs = refs
                .iter()
                .find(|refs| refs.merge_commit == candidate.merge_commit)?;
            let first = blobs.get(resolved.slot_oids.get(index * 3)?.as_ref()?)?;
            let source_log_oid = resolved.slot_oids.get(index * 3 + 1)?.as_ref()?;
            let source = blobs.get(source_log_oid)?;
            let merged = blobs.get(resolved.slot_oids.get(index * 3 + 2)?.as_ref()?)?;
            let first_fingerprint = sequence_fingerprint(first, candidate.first_layout)?;
            let source_fingerprint = sequence_fingerprint(source, candidate.source_layout)?;
            let source_key = (
                source_log_oid.clone(),
                candidate.source_layout == crate::domain::meta::LayoutVersion::V1,
                candidate.source_meta.session.clone(),
                candidate.source_meta.runtime.clone(),
            );
            let source_event_ids = source_sequences
                .entry(source_key)
                .or_insert_with(|| {
                    validate_source_sequence(
                        source,
                        candidate.source_layout,
                        &candidate.source_meta,
                    )
                    .map(|ids| ids.map(std::sync::Arc::new))
                })
                .as_ref()?
                .clone();
            let marker_ids =
                validate_merged_sequence(merged, &first_fingerprint, &source_fingerprint)?;
            Some(ValidatedMergeLog {
                merge_commit: candidate.merge_commit.clone(),
                source_parent: refs.source_parent.clone(),
                source_log_oid: source_log_oid.clone(),
                target_session: candidate.target_session.clone(),
                target_runtime: candidate.target_runtime.clone(),
                source_meta: candidate.source_meta.clone(),
                source_event_ids,
                marker_ids,
            })
        })
        .collect()
}

struct ResolvedProvenanceBlobs {
    /// One entry per input spec. `None` is never produced by the successful resolver, but retaining
    /// the option makes accidental protocol drift fail closed at each consumer.
    slot_oids: Vec<Option<String>>,
    unique_oids: Vec<String>,
}

/// Resolve `commit:path` expressions to immutable blob OIDs, deduplicate them, and reserve their
/// cumulative size before asking Git for a single body byte.
fn resolve_provenance_blobs(
    repo: &crate::domain::repo::Repo,
    specs: &[String],
    max_object_bytes: u64,
    budget: &mut ProvenanceReadBudget,
) -> Option<ResolvedProvenanceBlobs> {
    if specs.is_empty() || budget.exhausted {
        return None;
    }

    let mut slot_oids = Vec::with_capacity(specs.len());
    let mut unique_oids = Vec::new();
    let mut unique_sizes = HashMap::<String, u64>::new();
    let mut checked = 0usize;
    note_trusted_provenance_git_process();
    let check = repo.git_cat_file_batch_check(specs.to_vec(), |oid, kind, size| {
        checked += 1;
        if kind != "blob" || size > max_object_bytes {
            slot_oids.push(None);
            return Ok(());
        }
        let oid = oid.to_owned();
        slot_oids.push(Some(oid.clone()));
        if let Some(previous) = unique_sizes.get(&oid) {
            if *previous != size {
                anyhow::bail!("git reported conflicting sizes for provenance blob {oid}");
            }
        } else {
            unique_sizes.insert(oid.clone(), size);
            unique_oids.push(oid);
        }
        Ok(())
    });
    if check.is_err()
        || checked != specs.len()
        || slot_oids.len() != specs.len()
        || slot_oids.iter().any(Option::is_none)
    {
        return None;
    }
    let total = unique_sizes
        .values()
        .try_fold(0u64, |sum, size| sum.checked_add(*size))?;
    if !budget.reserve(total) {
        return None;
    }
    Some(ResolvedProvenanceBlobs {
        slot_oids,
        unique_oids,
    })
}

fn read_resolved_provenance_blobs(
    repo: &crate::domain::repo::Repo,
    resolved: &ResolvedProvenanceBlobs,
) -> Option<HashMap<String, Vec<u8>>> {
    let mut blobs = HashMap::with_capacity(resolved.unique_oids.len());
    let mut read = 0usize;
    note_trusted_provenance_git_process();
    let bodies = repo.git_cat_file_batch(
        resolved.unique_oids.clone(),
        usize::MAX,
        |oid, kind, body| {
            read += 1;
            if kind != "blob" {
                anyhow::bail!("resolved provenance object {oid} changed type to {kind}");
            }
            let crate::domain::repo::ObjectBody::Read(bytes) = body else {
                anyhow::bail!("resolved provenance object {oid} was not read");
            };
            note_trusted_provenance_payload_bytes(bytes.len());
            if blobs.insert(oid.to_owned(), bytes.to_vec()).is_some() {
                anyhow::bail!("git returned duplicate provenance blob {oid}");
            }
            Ok(())
        },
    );
    (bodies.is_ok()
        && read == resolved.unique_oids.len()
        && blobs.len() == resolved.unique_oids.len())
    .then_some(blobs)
}

fn log_path(layout: crate::domain::meta::LayoutVersion) -> &'static str {
    match layout {
        crate::domain::meta::LayoutVersion::V0 => crate::domain::meta::LEGACY_LOG_FILE,
        crate::domain::meta::LayoutVersion::V1 => crate::domain::meta::LOG_FILE,
    }
}

fn sequence_fingerprint(
    bytes: &[u8],
    layout: crate::domain::meta::LayoutVersion,
) -> Option<SequenceFingerprint> {
    use sha2::Digest as _;

    let mut digest = sha2::Sha256::new();
    let mut count = 0usize;
    match layout {
        crate::domain::meta::LayoutVersion::V1 => {
            if !bytes
                .len()
                .is_multiple_of(crate::domain::meta::EVENT_ID_HEX_LEN + 1)
            {
                return None;
            }
            let line_bytes = crate::domain::meta::EVENT_ID_HEX_LEN + 1;
            let mut offset = 0usize;
            while offset < bytes.len() {
                let line = &bytes[offset..offset + line_bytes];
                if line[crate::domain::meta::EVENT_ID_HEX_LEN] != b'\n'
                    || !line[..crate::domain::meta::EVENT_ID_HEX_LEN]
                        .iter()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
                {
                    return None;
                }
                count = count.checked_add(1)?;
                if count > crate::domain::storage::MAX_SEQUENCE_EVENTS {
                    return None;
                }
                offset += line_bytes;
            }
            digest.update(bytes);
        }
        crate::domain::meta::LayoutVersion::V0 => {
            let text = std::str::from_utf8(bytes).ok()?;
            for line in text.split_inclusive('\n') {
                let envelope = crate::domain::storage::parse_legacy_envelope_line(line).ok()?;
                let canonical = crate::domain::storage::envelope_line(&envelope);
                let event_id = crate::domain::storage::event_id(&canonical).ok()?;
                digest.update(event_id.as_bytes());
                digest.update(b"\n");
                count = count.checked_add(1)?;
                if count > crate::domain::storage::MAX_SEQUENCE_EVENTS {
                    return None;
                }
            }
            if !text.is_empty() && !text.ends_with('\n') {
                return None;
            }
        }
    }
    Some(SequenceFingerprint {
        normalized_len: count.checked_mul(crate::domain::meta::EVENT_ID_HEX_LEN + 1)?,
        digest: digest.finalize().into(),
    })
}

/// Validate that the source sequence actually belongs to the source meta identity. A v0 LOG owns
/// its envelopes inline; a v1 LOG owns event IDs, which are returned for object-level validation.
/// Empty source histories never establish an identity.
fn validate_source_sequence(
    bytes: &[u8],
    layout: crate::domain::meta::LayoutVersion,
    source_meta: &crate::domain::meta::Meta,
) -> Option<Option<Vec<String>>> {
    match layout {
        crate::domain::meta::LayoutVersion::V0 => {
            let text = std::str::from_utf8(bytes).ok()?;
            let mut count = 0usize;
            for line in text.split_inclusive('\n') {
                let envelope = crate::domain::storage::parse_legacy_envelope_line(line).ok()?;
                if envelope.session_id != source_meta.session
                    || envelope.source != source_meta.runtime
                {
                    return None;
                }
                count = count.checked_add(1)?;
            }
            (!text.is_empty() && text.ends_with('\n') && count > 0).then_some(None)
        }
        crate::domain::meta::LayoutVersion::V1 => {
            let text = std::str::from_utf8(bytes).ok()?;
            let ids = crate::domain::storage::parse_sequence(text).ok()?;
            (!ids.is_empty()).then_some(Some(ids))
        }
    }
}

fn validate_merged_sequence(
    bytes: &[u8],
    first: &SequenceFingerprint,
    source: &SequenceFingerprint,
) -> Option<[String; 3]> {
    use sha2::Digest as _;

    sequence_fingerprint(bytes, crate::domain::meta::LayoutVersion::V1)?;
    let line_bytes = crate::domain::meta::EVENT_ID_HEX_LEN + 1;
    let expected = first
        .normalized_len
        .checked_add(line_bytes)?
        .checked_add(source.normalized_len)?
        .checked_add(line_bytes.checked_mul(2)?)?;
    if bytes.len() != expected {
        return None;
    }

    let start_offset = first.normalized_len;
    let source_offset = start_offset.checked_add(line_bytes)?;
    let summary_offset = source_offset.checked_add(source.normalized_len)?;
    let end_offset = summary_offset.checked_add(line_bytes)?;
    if <[u8; 32]>::from(sha2::Sha256::digest(&bytes[..start_offset])) != first.digest
        || <[u8; 32]>::from(sha2::Sha256::digest(&bytes[source_offset..summary_offset]))
            != source.digest
    {
        return None;
    }

    let event_id = |offset: usize| {
        std::str::from_utf8(&bytes[offset..offset + crate::domain::meta::EVENT_ID_HEX_LEN])
            .ok()
            .map(str::to_owned)
    };
    Some([
        event_id(start_offset)?,
        event_id(summary_offset)?,
        event_id(end_offset)?,
    ])
}

const TRUSTED_MERGE_MARKER_MAX_BYTES: usize = 1024 * 1024;
const TRUSTED_SOURCE_EVENT_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
struct EnvelopeEvidence {
    event_id: String,
    session_id: String,
    runtime: String,
    merge_start_source: Option<String>,
    merge_end_source: Option<String>,
    merge_summary: bool,
}

fn envelope_evidence(bytes: &[u8]) -> Option<EnvelopeEvidence> {
    let text = std::str::from_utf8(bytes).ok()?;
    let envelope = crate::domain::storage::parse_envelope_line(text).ok()?;
    Some(EnvelopeEvidence {
        event_id: crate::domain::storage::event_id(text).ok()?,
        session_id: envelope.session_id.clone(),
        runtime: envelope.source.clone(),
        merge_start_source: merge_marker_source(&envelope, "__merge_start__"),
        merge_end_source: merge_marker_source(&envelope, "__merge_end__"),
        merge_summary: is_merge_summary(&envelope),
    })
}

/// A v1 source LOG is only an index. Every referenced event must exist in the source parent and its
/// canonical bytes must hash to the indexed ID while carrying the source meta identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SourceSequenceKey {
    source_parent: String,
    source_log_oid: String,
    session: String,
    runtime: String,
}

fn source_sequence_key(candidate: &ValidatedMergeLog) -> SourceSequenceKey {
    SourceSequenceKey {
        source_parent: candidate.source_parent.clone(),
        source_log_oid: candidate.source_log_oid.clone(),
        session: candidate.source_meta.session.clone(),
        runtime: candidate.source_meta.runtime.clone(),
    }
}

fn validate_merge_source_events_batch(
    repo: &crate::domain::repo::Repo,
    candidates: Vec<ValidatedMergeLog>,
    budget: &mut ProvenanceReadBudget,
) -> Vec<ValidatedMergeLog> {
    if candidates.is_empty() {
        return vec![];
    }

    let mut sequences = HashMap::<SourceSequenceKey, std::sync::Arc<Vec<String>>>::new();
    for candidate in &candidates {
        if let Some(ids) = &candidate.source_event_ids {
            let key = source_sequence_key(candidate);
            if let Some(previous) = sequences.get(&key) {
                if previous.as_slice() != ids.as_slice() {
                    return candidates
                        .into_iter()
                        .filter(|candidate| candidate.source_event_ids.is_none())
                        .collect();
                }
            } else {
                sequences.insert(key, std::sync::Arc::clone(ids));
            }
        }
    }
    if sequences.is_empty() {
        return candidates;
    }

    let mut specs = Vec::new();
    let mut event_slots = HashMap::<(String, String), usize>::new();
    for (key, ids) in &sequences {
        for id in ids.iter() {
            let event_key = (key.source_parent.clone(), id.clone());
            if event_slots.contains_key(&event_key) {
                continue;
            }
            let Ok(path) = crate::domain::meta::event_path(id) else {
                return candidates
                    .into_iter()
                    .filter(|candidate| candidate.source_event_ids.is_none())
                    .collect();
            };
            event_slots.insert(event_key, specs.len());
            specs.push(format!("{}:{path}", key.source_parent));
        }
    }
    let Some(resolved) =
        resolve_provenance_blobs(repo, &specs, TRUSTED_SOURCE_EVENT_MAX_BYTES, budget)
    else {
        return candidates
            .into_iter()
            .filter(|candidate| candidate.source_event_ids.is_none())
            .collect();
    };
    let Some(blobs) = read_resolved_provenance_blobs(repo, &resolved) else {
        return candidates
            .into_iter()
            .filter(|candidate| candidate.source_event_ids.is_none())
            .collect();
    };
    let evidence: HashMap<String, EnvelopeEvidence> = blobs
        .into_iter()
        .filter_map(|(oid, bytes)| envelope_evidence(&bytes).map(|evidence| (oid, evidence)))
        .collect();

    let valid_sequences: HashSet<SourceSequenceKey> = sequences
        .into_iter()
        .filter_map(|(key, ids)| {
            (!ids.is_empty()
                && ids.iter().all(|expected_id| {
                    let event_key = (key.source_parent.clone(), expected_id.clone());
                    let Some(slot) = event_slots.get(&event_key) else {
                        return false;
                    };
                    let Some(Some(oid)) = resolved.slot_oids.get(*slot) else {
                        return false;
                    };
                    evidence.get(oid).is_some_and(|event| {
                        event.event_id == *expected_id
                            && event.session_id == key.session
                            && event.runtime == key.runtime
                    })
                }))
            .then_some(key)
        })
        .collect();

    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.source_event_ids.is_none()
                || valid_sequences.contains(&source_sequence_key(candidate))
        })
        .collect()
}

/// Validate that the three event ids the LOG adds really point at AgentGit's start / summary /
/// end.
fn validate_merge_markers_batch(
    repo: &crate::domain::repo::Repo,
    candidates: Vec<ValidatedMergeLog>,
    budget: &mut ProvenanceReadBudget,
) -> Vec<ValidatedMergeLog> {
    if candidates.is_empty() {
        return vec![];
    }
    let mut specs = Vec::with_capacity(candidates.len() * 3);
    for candidate in &candidates {
        for id in &candidate.marker_ids {
            let Ok(path) = crate::domain::meta::event_path(id) else {
                return vec![];
            };
            specs.push(format!("{}:{path}", candidate.merge_commit));
        }
    }

    let Some(resolved) =
        resolve_provenance_blobs(repo, &specs, TRUSTED_MERGE_MARKER_MAX_BYTES as u64, budget)
    else {
        return vec![];
    };
    let Some(blobs) = read_resolved_provenance_blobs(repo, &resolved) else {
        return vec![];
    };
    let evidence: HashMap<String, EnvelopeEvidence> = blobs
        .into_iter()
        .filter_map(|(oid, bytes)| envelope_evidence(&bytes).map(|evidence| (oid, evidence)))
        .collect();

    candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let event = |position: usize| {
                let oid = resolved.slot_oids.get(index * 3 + position)?.as_ref()?;
                evidence.get(oid)
            };
            let start = event(0)?;
            let summary = event(1)?;
            let end = event(2)?;
            let identity_matches = |event: &EnvelopeEvidence| {
                event.session_id == candidate.target_session
                    && event.runtime == candidate.target_runtime
            };
            (identity_matches(start)
                && identity_matches(summary)
                && identity_matches(end)
                && start.event_id == candidate.marker_ids[0]
                && summary.event_id == candidate.marker_ids[1]
                && end.event_id == candidate.marker_ids[2]
                && summary.merge_summary
                && start.merge_start_source.is_some()
                && start.merge_start_source == end.merge_end_source)
                .then_some(candidate)
        })
        .collect()
}

fn merge_marker_source(
    envelope: &crate::domain::transcript::Envelope,
    subtype: &str,
) -> Option<String> {
    let content = envelope.content.as_object()?;
    if content.len() != 3
        || content.get("type")?.as_str()? != "system"
        || content.get("subtype")?.as_str()? != format!("agit:{subtype}")
    {
        return None;
    }
    let source = content.get("source")?.as_str()?;
    if source.is_empty() || source.len() > 1024 || source.contains(['\n', '\r']) {
        return None;
    }
    Some(source.to_owned())
}

fn is_merge_summary(envelope: &crate::domain::transcript::Envelope) -> bool {
    let Some(content) = envelope.content.as_object() else {
        return false;
    };
    let Some(message) = content
        .get("message")
        .and_then(serde_json::Value::as_object)
    else {
        return false;
    };
    content.len() == 3
        && content.get("type").and_then(serde_json::Value::as_str) == Some("user")
        && content.get("agit").and_then(serde_json::Value::as_str) == Some("merge_summary")
        && message.len() == 2
        && message.get("role").and_then(serde_json::Value::as_str) == Some("user")
        && message
            .get("content")
            .and_then(serde_json::Value::as_str)
            .is_some()
}

/// Read `commit:session/meta.json` in strict, fixed-size batches.
///
/// `commit:path` is first resolved to an immutable blob OID with its length; a missing object, a
/// non-blob, anything over 1 MiB, or an insufficient cumulative provenance budget means the body
/// is never requested. Only then are the unique verified OIDs read. Every output has to line up
/// item for item with the input, and any process or protocol failure makes the whole batch return
/// `None` — the direction is always fewer exemptions.
fn read_trusted_meta_batch(
    repo: &crate::domain::repo::Repo,
    specs: &[String],
    budget: &mut ProvenanceReadBudget,
) -> Vec<Option<crate::domain::meta::Meta>> {
    let mut out: Vec<Option<crate::domain::meta::Meta>> =
        std::iter::repeat_with(|| None).take(specs.len()).collect();
    if specs.is_empty() {
        return out;
    }

    let Some(resolved) = resolve_provenance_blobs(repo, specs, TRUSTED_META_MAX_BYTES, budget)
    else {
        return out;
    };
    let Some(blobs) = read_resolved_provenance_blobs(repo, &resolved) else {
        return out;
    };
    for (index, slot) in resolved.slot_oids.iter().enumerate() {
        let Some(bytes) = slot.as_ref().and_then(|oid| blobs.get(oid)) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<crate::domain::meta::Meta>(bytes) else {
            continue;
        };
        if crate::domain::meta::validate(&meta).is_ok() {
            out[index] = Some(meta);
        }
    }
    out
}

#[cfg(test)]
thread_local! {
    static TRUSTED_PROVENANCE_GIT_PROCESS_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TRUSTED_PROVENANCE_PAYLOAD_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn note_trusted_provenance_git_process() {
    #[cfg(test)]
    TRUSTED_PROVENANCE_GIT_PROCESS_COUNT.with(|count| count.set(count.get() + 1));
}

fn note_trusted_provenance_payload_bytes(_bytes: usize) {
    #[cfg(test)]
    TRUSTED_PROVENANCE_PAYLOAD_BYTES.with(|count| {
        count.set(count.get().saturating_add(_bytes as u64));
    });
}

#[cfg(test)]
fn reset_trusted_provenance_git_process_count() {
    TRUSTED_PROVENANCE_GIT_PROCESS_COUNT.with(|count| count.set(0));
    TRUSTED_PROVENANCE_PAYLOAD_BYTES.with(|count| count.set(0));
}

#[cfg(test)]
fn trusted_provenance_git_process_count() -> usize {
    TRUSTED_PROVENANCE_GIT_PROCESS_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
fn trusted_provenance_payload_bytes() -> u64 {
    TRUSTED_PROVENANCE_PAYLOAD_BYTES.with(std::cell::Cell::get)
}

/// A safe scanning view of a v0 JSONL file or a v1 single-event blob.
///
/// Empty text has no field to mask, so it returns `None` and goes to the ordinary scan. Non-empty
/// text has to be valid LF-terminated envelopes from start to end; accepting only the first few
/// lines would let an attacker append unscanned content after a valid prefix.
fn mask_valid_envelope_stream(
    text: &str,
    trusted_identities: &TrustedEnvelopeIdentities,
) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let mut view = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let envelope = crate::domain::storage::parse_legacy_envelope_line(line).ok()?;
        let session_is_trusted = trusted_identities
            .get(&envelope.session_id)
            .is_some_and(|runtimes| runtimes.contains(&envelope.source));
        append_masked_envelope_identity_fields(&mut view, line, &envelope, session_is_trusted)?;
    }
    Some(view)
}

/// Inside an envelope that already passed semantic validation, replace only the value spans of
/// the two top-level identity fields.
///
/// A v0 envelope allows the legacy field order and insignificant whitespace, so scanning a
/// canonical re-serialization is not an option: that would rewrite `content`'s original
/// representation. This small JSON cursor only finds **top-level** field boundaries; whether the
/// JSON is valid, whether fields are duplicated or extra, and whether the hash matches all remain
/// the job of the serde parser above.
fn append_masked_envelope_identity_fields(
    masked: &mut String,
    line: &str,
    envelope: &crate::domain::transcript::Envelope,
    mask_session_id: bool,
) -> Option<()> {
    const SESSION_SENTINEL: &str = "\"agit-internal-session-id\"";
    const OBJECT_SENTINEL: &str = "\"agit-internal-object-hash\"";

    let json = line.strip_suffix('\n')?;
    let bytes = json.as_bytes();
    let mut i = skip_json_ws(bytes, 0);
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;

    let mut session_span = None;
    let mut object_span = None;
    loop {
        i = skip_json_ws(bytes, i);
        if bytes.get(i) == Some(&b'}') {
            i += 1;
            break;
        }

        let key_start = i;
        let key_end = json_string_end(bytes, key_start)?;
        let key: String = serde_json::from_str(&json[key_start..key_end]).ok()?;
        i = skip_json_ws(bytes, key_end);
        if bytes.get(i) != Some(&b':') {
            return None;
        }
        i = skip_json_ws(bytes, i + 1);
        let value_start = i;
        let value_end = json_value_end(bytes, value_start)?;

        match key.as_str() {
            "_session_id" => {
                let value: String = serde_json::from_str(&json[value_start..value_end]).ok()?;
                if value != envelope.session_id || session_span.is_some() {
                    return None;
                }
                session_span = Some((value_start, value_end, SESSION_SENTINEL));
            }
            "_object_hash" => {
                let value: String = serde_json::from_str(&json[value_start..value_end]).ok()?;
                if value != envelope.object_hash || object_span.is_some() {
                    return None;
                }
                object_span = Some((value_start, value_end, OBJECT_SENTINEL));
            }
            _ => {}
        }

        i = skip_json_ws(bytes, value_end);
        match bytes.get(i) {
            Some(b',') => i += 1,
            Some(b'}') => {
                i += 1;
                break;
            }
            _ => return None,
        }
    }

    if skip_json_ws(bytes, i) != bytes.len() {
        return None;
    }
    let session_span = session_span?;
    let object_span = object_span?;
    if !mask_session_id {
        masked.push_str(&line[..object_span.0]);
        masked.push_str(object_span.2);
        masked.push_str(&line[object_span.1..]);
        return Some(());
    }
    let replacements = if session_span.0 < object_span.0 {
        [session_span, object_span]
    } else {
        [object_span, session_span]
    };
    let mut copied = 0;
    for (start, end, sentinel) in replacements {
        if start < copied || end < start {
            return None;
        }
        masked.push_str(&line[copied..start]);
        masked.push_str(sentinel);
        copied = end;
    }
    masked.push_str(&line[copied..]);
    Some(())
}

fn skip_json_ws(bytes: &[u8], mut i: usize) -> usize {
    while matches!(bytes.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        i += 1;
    }
    i
}

/// Return one past the end of a JSON string token. serde validates escape legality; this only
/// locates the boundary.
fn json_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'\"') {
        return None;
    }
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = i.checked_add(2)?,
            b'\"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Return one past the end of a JSON value token; nested values inside an envelope's `content`
/// are supported.
fn json_value_end(bytes: &[u8], start: usize) -> Option<usize> {
    match *bytes.get(start)? {
        b'\"' => json_string_end(bytes, start),
        b'{' | b'[' => {
            // serde already verified that the brackets pair up; this only finds where the
            // outermost value ends.
            let mut depth = 1usize;
            let mut i = start + 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\"' => i = json_string_end(bytes, i)?,
                    b'{' | b'[' => {
                        depth = depth.checked_add(1)?;
                        i += 1;
                    }
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        i += 1;
                        if depth == 0 {
                            return Some(i);
                        }
                    }
                    _ => i += 1,
                }
            }
            None
        }
        _ => {
            let mut end = start;
            while end < bytes.len() && !matches!(bytes[end], b',' | b'}') {
                end += 1;
            }
            while end > start && matches!(bytes[end - 1], b' ' | b'\t' | b'\r' | b'\n') {
                end -= 1;
            }
            (end > start).then_some(end)
        }
    }
}

/// Allowed on an exact match, or when an allowlist entry is wrapped inside the longer match.
fn is_allowlisted(found: &str, allowlist: &HashSet<String>) -> bool {
    allowlist.contains(found) || allowlist.iter().any(|a| found.contains(a.as_str()))
}

/// The outbound scan surface: whether the content at this path **in the working tree** leaves
/// this machine with a push.
///
/// The design says "outbound jsonl, shared files, commit messages" — that is, **everything except
/// the metadata**. So this is an exclusion list and not an allowlist: an allowlist means every new
/// kind of shared file needs someone to come back and add a line, and the day that is forgotten
/// the gate fails **silently**, which is worse than no gate (it even prints "clean scan").
///
/// `session/meta.json` is excluded: it is fixed-field metadata whose 40-hex session identity
/// matches the generic rules reliably, and it is public by design.
///
/// # Only for the working-tree pass
///
/// This test takes **a real relative path in the working tree** — a file there has exactly one
/// path, path and content correspond one to one, and excluding by path therefore makes sense. The
/// reachable-object pass cannot use it: `rev-list --objects` labels a blob with a path once, and
/// when the same content appears at several paths which label wins depends only on traversal
/// order, so using the label as the test drops the in-surface copy along with it.
///
/// The reachable-object pass therefore **excludes nothing**: every blob is scanned.
pub fn in_publish_surface(rel: &str) -> bool {
    let rel = rel.replace('\\', "/");
    if rel == crate::domain::meta::FILE {
        return false;
    }
    !rel.starts_with(".git/") && rel != ".git"
}

/// Scan everything a repo is about to publish: the working tree plus the reachable objects this
/// push will send.
///
/// # This is the **only** entry point
///
/// The `agit push` gate and `agit scan` call the same function with the same [`ScanPlan`]. With
/// two entry points (one walking the working tree, one not), the two gates reach opposite
/// verdicts on **the most common** shape of all: with a secret that has already been pushed still
/// sitting in a working-tree file, `agit scan --secrets` says clean while `agit push` stops the
/// very same one — and the user cannot reproduce locally why they were refused.
///
/// Two entry points drift as long as they both exist, and the drift is always one of them missing
/// something. So there is one: "scan and push reach the same verdict" is no longer a promise held
/// up by discipline, it is the same code.
///
/// For the surface see [`in_publish_surface`]: the session itself (`session/log.jsonl`,
/// `session/VIEW`), the shared files (`memory/`, `skills/`, `AGENTS.md`, ...) and commit messages
/// not yet pushed.
///
/// The allowlist lives in `$AGIT_HOME` (it must not be published with the repo) rather than in
/// the repo: folding it into one parameter makes the allowlist fail silently — a false positive
/// could not be allowed, and a gate with no way to allow one ends up switched off entirely.
///
/// The four paths (working-tree files, reachable blobs, commit objects, tag objects) share **one**
/// [`HitCollector`], so the combined total is bounded too — capping each path separately and then
/// adding them makes the bound "number of paths × cap", and that multiplier grows quietly as the
/// next path is added.
///
/// # Why both the working tree and the git objects are scanned
///
/// The two answer different questions, and dropping either opens a door:
///
/// * **The working tree** is the only place a secret can be stopped **before** it enters history
///   — content not yet `git add`ed is in no tree.
/// * **The reachable objects** are the only way to guarantee "what is scanned is exactly the
///   bytes about to leave this machine". Working-tree content comes from a checkout, and checkout
///   honours `refs/replace/*`: once a clean replacement tree is materialized by
///   `git reset --hard`, the working tree holds not one byte of the secret while the tree pushed
///   is the real one. And the working tree is only **this one snapshot**, while push sends the
///   whole unpushed history. `rev-list` / `cat-file` go through
///   [`crate::domain::repo::Repo`] with `--no-replace-objects`, so what they read is always the
///   real object.
///
/// The union, not one or the other: either side alone is a gate that can be bypassed, or bypassed
/// ahead of time. One hit from each side is **not** a duplicate: the way out for the working-tree
/// one is an inline annotation, the way out for the blob in history is rewriting history, and
/// collapsing them into one drops one of the two ways out.
///
/// # Why it takes no ref argument
///
/// Because the publish surface is not a function of "the ref the user typed on the command line".
/// One `agit push` pushes `refs_to_push(branches, has_main)` plus the tags they reach — `-b` can
/// be given several times, `--all` is everything with updates, and main is appended
/// unconditionally whenever it exists locally.
///
/// Scanning per ref is wrong in both directions:
///
/// * **Missing**: `agit scan other` checks only `other`'s tree, while `agit push -b other` also
///   carries `main`. With the secret in a current file on `main` and `other` clean, a standalone
///   scan says clean while push stops it — and the user cannot reproduce locally why they were
///   refused.
/// * **Duplicated**: hanging the repository-level paths inside a per-ref loop makes
///   `agit scan main other` scan and report the same hits twice, each stamped with that round's
///   target, pointing at a ref that has nothing to do with them.
///
/// The only thing that varies is the **destination** ([`ScanPlan::dest`]), and that is not a ref:
/// it answers "what does the far side already have", while the `--branches` side does not change
/// by a word.
pub fn scan_agent_repo(
    repo: &crate::domain::repo::Repo,
    plan: &ScanPlan,
) -> crate::Result<ScanReport> {
    let home = crate::infra::config::agit_home()?;
    let allowlist = load_allowlist(&home);
    // When a vault exists but cannot be unlocked or authenticated, return an error; degrading to
    // "there are no registered rules" is not allowed.
    let registered = load_registered_matcher()?;
    #[cfg(feature = "secret-vault")]
    let registered = registered.merged(
        &crate::domain::secret_filter::RepositoryDictionary::open(repo.root())?.active_matcher()?,
    )?;
    // The working tree and the history objects have to use one provenance view. Source events
    // imported by a merge appear both in the current events/** and in a history blob; supplying
    // the identity to the second pass alone still leaves the working-tree pass false-positive.
    let tag_branches = tag_scan_branches(repo)?;
    let branch_identities = branch_trusted_envelope_identities(repo, &tag_branches);
    let worktree_identities = worktree_trusted_envelope_identities(repo, &branch_identities);
    let mut out = HitCollector::new();
    let mut unscanned = Unscanned::default();
    // **How much read cost the working-tree pass has already paid** — both this pass's own brake
    // and the starting point the object pass estimates from.
    //
    // Otherwise `budget_bytes` covers half the scan surface: both passes read the content of one
    // publish, while the budget only counts the second half.
    //
    // What is counted is the size metadata reports, not the length `read_to_string` returns: the
    // test has to be available **before** the read, or the budget only has effect after the fact
    // (see the bookkeeping paragraph below). The cost is that a non-UTF-8 file counts at its size
    // on disk — which is exactly the memory it really takes.
    let mut spent: u64 = 0;
    for entry in walkdir::WalkDir::new(repo.root())
        .into_iter()
        .filter_entry(|e| {
            e.path()
                .strip_prefix(repo.root())
                .ok()
                .map(|r| r.as_os_str().is_empty() || in_publish_surface(&r.to_string_lossy()))
                .unwrap_or(false)
        })
        .filter_map(std::result::Result::ok)
    {
        // Once full, **do not even read**: in a file that matches on every line the cost is
        // `read_to_string` + `scan_text`, so skipping only the push saves nothing.
        if out.is_full() {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(repo.root()) else {
            continue;
        };
        let rel = rel.to_string_lossy().to_string();
        // **Ask how big first, then decide whether to read** — the same bound as the object
        // path.
        //
        // With no test on file size in front of `read_to_string`, an uncommitted 300 MiB
        // `artifact.log` in the working tree drives maxRSS to 682 MB (observed), and this pass
        // runs **before** the object scan, so the "read no object at all when over budget" fast
        // path cannot save it.
        //
        // Metadata that cannot be obtained counts as "cannot be read", the same as the read
        // failure below.
        let Ok(size) = entry.metadata().map(|m| m.len()) else {
            continue;
        };
        if size > plan.limits.max_object_bytes {
            // Booked in **the working tree's own ledger**: the handle here is a path, not an
            // oid (see [`Unscanned`]). A file over the line is not read at all, so not one of
            // its bytes belongs in the cumulative budget — it is booked separately in
            // `oversized_files` and must not eat the total (the same rule by which
            // [`estimate_object_bytes`] skips objects over the line).
            unscanned.oversized_files.push((rel, size));
            continue;
        }
        // **Book it first, then read**, and with the size metadata reports rather than the
        // length that comes back.
        //
        // The other order (`read_to_string` first, `spent += size` after) leaves the budget with
        // effect only after the fact: the per-file cap stops nothing when a pile of files are
        // **each within it**, and the cumulative budget only reaches a verdict once the whole
        // working tree has been walked and the object pass begins — so enough files can read
        // bytes far past the budget entirely into memory and only then be told "over budget".
        // The promise "over the budget means no work" would not be kept by a word.
        spent = spent.saturating_add(size);
        if spent > plan.limits.budget_bytes {
            // Stop where it stands: the remaining working-tree files are not read, and the
            // object pass is not even asked (see the guard below). What is recorded is `spent`
            // **as it is now** — a lower bound, for the reason in [`Unscanned::over_budget`].
            unscanned.over_budget = Some((spent, plan.limits.budget_bytes));
            break;
        }
        // One file failing to read (binary, permissions) does not interrupt the whole scan.
        //
        // But it **has already been booked**: the cost of `read_to_string` was paid at the moment
        // it failed (the whole file went through memory), and booking it after this `continue`
        // would let arbitrarily many bytes that are not valid UTF-8 through the budget for free
        // — and binary files are the easiest thing in a working tree to pile up.
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        // The bound is **how much the collector can still take**: a large file matching on every
        // line materializes `Hit`s on the order of its line count before `extend` takes over, and
        // capping the final list does not stop that stretch. See [`scan_text_capped`].
        let cap = out.remaining();
        let scanned = scan_repository_payload_capped(
            &text,
            &allowlist,
            &worktree_identities,
            Policy::CLIENT,
            cap,
            &registered,
        );
        // This file's own budget ran out: it holds hits that were never scanned. Completeness is
        // stated by the side that knows.
        if scanned.truncated {
            out.mark_truncated();
        }
        out.extend(scanned.hits.into_iter().map(|mut h| {
            h.file = Some(rel.clone());
            h
        }));
    }
    // The working-tree pass already spent the budget: the object pass does not even
    // **estimate**. Its first act would be to ask git for the object list starting from the same
    // `spent`, and the answer is already settled (over), so that `rev-list` + `for-each-ref` is
    // pure waste; it would also overwrite the lower bound recorded above with a number that is
    // equally over the line but harder to read.
    if unscanned.over_budget.is_none() {
        let context = PublishScanContext {
            repo,
            plan,
            allowlist: &allowlist,
            registered: &registered,
            tag_branches: &tag_branches,
            trusted_identities: &branch_identities,
        };
        scan_publish_objects(&context, spent, &mut out, &mut unscanned)?;
    }
    Ok(ScanReport {
        truncated: out.was_truncated(),
        hits: out.into_hits(),
        unscanned,
    })
}

/// The whole block of the scan surface that **does not vary with the working tree**: reachable
/// blobs, commit objects, tag objects.
///
/// All three selectors are decided only by "what this push will send", independent of which
/// snapshot is currently checked out and of which ref was typed on the command line. So one
/// **single function** issues them, and the rev range comes from [`Destination::revs`] alone — a
/// mismatch where blobs are scanned over the unpushed range while commits are scanned over the
/// full history reports hits the user has no way to act on, and the other half of a mismatch is
/// sooner or later a miss.
///
/// # Compute the work first, then decide whether to do it
///
/// The estimate goes through `cat-file --batch-check`: **headers only**, no body inflation, which
/// makes it an order of magnitude or two cheaper than the real scan. Over budget means no object
/// is read at all, and that fact is handed out in [`Unscanned::over_budget`].
///
/// The order is deliberate — "you are over budget" after grinding through 21 GiB and the same
/// sentence up front are two different things to the user, and they carry exactly the same
/// information.
///
/// `spent` is the byte count the working-tree pass already read, used as the estimate's
/// **starting point**: both passes read the content of one publish, so the budget has to count
/// both halves.
struct PublishScanContext<'a> {
    repo: &'a crate::domain::repo::Repo,
    plan: &'a ScanPlan,
    allowlist: &'a HashSet<String>,
    registered: &'a RegisteredMatcher,
    tag_branches: &'a [String],
    trusted_identities: &'a TrustedEnvelopeIdentities,
}

fn scan_publish_objects(
    context: &PublishScanContext<'_>,
    spent: u64,
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    let PublishScanContext {
        repo,
        plan,
        allowlist,
        registered,
        tag_branches,
        trusted_identities,
    } = *context;
    let revs = plan.dest.revs();
    let sel: Vec<&str> = revs.iter().map(String::as_str).collect();

    let estimate = estimate_object_bytes(repo, &sel, tag_branches, &plan.limits, spent)?;
    if estimate > plan.limits.budget_bytes {
        // **Do not scan**, and say so. Returning Ok silently is the failure this gate must not
        // have: a history that was never read reported as clean.
        unscanned.over_budget = Some((estimate, plan.limits.budget_bytes));
        return Ok(());
    }

    let blobs = BlobScanContext {
        repo,
        limits: &plan.limits,
        allowlist,
        registered,
        trusted_identities,
    };
    scan_publish_blobs(&blobs, &sel, out, unscanned)?;
    scan_commit_messages(
        repo,
        &sel,
        &plan.limits,
        allowlist,
        registered,
        out,
        unscanned,
    )?;
    scan_tag_objects(
        repo,
        &plan.limits,
        allowlist,
        registered,
        tag_branches,
        out,
        unscanned,
    )?;
    Ok(())
}

/// How many bytes this scan will read — estimated from **headers only**.
///
/// What is counted is what the three body-reading paths really read: blobs and **non-root trees**
/// ([`scan_publish_blobs`]), commits ([`scan_commit_messages`]) and annotated tags
/// ([`scan_tag_objects`]). A single object over the line is not read at all (see
/// [`ScanLimits::max_object_bytes`]) and is not counted either — those are booked separately in
/// [`Unscanned::oversized`] and must not eat the total budget.
///
/// # Why trees have to be counted
///
/// The enumeration step of [`scan_publish_blobs`] **cannot tell trees apart**: `rev-list
/// --objects` prints a subdirectory as `<oid> <directory name>`, carrying a path just like a
/// blob, so it enters the batch like one and `cat-file --batch` is asked for its body — git
/// inflates it, fills the pipe, this process reads it to length, and **only then** is it dropped
/// on that one `kind != "blob"` line. The bytes already flowed.
///
/// So "trees are never counted" would not describe what this code does: a repo with small blobs
/// and large trees (a directory of a few million entries is enough) estimates to those few blob
/// bytes, comfortably inside the budget, while what actually flows through `cat-file` adds the
/// whole pile of tree bytes and can be an order of magnitude larger — "read no object at all when
/// over budget" would not be kept by a word, and the duration goes back to being a function of
/// repo size. And this undercount does not need an adversarial construction to meet: v1's
/// `events/a/b/c/d/<id>` rewrites a five-level tree chain on every commit, so tree bytes grow on
/// the same order as the blob bytes this bound was calibrated against.
///
/// The **root** tree is not counted, the other way round: its enumeration line has an empty path
/// and [`scan_publish_blobs`] skips it itself, so counting it is dishonesty in the other
/// direction (bytes nobody reads pushed into the budget, large enough to keep a good repo out).
/// The test comes straight from the enumeration line's path, sharing a source with that side —
/// written once on each side they drift apart sooner or later.
///
/// # Tags need **a second enumeration**
///
/// Adding `"tag"` to the `kind` test alone says nothing: the selector is `--branches` (see
/// [`Destination::revs`]) and an annotated tag object is **unreachable** from a branch —
/// `rev-list --objects --branches` lists no tag at all. So this enumerates again under the same
/// test [`scan_tag_objects`] uses (`for-each-ref --merged <branches…> refs/tags`).
///
/// Only the ones with `objecttype == tag` are counted: the commit a lightweight tag points at was
/// already counted by the previous pass under `--merged`, and counting it again is double
/// billing.
///
/// `spent` is the byte count the working-tree pass already read, used as the **starting point**:
/// the budget covers the whole scan surface, not half of it.
///
/// Reaching the budget **stops the enumeration** (`--batch-check` is not free either), so the
/// return value is a lower bound once the limit is crossed. The verdict only cares whether it
/// went over, for which a lower bound is enough; when the number is printed it is said as "at
/// least".
///
/// A failed estimate is **not** "there are no objects": like every other git call in this module,
/// it errors instead of allowing silently.
fn estimate_object_bytes(
    repo: &crate::domain::repo::Repo,
    sel: &[&str],
    tag_branches: &[String],
    limits: &ScanLimits,
    spent: u64,
) -> crate::Result<u64> {
    let mut args = vec!["rev-list", "--objects"];
    args.extend_from_slice(sel);

    let mut total: u64 = spent;
    // Each entry is `(oid, did the enumeration line carry a non-empty path)`. The second half is
    // the test [`scan_publish_blobs`] uses to decide whether to read a body, so it travels with
    // the oid all the way to where the type can be asked.
    let mut batch: Vec<(String, bool)> = Vec::with_capacity(OBJECT_BATCH);
    let mut over = false;
    let weigh = |repo: &crate::domain::repo::Repo,
                 batch: &mut Vec<(String, bool)>,
                 total: &mut u64|
     -> crate::Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let (oids, has_path): (Vec<String>, Vec<bool>) = std::mem::take(batch).into_iter().unzip();
        // `--batch-check` answers one line per input in input order (see
        // [`crate::domain::repo::Repo::git_cat_file_batch_check`]), so the index is "which
        // enumeration record this answer belongs to".
        let mut at = 0usize;
        repo.git_cat_file_batch_check(oids, |_, kind, size| {
            let payload = has_path.get(at).copied().unwrap_or(false);
            at += 1;
            // The ones whose bodies get read: commits ([`scan_messages`]), tags
            // ([`scan_tag_objects`]), and the ones that carried a path during enumeration —
            // blobs and non-root trees, for which [`scan_publish_blobs`] asks for the body
            // alike. Anything over the line is not read at all.
            if (kind == "commit" || kind == "tag" || payload) && size <= limits.max_object_bytes {
                *total = total.saturating_add(size);
            }
            Ok(())
        })
    };

    let read = repo.git_stream_split(&args, b'\n', |rec| {
        if total > limits.budget_bytes {
            over = true;
            return Err(anyhow::Error::new(BudgetSpent));
        }
        let line = String::from_utf8_lossy(rec);
        // Each line is `<oid>` or `<oid> SP <path>`. Split the same way as
        // [`scan_publish_blobs`]: a commit has no path part and the root tree's path is empty,
        // and it reads neither.
        let (oid, path) = match line.split_once(' ') {
            Some((oid, path)) => (oid.trim(), path),
            None => (line.trim(), ""),
        };
        if oid.is_empty() {
            return Ok(());
        }
        batch.push((oid.to_string(), !path.is_empty()));
        if batch.len() >= OBJECT_BATCH {
            weigh(repo, &mut batch, &mut total)?;
        }
        Ok(())
    });
    match read {
        Ok(()) => {}
        Err(e) if e.downcast_ref::<BudgetSpent>().is_some() => return Ok(total),
        Err(e) => {
            return Err(e.context(
                "git cannot list the objects to publish, so they cannot be confirmed clean",
            ));
        }
    }
    if over {
        return Ok(total);
    }
    weigh(repo, &mut batch, &mut total)?;

    // The tag side: unreachable from a branch, so the pass above cannot see it.
    let read = stream_tag_object_oids(repo, tag_branches, |oid| {
        if total > limits.budget_bytes {
            over = true;
            return Err(anyhow::Error::new(BudgetSpent));
        }
        // A tag object goes through [`scan_tag_objects`] and never through the carries-a-path
        // test — it is counted by `kind == "tag"`.
        batch.push((oid.to_string(), false));
        if batch.len() >= OBJECT_BATCH {
            weigh(repo, &mut batch, &mut total)?;
        }
        Ok(())
    });
    match read {
        Ok(()) => {}
        Err(e) if e.downcast_ref::<BudgetSpent>().is_some() => return Ok(total),
        Err(e) => return Err(e),
    }
    if !over {
        weigh(repo, &mut batch, &mut total)?;
    }
    Ok(total)
}

/// How many OIDs one `cat-file --batch` is fed.
///
/// Its only reason to exist is **keeping the whole OID list out of memory**: `rev-list --objects`
/// lists every historical object on the first publish of a deep-history repo, and a `Vec<String>`
/// holding that is an allocation on the order of the history, while scanning itself only ever
/// looks at one object at a time. A batch is handed to git once it fills and cleared once read,
/// so the peak is `OBJECT_BATCH` rather than "how many objects the repo has".
///
/// 1024 is chosen between two costs: the smaller the batch, the more `cat-file` processes are
/// started (a few milliseconds each); the larger the batch, the more OIDs are resident. 1024 OIDs
/// are about 50 KB, and even a repo with a million objects starts only about a thousand
/// processes.
const OBJECT_BATCH: usize = 1024;

/// Used to **abort** the streaming read of `rev-list` once the collector is full.
///
/// [`crate::domain::repo::Repo::git_stream_split`] only stops and kills the child process when
/// the callback returns `Err`. And "I stopped because I am full" is not a failure, so it has to
/// be separable from a real git fault: conflated, it either reports a normal early exit as a scan
/// failure (calling a good repo broken) or swallows the fault along with it to avoid that
/// (calling a fault clean) — and the latter is the failure this gate must not have.
#[derive(Debug)]
struct EnoughHits;

impl std::fmt::Display for EnoughHits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("enough hits collected, no need to enumerate more objects")
    }
}

impl std::error::Error for EnoughHits {}

/// The estimate reached the budget, used to **abort** the enumeration. It is separate from
/// [`EnoughHits`] because the two say different things: one is "the verdict is already settled",
/// the other is "this history is beyond what the local machine can scan" — and the second has to
/// travel all the way to the user.
#[derive(Debug)]
struct BudgetSpent;

impl std::fmt::Display for BudgetSpent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "the estimate is already over the scan budget, no need to enumerate more objects",
        )
    }
}

impl std::error::Error for BudgetSpent {}

/// Scan **every reachable blob this push may send**, reading git objects rather than the working
/// tree.
///
/// # Why not "the tree at each branch tip"
///
/// A tip's tree is only the **last frame** of the history, while push sends a set of objects. The
/// first commit writes the secret into `payload.txt` and the second deletes the file, so
/// `ls-tree -r main` is empty — while that blob sits in the first push's reachable objects with
/// every byte intact (`rev-list --objects main` still lists it). A scan that looks only at tips
/// reports clean on such a history every time.
///
/// The same hole has a second cross-section: with the scan surface branching on "the ref being
/// scanned", `agit scan other` checks only `other` while `agit push -b other` has the publish
/// plan carry `main` as well. So this pass is **repository-level** and takes no ref argument — no
/// argument, no branching.
///
/// # The scan surface
///
/// ```text
/// destination asked:     rev-list --objects --branches --not <tips the destination reported>…
/// destination not asked: rev-list --objects --branches
/// ```
///
/// `--objects` lists the reachable commits / trees / blobs (the blob line also carries a path).
/// `--branches` covers every local branch — sharing a source with [`tag_scan_branches`]:
/// `agit push --all` can push any of them, and `push::refs_to_push` appends main unconditionally
/// whenever it exists locally, so the scan surface is a superset of the publish surface.
///
/// The subtracted half comes from [`Destination`] — **what the destination itself reported**, not
/// the local remote-tracking refs. Tracking refs describe the remote of the last fetch/push; they
/// do not change when the hub is switched or the remote is deleted and recreated, and in that
/// state this push sends the whole history (see [`Destination`] for the observed case). The
/// selector comes from [`Destination::revs`] alone, and the commit path uses the same one (a test
/// written once on each path produces, inside one push, the mismatch "commits scanned over the
/// unpushed range, blobs scanned over the full history").
///
/// Every object goes through [`crate::domain::repo::Repo`] with `--no-replace-objects`, so
/// `refs/replace/*` fools neither the enumeration nor the read.
///
/// # This pass filters by path not at all
///
/// The path `rev-list --objects` puts on a blob line is only a **label**: a blob gets one, and
/// when the same content appears at several paths which one is kept depends purely on traversal
/// order. Using it as a filter therefore makes a secret lying at both `session/meta.json` and
/// `skills/leak.md` disappear **entirely** because the label landed on the former — the in-surface
/// copy never gets its turn to be scanned, and it leaves with the push all the same. A gate's
/// verdict must not depend on the lexicographic order of file names.
///
/// So **neither** of [`in_publish_surface`]'s two exclusions is kept here:
///
/// * **`session/meta.json`** — its exclusion is **defensive and load-bearing for nothing**: a
///   canonical meta matches `[]` under the current rule set (observed; fixed fields plus a 40-hex
///   identity trip no rule).
///
///   Recognizing it by **content** instead (bytes compared to the canonical serialization) widens
///   an exclusion that belonged to one path into a **global** one: any blob whose bytes look like
///   a canonical meta is skipped, and those bytes can be constructed on purpose — put the secret
///   in `milestone`, save it as `skills/deploy/config.json`, and the scan says clean while push
///   sends it. A test that solves nothing and can be worn as a costume is only harmful.
///
///   Should a future entropy-based generic rule make session identities false-positive, what
///   changes then is the rule or the allowlist, not a hole in the scan surface keyed on the shape
///   of content.
///
/// * **`.git/*`** — it guards the working-tree walkdir against descending into a real `.git`
///   directory (local state: config, hooks, packed objects), and **there is no local state among
///   the reachable objects**: a tree entry named `.git` is not something ordinary git produces,
///   while one forced in with plumbing really is bytes that leave with a push. Excluding it here
///   opens a path an attacker can aim at.
///
/// # How the cost is contained
///
/// `rev-list --objects` lists every historical object on a large repo's first push, so every
/// layer has to be bounded:
///
/// 1. **Streaming enumeration**: through `git_stream_split`, holding one record at a time, so the
///    output never enters memory in one piece.
/// 2. **A bounded OID list**: once [`OBJECT_BATCH`] is full it goes to one `cat-file --batch` and
///    is cleared, so what is resident is independent of history length.
/// 3. **Stop when full**: as soon as the collector fills, [`EnoughHits`] aborts `rev-list` (the
///    child process is killed) rather than "read it all and throw it away".
/// 4. **A bounded output per carrier**: each blob's `cap` is `out.remaining()`, so even a large
///    file matching on every line does not materialize `Hit`s on the order of its line count
///    before the collector takes over (see [`scan_text_capped`]).
/// 5. **A bounded single object**: an object over [`ScanLimits::max_object_bytes`] is not read at
///    all and is recorded in [`Unscanned::oversized`] (see
///    [`crate::domain::repo::ObjectBody`]).
/// 6. **Bounded total work**: the whole batch's byte count is estimated first by
///    [`estimate_object_bytes`], and over budget means no object is read.
///
/// None of the first four counts **work**: 1 and 2 only keep the input out of memory in one
/// piece, 4 counts `Hit`s, and 3 **never fires** on a clean repo — the only early exit is dead on
/// the most common path. 5 and 6 are what fill that hole.
///
/// `rev-list --objects` prints each object once (it deduplicates by OID itself), so no full
/// "have I seen this" table is needed here — that table would itself be an allocation on the
/// order of the history.
fn scan_publish_blobs(
    context: &BlobScanContext<'_>,
    sel: &[&str],
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    let repo = context.repo;
    // Already full: no object needs listing. The verdict was settled long ago (non-empty is a
    // refusal).
    if out.is_full() {
        return Ok(());
    }
    let mut args = vec!["rev-list", "--objects"];
    args.extend_from_slice(sel);
    let sel = &args[..];

    let mut batch: Vec<String> = Vec::with_capacity(OBJECT_BATCH);
    // OID → the path shown in the report, covering **the current batch only** and cleared with
    // it.
    let mut label: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    let read = repo.git_stream_split(sel, b'\n', |rec| {
        if out.is_full() {
            return Err(anyhow::Error::new(EnoughHits));
        }
        // Each line is `<oid>` or `<oid> SP <path>`. A commit has no path part and the root
        // tree's path is empty — neither is a payload to read here (commit bodies belong to
        // `scan_commit_messages`).
        //
        // **No trim**: a path may end in a space, and trimming it is no longer the real path. The
        // line itself was already split on `\n`, so there are no stray bytes at the end.
        let line = String::from_utf8_lossy(rec);
        let Some((oid, path)) = line.split_once(' ') else {
            return Ok(());
        };
        if oid.is_empty() || path.is_empty() {
            return Ok(());
        }
        // The path is only a **label**: `rev-list --objects` prints one per object (keeping the
        // first path seen when several references share the same content) and prints it up to the
        // newline. It goes into the report and takes part in no filtering — the type comes from
        // the `kind` `cat-file` reports below (tree entries are skipped there), and this pass
        // excludes nothing by content or by path: every blob is scanned.
        if label.insert(oid.to_string(), path.to_string()).is_some() {
            return Ok(());
        }
        batch.push(oid.to_string());
        if batch.len() >= OBJECT_BATCH {
            scan_blob_batch(context, &mut batch, &mut label, out, unscanned)?;
        }
        Ok(())
    });
    match read {
        Ok(()) => {}
        // Stopped deliberately because it is full: the verdict was settled, this is not a failure.
        Err(e) if e.downcast_ref::<EnoughHits>().is_some() => return Ok(()),
        // Failing to list is **not** "there is nothing to publish". A scanner's worst failure is
        // calling a fault clean.
        Err(e) => {
            return Err(e.context(
                "git cannot list the objects to publish, so they cannot be confirmed clean",
            ));
        }
    }
    scan_blob_batch(context, &mut batch, &mut label, out, unscanned)
}

/// The read-only context shared by one batch of blob scans: the git read bounds, both rule sets,
/// and the provenance identity view.
struct BlobScanContext<'a> {
    repo: &'a crate::domain::repo::Repo,
    limits: &'a ScanLimits,
    allowlist: &'a HashSet<String>,
    registered: &'a RegisteredMatcher,
    trusted_identities: &'a TrustedEnvelopeIdentities,
}

/// Read a batch of objects and scan the blobs among them. `batch` and `label` are cleared once
/// read.
fn scan_blob_batch(
    context: &BlobScanContext<'_>,
    batch: &mut Vec<String>,
    label: &mut std::collections::HashMap<String, String>,
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    if batch.is_empty() {
        label.clear();
        return Ok(());
    }
    let oids = std::mem::take(batch);
    let cap_bytes = usize::try_from(context.limits.max_object_bytes).unwrap_or(usize::MAX);
    let read = context
        .repo
        .git_cat_file_batch(oids, cap_bytes, |oid, kind, body| {
            // Once full, **do not even scan**: what is skipped is `scan_text_capped`, not just
            // one push.
            if out.is_full() {
                return Ok(());
            }
            let payload = match body {
                crate::domain::repo::ObjectBody::Read(b) => b,
                // Over the line: not one byte was read. **Record it** — unrecorded it is
                // effectively removed from the scan surface, and the report calls something it
                // never looked at clean.
                crate::domain::repo::ObjectBody::TooLarge(n) => {
                    if kind == "blob" || kind == "commit" {
                        unscanned
                            .oversized
                            .push((oid[..oid.len().min(8)].to_string(), n as u64));
                    }
                    return Ok(());
                }
            };
            // A tree carries a path too (the directory name), so the enumeration step cannot
            // tell it apart — the authoritative answer for the type is only here. This line
            // **drops a body that has already been read**: git inflated it, the pipe was filled,
            // and it was read to length above. Those bytes are work, so
            // [`estimate_object_bytes`] has to count them into the budget (sharing the test:
            // the ones whose enumeration line carried a non-empty path).
            if kind != "blob" {
                return Ok(());
            }
            // Non-UTF-8 (binary) is skipped, the same test as `read_to_string` on the
            // working-tree path.
            let Ok(text) = std::str::from_utf8(payload) else {
                return Ok(());
            };
            // The bound is **how much the collector can still take**, for the same reason as the
            // working-tree path.
            let cap = out.remaining();
            let scanned = scan_repository_payload_capped(
                text,
                context.allowlist,
                context.trusted_identities,
                Policy::CLIENT,
                cap,
                context.registered,
            );
            // This blob's own budget ran out: it holds hits that were never scanned.
            if scanned.truncated {
                out.mark_truncated();
            }
            // The label has the same shape as the commit and tag paths: `<kind> object <sha8>`,
            // followed by the path this blob was referenced at. Both parts are needed — the sha8
            // is the user's only handle for `git cat-file blob` / `git log --all --find-object=`,
            // and only the path says what leaked.
            let file = label
                .get(oid)
                .map(|p| format!("blob object {}/{p}", &oid[..oid.len().min(8)]));
            out.extend(scanned.hits.into_iter().map(|mut h| {
                h.file.clone_from(&file);
                h.source = Source::BlobObject;
                h
            }));
            Ok(())
        });
    label.clear();
    read.map_err(|e| {
        anyhow::anyhow!(
            "git cannot read the content of the objects to publish, so they cannot be confirmed clean: {e}"
        )
    })
}

/// Is this repo readable — **that one question only**.
///
/// # Why it no longer answers "is HEAD born"
///
/// Both scan paths select with `--branches` / `--merged <branch>` — **neither touches HEAD**. So
/// "is HEAD born" corresponds to no real precondition, and using it as a "no history, return
/// empty" fast path does exactly one thing: after `git checkout --orphan` the scan silently
/// returns empty (= clean) while push's publish surface, unaffected by HEAD, pushes anyway.
///
/// So this function keeps only the half that still holds: **"unreadable is not clean"**. Nothing
/// returns a boolean, so there is no fast path for some layer to forget to delete.
///
/// # The test
///
/// A failing `rev-parse --verify HEAD` alone **cannot separate two things**: a corrupt HEAD,
/// wrong permissions on `.git`, and not being in a repo at all all make it fail, so a fault gets
/// treated as an empty history.
///
/// Hence two signals: `symbolic-ref` **succeeding** (HEAD really points at a branch) while
/// `rev-parse` **fails** (that branch has no commit yet) = an unborn branch on a healthy repo.
/// All four ways of being broken (corrupt HEAD / not a repo / unreadable objects / no `.git`)
/// fail at the `symbolic-ref` step (observed).
fn assert_repo_readable(repo: &crate::domain::repo::Repo) -> crate::Result<()> {
    if repo
        .git_opt(&["rev-parse", "--verify", "-q", "HEAD"])
        .is_some()
    {
        return Ok(());
    }
    if repo.git_opt(&["symbolic-ref", "-q", "HEAD"]).is_some() {
        return Ok(()); // An unborn branch: the repo is fine, this branch just has no commit.
    }
    anyhow::bail!("git cannot read this repo's HEAD, so the history cannot be confirmed clean")
}

/// The **entire body** of an annotated tag is in the scan surface too.
///
/// # Why this has to exist
///
/// A tag object is a git object of its own, carries its own message, and leaves with every clone
/// / fetch — `git show v1` reads it. The commit path (`rev-list`) cannot see it: when the commit
/// a new tag points at has already been pushed, not a byte of the commit graph changed.
///
/// Without this, the server-side gate refuses (it scans tags) while the local `agit scan` says
/// clean — and the user cannot reproduce locally why they were refused. The direction is safe
/// (the server is stricter), but "scanned, and clean" is then false, and that is the one lie a
/// scanner must never tell.
///
/// # Which tags are scanned
///
/// The same test as push: **the ones reachable from the branches being pushed**
/// (`git tag --merged <branch>`, see `commands::push::tags_to_push`). Not all of `--tags` —
/// version numbers on branches that were not selected are not pushed anyway, and scanning them
/// only reports hits the user can neither understand nor act on.
///
/// The scan surface and the publish surface sharing a source is the one thing to hold here: a
/// test written once on each side drifts apart sooner or later, and the drift is always a miss.
///
/// # Why the bodies no longer come all at once from `for-each-ref %(raw)`
///
/// As `git_bytes(for-each-ref --format=…%(raw))`, **the bodies of every reachable tag** land in
/// one `Vec<u8>` at once and each is then copied into a `String` by `from_utf8_lossy`. The two
/// problems are one:
///
/// * **[`ScanLimits::max_object_bytes`] does not apply** — the call site is not even passed
///   `limits`. The same bytes in a different carrier flip the verdict (observed): a 200 MiB
///   annotated tag goes through the whole pass at maxRSS 610 MiB, reports `clean scan` and is
///   booked nowhere, while a 66 MiB blob is refused and recorded in [`Unscanned::oversized`].
/// * **Reading it all at once is itself another unbounded allocation.** And large tag counts are
///   **normal** in this product: agit cuts a version tag every turn, so hundreds of turns are
///   hundreds of tags. "Each one bounded" does not fix "all of them read at once".
///
/// So it is exactly the same shape as blobs and commits: streaming enumeration
/// ([`stream_tag_object_oids`], whose output is only oids and types, independent of body size),
/// and OIDs handed to one [`crate::domain::repo::Repo::git_cat_file_batch`] once
/// [`OBJECT_BATCH`] fills, then cleared. The peak is therefore "the largest objects within one
/// batch", not "how large all the tags are together".
///
/// The framing precondition also returns to the sturdiest one: the enumeration step outputs only
/// hex and type names, so splitting on `\n` is unambiguous; bodies are taken by the **byte
/// count** git reports itself, so a NUL or non-UTF-8 inside a body changes nothing (framing
/// `%(raw)` by hand on `%(raw:size)` existed precisely to deal with those two).
fn scan_tag_objects(
    repo: &crate::domain::repo::Repo,
    limits: &ScanLimits,
    allowlist: &HashSet<String>,
    registered: &RegisteredMatcher,
    branches: &[String],
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    // Already full: no tag needs reading. The verdict was settled long ago (non-empty is a
    // refusal), and reading on only stuffs a list nobody prints.
    if out.is_full() {
        return Ok(());
    }

    let mut batch: Vec<String> = Vec::with_capacity(OBJECT_BATCH);
    let read = stream_tag_object_oids(repo, branches, |oid| {
        // Stop when full: what is skipped is the body of every tag behind this one, not just one
        // push.
        if out.is_full() {
            return Err(anyhow::Error::new(EnoughHits));
        }
        batch.push(oid.to_string());
        if batch.len() >= OBJECT_BATCH {
            scan_tag_batch(
                repo, limits, allowlist, registered, &mut batch, out, unscanned,
            )?;
        }
        Ok(())
    });
    match read {
        Ok(()) => {}
        // Stopped deliberately because it is full: the verdict was settled, this is not a failure.
        Err(e) if e.downcast_ref::<EnoughHits>().is_some() => return Ok(()),
        Err(e) => return Err(e),
    }
    scan_tag_batch(
        repo, limits, allowlist, registered, &mut batch, out, unscanned,
    )
}

/// Read a batch of tag objects and scan their bodies. `batch` is cleared once read.
fn scan_tag_batch(
    repo: &crate::domain::repo::Repo,
    limits: &ScanLimits,
    allowlist: &HashSet<String>,
    registered: &RegisteredMatcher,
    batch: &mut Vec<String>,
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let oids = std::mem::take(batch);
    let cap_bytes = usize::try_from(limits.max_object_bytes).unwrap_or(usize::MAX);
    repo.git_cat_file_batch(oids, cap_bytes, |oid, kind, body| {
        // Once full, **do not even scan**: what is skipped is `scan_text_capped`, not just one
        // push.
        if out.is_full() {
            return Ok(());
        }
        let payload = match body {
            crate::domain::repo::ObjectBody::Read(b) => b,
            // Over the line: not one byte was read. **Record it** — unrecorded it is effectively
            // removed from the scan surface, and the report calls something it never looked at
            // clean. Locating and fixing both go by oid (`git cat-file tag <sha>` for the
            // content, rebuild the tag to replace it), so it is booked in the same ledger as
            // blobs and commits.
            crate::domain::repo::ObjectBody::TooLarge(n) => {
                if kind == "tag" {
                    unscanned
                        .oversized
                        .push((oid[..oid.len().min(8)].to_string(), n as u64));
                }
                return Ok(());
            }
        };
        // A lightweight tag points straight at a commit and has no tag object of its own — the
        // commit path already scanned it. The enumeration step filtered on `%(objecttype)`
        // once; this is the authoritative second one.
        if kind != "tag" {
            return Ok(());
        }
        // The conversion applies to this one body — the scanning engine wants a `&str`, and the
        // rule set is insensitive to U+FFFD (it appears in no credential shape). This uses lossy
        // rather than skipping non-UTF-8 the way the blob path does: the first lines of a tag
        // body (tagger, tag name) are always text, and a message holding binary must not remove
        // the whole tag from the scan surface.
        let text = String::from_utf8_lossy(payload);
        // The bound is **how much the collector can still take**, for the same reason as the
        // working-tree path: a single dense carrier's peak has to be bounded too.
        let cap = out.remaining();
        let scanned =
            scan_text_capped_registered(&text, allowlist, Policy::CLIENT, cap, registered);
        // This tag body's own budget ran out: it holds hits that were never scanned.
        if scanned.truncated {
            out.mark_truncated();
        }
        let file = format!("tag object {}", &oid[..oid.len().min(8)]);
        out.extend(scanned.hits.into_iter().map(|mut h| {
            // The label has the same shape as the commit path: `<kind> object <sha8>`, with the
            // line number counted in the output of `git cat-file tag <sha>` — the position the
            // report gives has to be findable by following it.
            h.file = Some(file.clone());
            h.source = Source::TagObject;
            h
        }));
        Ok(())
    })
    .map_err(|e| {
        anyhow::anyhow!(
            "git cannot read the tag bodies to publish, so they cannot be confirmed clean: {e}"
        )
    })
}

/// List, **as a stream**, the OIDs of the annotated tag objects reachable from these branches.
///
/// The scan ([`scan_tag_objects`]) and the estimate ([`estimate_object_bytes`]) use **one**
/// enumeration: with a `--merged` test written once on each side, the surface that was estimated
/// and the surface that is really scanned drift apart sooner or later, and one half of the drift
/// is always "the budget never counted it, so it never contained it".
///
/// The format contains **no** `%(raw)`: the output volume therefore follows the **number** of
/// tags and not the size of their bodies, and the bodies go to
/// [`crate::domain::repo::Repo::git_cat_file_batch`] in batches. All three fields (40-hex, type
/// name, empty) are ASCII, so splitting on `\n` is unambiguous — the bodies are not here, so
/// "a body may contain any byte" is not a problem here either.
///
/// `--merged` may be given several times and is a union — so the scan surface can cover the
/// publish surface exactly, with no need to fall back on the over-wide approximation "scan every
/// tag".
///
/// This **does not look at HEAD**: `branches` comes from `for-each-ref refs/heads` and is by
/// construction all born branches, and `--merged` never touches HEAD. After
/// `git checkout --orphan` HEAD is unborn, while push's publish surface, unaffected by HEAD,
/// pushes these tags all the same. With no branch there is no reachable tag, and this one test is
/// already enough.
fn stream_tag_object_oids(
    repo: &crate::domain::repo::Repo,
    branches: &[String],
    mut on_oid: impl FnMut(&str) -> crate::Result<()>,
) -> crate::Result<()> {
    if branches.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec!["for-each-ref".into()];
    for b in branches {
        args.push("--merged".into());
        args.push(b.clone());
    }
    args.push("--format=%(objectname) %(objecttype)".into());
    args.push("refs/tags".into());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    repo.git_stream_split(&argv, b'\n', |rec| {
        let line = String::from_utf8_lossy(rec);
        let mut f = line.split_whitespace();
        let (Some(oid), Some(kind)) = (f.next(), f.next()) else {
            return Ok(());
        };
        // The commit a lightweight tag points at was already scanned on the commit path and
        // already counted by the estimate.
        if kind != "tag" || oid.is_empty() {
            return Ok(());
        }
        on_oid(oid)
    })
    .map_err(|e| {
        if e.downcast_ref::<EnoughHits>().is_some() || e.downcast_ref::<BudgetSpent>().is_some() {
            // The caller's own early exit: propagate it unchanged so the caller recognizes it.
            e
        } else {
            e.context(
                "git cannot list the tags reachable from these branches, so they cannot be confirmed clean",
            )
        }
    })
}

/// The tag scan surface is **always** every local branch, independent of whether the caller is
/// `agit push` or `agit scan`, and independent of what the destination subtracted.
///
/// # Why it does not follow the entry point's ref
///
/// The publish surface for tags does not split by ref in the first place: `agit push` pushes
/// `tags_to_push(refs_to_push(branches, has_main))`, and `refs_to_push` appends main
/// unconditionally whenever it exists locally. So even with `agit scan <some ref>`, a push may
/// still send a tag on another branch.
///
/// Both entry points go through this one function rather than each passing a branch set of its
/// own: **the contract is guaranteed by a single source, not by remembering to change two
/// places**.
fn tag_scan_branches(repo: &crate::domain::repo::Repo) -> crate::Result<Vec<String>> {
    local_branches(repo)
}

/// Every local branch name.
///
/// The widest answer to "what may this repo push": `agit push --all` can push any of them, and
/// `push::refs_to_push` also appends `main` unconditionally. Taking this set as the scan surface
/// makes it **a superset of the publish surface** — the direction is right, a few extra tags only
/// mean a few extra reports, and a miss is what is unacceptable.
fn local_branches(repo: &crate::domain::repo::Repo) -> crate::Result<Vec<String>> {
    // Failing to list is **not** "there are no branches".
    //
    // `.unwrap_or_default()` conflates "the git command failed" with "there really are no
    // branches" into the same empty `Vec`, and an empty `Vec` makes `scan_tag_objects` return
    // empty = clean. This is the only entry test on this path, and conflating them allows
    // silently whenever refs cannot be listed.
    let Some(out) = repo.git_opt(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])
    else {
        anyhow::bail!(
            "git cannot list the local branches, so the content to publish cannot be confirmed clean"
        );
    };
    Ok(out
        .lines()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_string)
        .collect())
}

/// A commit message leaves with a push too, so it is in the scan surface as well.
///
/// Only the ones **this push really sends** are scanned: rescanning what the destination already
/// has does nothing but stop the user on history they cannot change. The range shares a source
/// with the blob path (both come from [`Destination::revs`]) and is not assembled again here —
/// written once on each path, one push shows the mismatch "commits scanned over the unpushed
/// range, blobs scanned over the full history", and the other half of a mismatch is sooner or
/// later a miss.
///
/// # Why the range is `--branches` and not the current branch
///
/// `agit push` pushes `refs_to_push(branches, has_main)` — `-b` may be given several times,
/// `--all` is everything with updates, and main is appended unconditionally whenever it exists
/// locally. So pushing from a session branch sends an unpushed commit message on main that
/// carries a secret, while the local scan says clean.
///
/// This is **one bug on two levels** with the tag path: both levels depend on the same invariant
/// (the scan surface covers every branch), and giving branch coverage to only one of them does
/// not deliver it.
///
/// # Why there is no cap on the number of commits
///
/// With `-n 200` there are two problems: `-n` limits the **total** rather than the per-branch
/// count (observed: `-n 4 --branches` gives four commits interleaved across two branches, not
/// four per branch), and past it the miss is **silent** — the scan surface becomes a proper
/// subset of the publish surface. Boundedness is now handled uniformly by [`ScanLimits`], and
/// crossing a limit is said out loud ([`Unscanned`]) instead of quietly scanning a few fewer.
fn scan_commit_messages(
    repo: &crate::domain::repo::Repo,
    sel: &[&str],
    limits: &ScanLimits,
    allowlist: &HashSet<String>,
    registered: &RegisteredMatcher,
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    // Only confirm that the repo is readable; an unborn HEAD does **not** make this return empty
    // — the selector is `--branches` and never touches HEAD. After `git checkout --orphan`,
    // unpushed commits on other branches are pushed all the same.
    assert_repo_readable(repo)?;
    scan_messages(repo, sel, limits, allowlist, registered, out, unscanned)
}

/// `git rev-list <sel>` for the OIDs, `git cat-file --batch` to read each **raw body** and scan
/// it.
///
/// # Why two steps and not `rev-list --header`
///
/// `--header` terminates each record with a NUL, and **a commit body may contain a NUL**:
/// `git hash-object -t commit --literally` produces one and `git push` accepts it. The record is
/// then truncated at the first NUL and the suffix (possibly holding a secret) escapes the scan
/// surface silently — observed: `rev-list --header` itself cuts the body at that NUL, and not one
/// byte of what follows appears in the output.
///
/// Split in two, each half's framing precondition holds: `rev-list` gives only OIDs (hex,
/// unambiguous to split by line, and still readable as a stream), and the body is taken by the
/// **byte count** git reports itself through
/// [`crate::domain::repo::Repo::git_cat_file_batch`], so whatever the body contains changes
/// nothing. The tag path frames by `%(raw:size)`, and this brings the commit path onto the same
/// test.
fn scan_messages(
    repo: &crate::domain::repo::Repo,
    sel: &[&str],
    limits: &ScanLimits,
    allowlist: &HashSet<String>,
    registered: &RegisteredMatcher,
    out: &mut HitCollector,
    unscanned: &mut Unscanned,
) -> crate::Result<()> {
    // Already full: no commit needs reading. The verdict was settled long ago (non-empty is a
    // refusal), and reading on only stuffs a list nobody prints. (`is_full` also records "this
    // list is incomplete".)
    if out.is_full() {
        return Ok(());
    }
    let mut args = vec!["rev-list"];
    args.extend_from_slice(sel);
    // Read the OIDs **as a stream**, keeping the whole stdout out of memory: without a "scan only
    // the most recent few hundred" cap, `rev-list`'s output grows linearly with the history.
    //
    // Failing to read is **not** "there is no message here": as soon as git errors (a malformed
    // selector, an abnormal repo state, wrong `rev-list` usage) this errors instead of returning
    // empty — a scanner's worst failure is calling a fault clean. `rev-list` with `-n` and no rev
    // argument errors outright, and swallowing that would take the whole commit scan out
    // silently.
    let mut oids: Vec<String> = Vec::new();
    repo.git_stream_split(&args, b'\n', |rec| {
        let oid = String::from_utf8_lossy(rec);
        let oid = oid.trim();
        if !oid.is_empty() {
            oids.push(oid.to_string());
        }
        Ok(())
    })
    .map_err(|e| {
        anyhow::anyhow!(
            "git rev-list {} cannot be read, so the history cannot be confirmed clean: {e}",
            sel.join(" ")
        )
    })?;

    // Read the **raw commit body** (every header line + the blank line + the message), not a
    // projection of a few chosen fields.
    //
    // A field projection (`%an %cn %B` and the like) does not cover it: the set of headers on a
    // commit object is not closed for git, every line outside the list is outside the scan
    // surface, and **an ordinary git command writes one**:
    //
    //     git -c i18n.commitEncoding=<secret> commit --allow-empty -m clean
    //
    // writes a line `encoding <secret>`. It is public with every clone and readable verbatim by
    // `git cat-file commit`, yet it is nowhere in that projection. `gpgsig` is the same. In other
    // words, the list of "which fields to scan" is itself the attack surface.
    //
    // The server's `walk_new_commits` reads the same raw body, so the two scan surfaces stay
    // aligned.
    let cap_bytes = usize::try_from(limits.max_object_bytes).unwrap_or(usize::MAX);
    repo.git_cat_file_batch(oids, cap_bytes, |oid, kind, body| {
        // Once there are enough hits it **stops scanning** — what is skipped is
        // `scan_text_capped`, not just one push. Hits are a **function** of the input, not a
        // subset: a history matching on every line produces `Hit`s on the order of its line
        // count, and streaming the input does not bound that side. Stopping when full does not
        // change the verdict (one hit is enough to refuse).
        if out.is_full() {
            return Ok(());
        }
        let payload = match body {
            crate::domain::repo::ObjectBody::Read(b) => b,
            // A commit body over the line (`git commit -F <a huge file>` is enough) had not one
            // byte read. Record it, so it does not enter a report claiming to be clean in the
            // shape of "no hits".
            crate::domain::repo::ObjectBody::TooLarge(n) => {
                unscanned
                    .oversized
                    .push((oid[..oid.len().min(8)].to_string(), n as u64));
                return Ok(());
            }
        };
        if kind != "commit" {
            return Ok(());
        }
        // The conversion applies to this one body — the scanning engine wants a `&str`, and the
        // rule set is insensitive to U+FFFD (it appears in no credential shape). The framing was
        // already done on bytes.
        let text = String::from_utf8_lossy(payload);
        // The bound is **how much the collector can still take**: materializing every hit of a
        // whole body before handing it over means the allocation over the line already happened
        // before the collector took over. See [`scan_text_capped`].
        let cap = out.remaining();
        let scanned =
            scan_text_capped_registered(&text, allowlist, Policy::CLIENT, cap, registered);
        // This commit body's own budget ran out: it holds hits that were never scanned.
        if scanned.truncated {
            out.mark_truncated();
        }
        out.extend(scanned.hits.into_iter().map(|mut h| {
            // The label says "commit object" and not "commit": what is scanned is the whole raw
            // body, a hit may sit on a header line such as `encoding`, and **the line number is
            // counted in the object too**. Written as just "commit", the user takes that line
            // number to `git log`, where the message carries no headers, the number does not line
            // up, and they conclude the report is wrong. Saying it is an object makes the
            // matching action clear: line N of `git cat-file commit <sha>`.
            h.file = Some(format!("commit object {}", &oid[..oid.len().min(8)]));
            h.source = Source::CommitObject;
            h
        }));
        Ok(())
    })
    .map_err(|e| {
        anyhow::anyhow!(
            "git cat-file cannot read the commit bodies on {}, so the history cannot be confirmed clean: {e}",
            sel.join(" ")
        )
    })
}

/// Mask in place: replace each matched span with `[redacted:<rule>]` and return (text, hits).
///
/// It reads the same rule table as [`scan_text`], but honours **neither the allowlist nor the
/// inline waiver** — those two are "do not stop me locally" switches, not "carry the secret into
/// the published copy" switches.
///
/// One view, one match pass, replacements in reverse: of overlapping hits (the same characters
/// recognized by two rules) only the first is kept, so no placeholder ever gets masked a second
/// time.
pub fn scrub(text: &str) -> (String, usize) {
    let view = view_of(text);
    let raw = raw_hits(&view);
    let mut out = text.to_string();
    let mut applied = 0usize;
    let mut last_start = usize::MAX;
    // Replace in reverse: change the later spans first so the earlier offsets stay valid.
    for h in raw.iter().rev() {
        if h.end > last_start {
            continue; // Overlaps a span that was already replaced.
        }
        out.replace_range(h.start..h.end, &format!("[redacted:{}]", h.rule));
        last_start = h.start;
        applied += 1;
    }
    (out, applied)
}

/// Redaction: keep the first four and the last two characters. Enough of the ends for the user to
/// recognize which key it is, not enough to reuse it.
pub fn redact(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= 8 {
        return "*".repeat(c.len().max(3));
    }
    let head: String = c.iter().take(4).collect();
    let tail: String = c[c.len() - 2..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(6))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn registered_low_entropy_literal_is_not_suppressed_by_allowlist() {
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[(
            "sec_memorable",
            "blue horse battery",
        )]);
        let allowlist = HashSet::from(["blue horse battery".to_string()]);
        let report = scan_text_capped_registered(
            "prefix blue horse battery suffix",
            &allowlist,
            Policy::CLIENT,
            10,
            &matcher,
        );
        assert!(!report.truncated);
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].rule, "registered-secret");
        assert_eq!(report.hits[0].line, 1);
        assert_eq!(report.hits[0].redacted, "[redacted:registered-secret]");
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn registered_literals_match_decoded_json_strings() {
        let secret = "quote\" slash\\ and\nnewline";
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_json", secret)]);
        let line = serde_json::to_string(&serde_json::json!({
            "message": format!("before {secret} after")
        }))
        .unwrap();
        assert!(
            !line.contains(secret),
            "the regression requires a wire representation different from the semantic value"
        );

        let (hits, truncated) = registered_hits_semantic_capped(&line, 10, &matcher);
        assert!(!truncated);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[0].rule, "registered-secret");
    }

    /// One unparseable line must not disable semantic matching for the rest.
    ///
    /// A crashed or still-writing harness leaves a half-written trailing line
    /// behind — the single most ordinary state a live transcript is found in.
    /// Deciding «JSONL or not» for the whole file meant that line silently
    /// downgraded the gate to wire-byte matching, and every registered value
    /// containing `"`, `\` or a newline walked straight through `push` and
    /// `share`.
    #[cfg(feature = "secret-vault")]
    #[test]
    fn a_half_written_line_does_not_downgrade_the_rest_of_the_file() {
        let secret = "quote\" slash\\ and\nnewline";
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_json", secret)]);
        let good = serde_json::to_string(&serde_json::json!({
            "message": format!("before {secret} after")
        }))
        .unwrap();
        assert!(
            !good.contains(secret),
            "the value must be escaped on the wire"
        );
        let text = format!("{good}\n{{\"message\":\"truncated mid-w");

        let (hits, truncated) = registered_hits_semantic_capped(&text, 10, &matcher);
        assert!(!truncated);
        assert_eq!(
            hits.len(),
            1,
            "the escaped value on the intact line must still be reported: {hits:?}"
        );
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[0].rule, "registered-secret");
    }

    /// The two passes must not report the same occurrence twice.
    #[cfg(feature = "secret-vault")]
    #[test]
    fn a_value_visible_in_both_representations_is_reported_once() {
        let secret = "blue horse battery";
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_plain", secret)]);
        let text = serde_json::to_string(&serde_json::json!({ "message": secret })).unwrap();
        assert!(
            text.contains(secret),
            "a value with no escapes is identical in both representations"
        );

        let (hits, truncated) = registered_hits_semantic_capped(&text, 10, &matcher);
        assert!(!truncated);
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn registered_scanner_ignores_repository_placeholder_internals() {
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_short", "AGIT")]);
        let text = r#"{"value":"{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_00000000000000000000000000000000}}"}"#;
        let (hits, truncated) = registered_hits_semantic_capped(text, 10, &matcher);
        assert!(!truncated);
        assert!(hits.is_empty());
    }

    // All three scan paths collect into the **one** `HitCollector` the caller holds (only the
    // combination is bounded). A single test asks "what did this path report", so each gets a
    // thin wrapper carrying its own collector rather than every test opening one.
    fn tag_hits(
        repo: &crate::domain::repo::Repo,
        allowlist: &HashSet<String>,
        branches: &[String],
    ) -> crate::Result<Vec<Hit>> {
        let mut out = HitCollector::new();
        let mut unscanned = Unscanned::default();
        scan_tag_objects(
            repo,
            &ScanLimits::DEFAULT,
            allowlist,
            &RegisteredMatcher::default(),
            branches,
            &mut out,
            &mut unscanned,
        )?;
        Ok(out.into_hits())
    }

    fn commit_message_hits(
        repo: &crate::domain::repo::Repo,
        allowlist: &HashSet<String>,
    ) -> crate::Result<Vec<Hit>> {
        let mut out = HitCollector::new();
        let mut unscanned = Unscanned::default();
        let revs = Destination::Unknown.revs();
        let sel: Vec<&str> = revs.iter().map(String::as_str).collect();
        scan_commit_messages(
            repo,
            &sel,
            &ScanLimits::DEFAULT,
            allowlist,
            &RegisteredMatcher::default(),
            &mut out,
            &mut unscanned,
        )?;
        Ok(out.into_hits())
    }

    fn message_hits(
        repo: &crate::domain::repo::Repo,
        sel: &[&str],
        allowlist: &HashSet<String>,
    ) -> crate::Result<Vec<Hit>> {
        let mut out = HitCollector::new();
        let mut unscanned = Unscanned::default();
        scan_messages(
            repo,
            sel,
            &ScanLimits::DEFAULT,
            allowlist,
            &RegisteredMatcher::default(),
            &mut out,
            &mut unscanned,
        )?;
        Ok(out.into_hits())
    }

    /// Every hit the `agit scan` path sees.
    ///
    /// One call to [`scan_agent_repo`] — the scan surface does not split by ref (that is wrong in
    /// both directions, see its docs). The same entry point is in [`crate::commands::scan`],
    /// where the tests pin **attribution** (hits are not stamped with a ref name), while the
    /// tests here pin **coverage** (scan and push see the same things).
    ///
    /// The two paths **are the same function**, so "scan and push reach the same verdict" no
    /// longer rests on comparison but on the same code — see
    /// `scan_and_push_agree_on_the_same_repo`.
    fn scan_path_hits(repo: &crate::domain::repo::Repo) -> Vec<Hit> {
        scan_agent_repo(repo, &ScanPlan::full())
            .expect("the repo is healthy")
            .hits
    }

    /// Create a real (empty) git repo.
    ///
    /// `scan_agent_repo` treats "this is not a repo" as a failure rather than "there is no
    /// history" — which is right (unreadable is not clean), but it means a test can no longer
    /// grab any empty directory and call it an agent.
    fn repo_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git init");
        d
    }

    // A fake secret in a test has to **look real**: the gitleaks rules carry entropy thresholds,
    // a placeholder like `"a".repeat(36)` is filtered out by entropy, and writing a test with one
    // tests a path that never fires. The strings below are hand-typed high-entropy values and
    // correspond to no real credential.
    const AWS: &str = "AKIA4X7QZ2M5RT6VW3JH";
    const GHP: &str = "ghp_7Kd2mQ9xR4vB1nT8sW3zY6cL5jH0gF2aE4pU";
    const NPM: &str = "npm_9fK2mQ7xR4vB1nT8sW3zY6cL5jH0gF2aE4pU";
    const HF: &str = "hf_QrsTuvWxyzAbcdEfghIjklMnopQrstUvwx";
    const AGIT: &str = "agit_at_9f3ca71e04b8d25f6e103a4c7b9d82f051ae6cb37d40928ef15b6a3c8d072e94";
    const INTERNAL_HEX: &str = "9f3ca71e04b8d25f6e103a4c7b9d82f051ae6cb3";
    const UNTRUSTED_HEX: &str = "0123456789abcdef0123456789abcdef01234567";

    fn agent_envelope(content: serde_json::Value) -> crate::domain::transcript::Envelope {
        crate::domain::transcript::Envelope {
            source: "codex".into(),
            session_id: format!("agit-{INTERNAL_HEX}"),
            object_hash: crate::domain::transcript::object_hash(&content),
            content,
        }
    }

    fn trust(envelope: &crate::domain::transcript::Envelope) -> TrustedEnvelopeIdentities {
        let mut trusted = TrustedEnvelopeIdentities::new();
        trusted
            .entry(envelope.session_id.clone())
            .or_default()
            .insert(envelope.source.clone());
        trusted
    }

    /// The 40-hex hash AgentGit writes into an event envelope is not a Sourcegraph token.
    ///
    /// `sourcegraph-access-token` has a keyword prefilter, and in the real loop the preceding
    /// report already wrote the rule name into the session, so this fixture keeps it too. That
    /// makes it reproduce the "rule name + a fresh object hash" false positive of the next turn,
    /// instead of testing an empty result on text where the rule never runs.
    #[test]
    fn valid_agent_envelope_masks_only_internal_hashes() {
        let envelope = agent_envelope(serde_json::json!({
            "message": "scanner reported sourcegraph-access-token"
        }));
        let line = crate::domain::storage::envelope_line(&envelope);

        let raw = scan_text_with(&line, &none(), Policy::STRICT);
        assert!(
            raw.iter().any(|h| h.rule == "sourcegraph-access-token"),
            "precondition: the internal hash of a raw envelope must reproduce the false positive: {raw:?}"
        );

        let scanned = scan_repository_payload_capped(
            &line,
            &none(),
            &trust(&envelope),
            Policy::STRICT,
            50,
            &RegisteredMatcher::default(),
        );
        assert!(
            scanned.hits.is_empty(),
            "a valid envelope holds only hashes AgentGit generated, which must not block the next commit: {:?}",
            scanned.hits
        );
        assert!(!scanned.truncated);
    }

    /// Masking identities must not mask the registered rules along with them.
    ///
    /// The masked view is the only input this path hands to the rules. As soon as it falls back
    /// to [`scan_text_capped`], a low-entropy literal the user registered explicitly is only
    /// stopped inside an **invalid** envelope — while a valid envelope is exactly what every
    /// ordinary settlement goes through. The two tests above use an empty matcher, so swapping
    /// that back in turns nothing red; this one pins separately that the registered rules still
    /// run on the masked view.
    #[cfg(feature = "secret-vault")]
    #[test]
    fn masking_internal_identities_still_runs_registered_rules() {
        let envelope = agent_envelope(serde_json::json!({
            "message": "deploy with blue horse battery"
        }));
        let line = crate::domain::storage::envelope_line(&envelope);
        assert!(
            mask_valid_envelope_stream(&line, &trust(&envelope)).is_some(),
            "precondition: this line has to take the masking path, or the fallback branch is what is under test"
        );
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[(
            "sec_memorable",
            "blue horse battery",
        )]);

        let scanned = scan_repository_payload_capped(
            &line,
            // The registered rules do not honour the allowlist: repo content must not switch a
            // local policy off.
            &HashSet::from(["blue horse battery".to_string()]),
            &trust(&envelope),
            Policy::STRICT,
            50,
            &matcher,
        );

        assert!(
            scanned.hits.iter().any(|h| h.rule == "registered-secret"),
            "masking replaces only AgentGit's own two identity fields; the registered rules must still see content: {:?}",
            scanned.hits
        );
    }

    /// A self-consistent envelope shape and content hash do not make the session id an internal
    /// AgentGit identity.
    #[test]
    fn envelope_with_untrusted_session_id_cannot_hide_a_credential() {
        let envelope = agent_envelope(serde_json::json!({
            "message": "scanner reported sourcegraph-access-token"
        }));
        let line = crate::domain::storage::envelope_line(&envelope);
        let scanned = scan_repository_payload_capped(
            &line,
            &none(),
            &TrustedEnvelopeIdentities::new(),
            Policy::STRICT,
            50,
            &RegisteredMatcher::default(),
        );

        assert!(
            scanned
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "the repo meta never declared this session; even a fully self-consistent Envelope keeps its 40-hex value scanned: {:?}",
            scanned.hits
        );
    }

    /// A structural exemption must not exempt the envelope body along with it.
    #[test]
    fn secret_inside_valid_agent_envelope_is_still_reported() {
        let envelope = agent_envelope(serde_json::json!({
            "message": format!("deploy with {AWS}")
        }));
        let line = crate::domain::storage::envelope_line(&envelope);
        let scanned = scan_repository_payload_capped(
            &line,
            &none(),
            &trust(&envelope),
            Policy::STRICT,
            50,
            &RegisteredMatcher::default(),
        );

        assert!(
            scanned.hits.iter().any(|h| h.rule == "aws-access-token"),
            "only the internal identity fields may be masked; a real secret in content is still reported: {:?}",
            scanned.hits
        );
    }

    /// Replacement is by field span only: the same 40-hex string appearing in the body still
    /// matches.
    #[test]
    fn internal_value_repeated_in_content_is_not_globally_allowlisted() {
        let envelope = agent_envelope(serde_json::json!({
            "message": format!("sourcegraph-access-token candidate {INTERNAL_HEX}")
        }));
        let line = crate::domain::storage::envelope_line(&envelope);
        let scanned = scan_repository_payload_capped(
            &line,
            &none(),
            &trust(&envelope),
            Policy::STRICT,
            50,
            &RegisteredMatcher::default(),
        );

        assert!(
            scanned
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "content is not an internal AgentGit field and is not globally allowed just because the value happens to match: {:?}",
            scanned.hits
        );
    }

    /// One line whose hash does not match its content sends the whole payload back to scanning
    /// the raw text.
    #[test]
    fn tampered_envelope_falls_back_to_raw_scanning() {
        let envelope = agent_envelope(serde_json::json!({
            "message": "scanner reported sourcegraph-access-token"
        }));
        let canonical = crate::domain::storage::envelope_line(&envelope);
        let first = envelope.object_hash.as_bytes()[0];
        let replacement = if first == b'0' { '1' } else { '0' };
        let mut bad_hash = envelope.object_hash.clone();
        bad_hash.replace_range(..1, &replacement.to_string());
        let tampered = canonical.replacen(&envelope.object_hash, &bad_hash, 1);
        assert!(
            crate::domain::storage::parse_legacy_envelope_line(&tampered).is_err(),
            "precondition: the altered object hash must fail the integrity check"
        );

        let scanned = scan_repository_payload_capped(
            &tampered,
            &none(),
            &trust(&envelope),
            Policy::STRICT,
            50,
            &RegisteredMatcher::default(),
        );
        assert!(
            scanned
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "a corrupt envelope gets no internal-field exemption and must fail closed: {:?}",
            scanned.hits
        );
    }

    /// Multi-line v0 JSONL uses a different field order, and every line still goes through the
    /// same integrity checks.
    #[test]
    fn legacy_multiline_envelopes_mask_hashes_but_scan_content() {
        let clean = agent_envelope(serde_json::json!({
            "message": "scanner reported sourcegraph-access-token"
        }));
        let secret = agent_envelope(serde_json::json!({
            "message": format!("deploy with {AWS}")
        }));
        let legacy = |e: &crate::domain::transcript::Envelope| {
            format!(
                "{{\"content\":{},\"_object_hash\":{},\"_session_id\":{},\"_source\":{}}}\n",
                serde_json::to_string(&e.content).unwrap(),
                serde_json::to_string(&e.object_hash).unwrap(),
                serde_json::to_string(&e.session_id).unwrap(),
                serde_json::to_string(&e.source).unwrap(),
            )
        };
        let text = format!("{}{}", legacy(&clean), legacy(&secret));
        let scanned = scan_repository_payload_capped(
            &text,
            &none(),
            &trust(&clean),
            Policy::STRICT,
            50,
            &RegisteredMatcher::default(),
        );

        assert!(
            scanned.hits.iter().any(|h| h.rule == "aws-access-token"),
            "the content of v0 JSONL is still scanned: {:?}",
            scanned.hits
        );
        assert!(
            !scanned
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "an internal hash that passed validation in v0 JSONL is not a false positive either: {:?}",
            scanned.hits
        );
    }

    /// The `i`-th distinct npm token (`npm_` + 36 alphanumerics).
    ///
    /// The first three characters vary and the high-entropy tail is kept unchanged — the npm rule
    /// carries `entropy = 2` and `generic-api-key` carries `entropy = 3.5`, so a casually invented
    /// string is filtered out silently.
    fn npm_n(i: usize) -> String {
        format!("npm_{i:03x}2mQ7xR4vB1nT8sW3zY6cL5jH0gF2aE4pU")
    }

    /// The `i`-th distinct agit token (`agit_at_` + 64 hex characters).
    fn agit_n(i: usize) -> String {
        format!("agit_at_{i:04x}a71e04b8d25f6e103a4c7b9d82f051ae6cb37d40928ef15b6a3c8d072e94")
    }

    /// `cap` counts the hits **after dedupe**, not the `Raw`s produced.
    ///
    /// # What this pins
    ///
    /// `token: npm_…` is recognized once by `generic-api-key` and once by `npm-access-token` on
    /// **the same span**, and after [`dedupe_same_span`] only one remains. Were the budget to
    /// count `Raw`s, that one hit would eat two slots: at `cap = 2` the budget is exhausted on the
    /// spot, dedupe leaves one reportable hit, and the **other kind** of secret behind it is never
    /// scanned — while the collector is not full and the report still calls itself complete.
    ///
    /// # Why the second secret is an agit token and not an AWS one
    ///
    /// Scanning iterates **by rule**, not by position. `aws-access-token` sits **before**
    /// `generic-api-key` in the rule set, so by the time it runs the budget is untouched and it
    /// can never be squeezed out — writing this test with it yields a test that is green forever.
    /// The second secret has to be recognized by a rule ordered **after the two overlapping
    /// ones**: all of `agit-rules.toml` loads after `gitleaks.toml`, and `agit-token` satisfies
    /// that.
    #[test]
    fn the_cap_counts_unique_findings_not_rule_overlaps() {
        // Precondition 1: that pair of rules really overlaps — the catch-all also recognizes
        // this line, and dedupe leaves one.
        let overlapping = format!("token: {NPM}\n");
        let catch_all = rules::all()
            .iter()
            .find(|r| r.id == "generic-api-key")
            .expect("the catch-all rule is in the rule set");
        assert!(
            catch_all
                .regex()
                .is_some_and(|re| re.is_match(&view_of(&overlapping))),
            "precondition: `generic-api-key` also recognizes this line, or the overlap is not under test"
        );
        let collapsed = scan_text_with(&overlapping, &none(), Policy::STRICT);
        assert_eq!(
            collapsed.len(),
            1,
            "precondition: both rules recognize the same span and dedupe into one: {collapsed:?}"
        );

        // Precondition 2: uncapped, both kinds of secret are reported.
        let text = format!("token: {NPM}\n{AGIT}\n");
        let uncapped: Vec<String> = scan_text_with(&text, &none(), Policy::STRICT)
            .into_iter()
            .map(|h| h.rule)
            .collect();
        assert!(
            uncapped.iter().any(|r| r == "npm-access-token")
                && uncapped.iter().any(|r| r == "agit-token"),
            "precondition: unbounded, both kinds of secret are present: {uncapped:?}"
        );

        // Two hits, two slots of budget: one for npm (shared by two rules), one for agit.
        let report = scan_text_capped(&text, &none(), Policy::STRICT, 2);
        let got: Vec<&str> = report.hits.iter().map(|h| h.rule.as_str()).collect();
        assert!(
            got.contains(&"npm-access-token"),
            "the overlapping one keeps the more specific rule: {got:?}"
        );
        assert!(
            got.contains(&"agit-token"),
            "an overlap must not eat the budget of the secret behind it: {got:?}"
        );
    }

    /// Stopping early because the budget was reached has to **say** the list is incomplete.
    ///
    /// # Why "is the collector full" cannot be used to infer it
    ///
    /// The miss happens **inside a single carrier**: once the second rule on the same span eats
    /// the budget the engine stops scanning, while what reaches the collector is the shorter
    /// deduplicated list — the collector is not full at all, so `truncated = false`. **A hit went
    /// unscanned and the report claims to be complete** is the shape this whole line of fixes
    /// keeps removing.
    ///
    /// So the signal is passed out explicitly by the producer ([`ScanReport::truncated`]) rather
    /// than inferred. The reverse is pinned too: without reaching the budget it has to be
    /// `false`, or every scan carries a "there is more" nobody reads for long.
    #[test]
    fn hitting_the_cap_marks_the_report_incomplete() {
        let mut text = String::new();
        // First half: one npm token per line, **recognized by two rules at once** (each counting
        // as one after dedupe).
        for i in 0..(MAX_REPORTED_HITS / 2) {
            text.push_str(&format!("token: {}\n", npm_n(i)));
        }
        // Then: another kind of secret, in numbers that certainly exhaust the budget (its rule
        // is ordered after the two above).
        for i in 0..(MAX_REPORTED_HITS + 1) {
            text.push_str(&format!("{}\n", agit_n(i)));
        }
        assert!(
            scan_text_with(&text, &none(), Policy::CLIENT).len() > MAX_REPORTED_HITS,
            "precondition: the unbounded output exceeds the cap, or the budget is never reached"
        );

        // The engine layer: reaching the budget sets the flag, not reaching it leaves it unset.
        assert!(
            scan_text_capped(&text, &none(), Policy::CLIENT, MAX_REPORTED_HITS).truncated,
            "the budget ran out with unscanned hits behind it, so this list is incomplete"
        );
        assert!(
            !scan_text_capped(
                &format!("just the one: {AWS}\n"),
                &none(),
                Policy::CLIENT,
                50
            )
            .truncated,
            "without reaching the budget nothing may claim there is more"
        );

        // End to end: this signal has to travel all the way to `ScanReport`.
        let d = repo_dir();
        std::fs::create_dir_all(d.path().join("memory")).unwrap();
        std::fs::write(d.path().join("memory/notes.md"), &text).unwrap();
        let repo = crate::domain::repo::Repo::at(d.path());
        let report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            report.truncated,
            "the scan stopped early on the budget, so the report must not claim this is all of it: {} hits",
            report.hits.len()
        );
    }

    /// Both session files awaiting publication get scanned.
    ///
    /// A scan rooted at a hard-coded `sessions/` while the transcript of that layout sits at the
    /// repo root makes the gate walk a directory that does not exist and return zero hits every
    /// time, taking the pre-push secret check out entirely and silently.
    #[test]
    fn both_committed_files_get_scanned() {
        for name in [
            crate::domain::meta::LOG_FILE,
            crate::domain::meta::VIEW_FILE,
        ] {
            let d = repo_dir();
            crate::domain::meta::ensure_session_dir(d.path()).unwrap();
            std::fs::write(d.path().join(name), format!("{{\"t\":\"{GHP}\"}}\n")).unwrap();

            let repo = crate::domain::repo::Repo::at(d.path());
            let hits = scan_agent_repo(&repo, &ScanPlan::full()).unwrap().hits;
            assert!(
                hits.iter()
                    .any(|h| h.file.as_deref().is_some_and(|f| f.ends_with(name))),
                "the secret in {name} must be scanned: {hits:?}"
            );
        }
    }

    /// A committed, unmodified file: **one report per carrier**, no more and no less.
    ///
    /// # Why this is not "the same hit reported twice"
    ///
    /// The two hits have different ways out, and neither alone works: the working-tree one can be
    /// annotated `agit:allow-secret`, but that only governs **the next** commit — the old blob is
    /// still reachable, still leaves with this push, and only rewriting history removes it.
    /// Collapsed into one, the report gives only one of the ways out, and the user follows it and
    /// is stopped by the same gate. So carriers do not collapse into each other (a blob's label is
    /// `blob object <sha8>/<path>`, which keys differently from the working-tree one by nature).
    ///
    /// What is pinned is that there is no duplication **inside a single carrier**: one hit from
    /// the working-tree pass, one from the blob pass, neither doubled.
    #[test]
    fn a_committed_file_hit_is_reported_once_per_carrier() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(d.path().join("AGENTS.md"), format!("deploy key: {AWS}\n")).unwrap();
        run(&["add", "AGENTS.md"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = scan_agent_repo(&repo, &ScanPlan::full())
            .expect("the repo is healthy")
            .hits;
        let of = |src: Source| -> Vec<&Hit> {
            hits.iter()
                .filter(|h| h.rule == "aws-access-token" && h.source == src)
                .collect()
        };
        let in_worktree = of(Source::File);
        assert_eq!(
            in_worktree.len(),
            1,
            "the working-tree pass visits this file once and must not report two: {in_worktree:?}"
        );
        let in_history = of(Source::BlobObject);
        assert_eq!(
            in_history.len(),
            1,
            "this blob appears once among the reachable objects and must not report two: {in_history:?}"
        );
        // The history one has to say which object it is — the user takes it to
        // `git cat-file blob` / `git log --all --find-object=`, and with no oid in the label
        // neither suggestion can be carried out.
        let at = in_history[0].file.as_deref().unwrap_or("");
        assert!(
            at.starts_with("blob object ") && at.ends_with("/AGENTS.md"),
            "a blob hit's location carries both the oid and the path: {at:?}"
        );
    }

    /// A secret deleted at the tip leaves with a push **all the same**, so the scan surface
    /// cannot be "the tree at each branch tip".
    ///
    /// # What this pins
    ///
    /// A scan surface of `ls-tree -r <each branch>` is the **last frame** of the history, while
    /// what push sends is a **set of objects**: the first commit writes the secret into
    /// `payload.txt` and the second deletes the file, so `ls-tree -r` is empty and the working
    /// tree holds nothing, yet `rev-list --objects` still lists that blob — the first push sends
    /// it with every byte intact while the local gate says clean.
    ///
    /// The three precondition assertions are not decoration: without them this case can turn
    /// green for unrelated reasons under an implementation whose surface shrank back to the
    /// tips.
    #[test]
    fn a_secret_deleted_at_the_tip_is_still_scanned() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(d.path().join("payload.txt"), format!("key = {AWS}\n")).unwrap();
        run(&["add", "payload.txt"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);
        run(&["rm", "-q", "payload.txt"]);
        run(&["commit", "-q", "-m", "another entirely ordinary commit"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        // Precondition 1: the tip's tree no longer holds this file.
        assert!(
            !repo
                .git_opt(&["ls-tree", "-r", "HEAD"])
                .expect("HEAD is readable")
                .contains("payload.txt"),
            "precondition fails: the tip's tree still holds this file, so this case tests nothing"
        );
        // Precondition 2: it is not in the working tree either, so the working-tree pass cannot
        // save it.
        assert!(
            !d.path().join("payload.txt").exists(),
            "precondition fails: the file is still in the working tree"
        );
        // Precondition 3: it is still among the reachable objects, which is exactly what the
        // first push sends.
        assert!(
            repo.git_opt(&["rev-list", "--objects", "--branches"])
                .expect("it lists")
                .contains("payload.txt"),
            "precondition fails: the blob is not in the reachable set, so push would not send it either"
        );

        let hits = scan_agent_repo(&repo, &ScanPlan::full())
            .expect("the repo is healthy")
            .hits;
        let leak = hits
            .iter()
            .find(|h| h.rule == "aws-access-token")
            .unwrap_or_else(|| {
                panic!("the first push sends this blob along, so the scan must see it: {hits:?}")
            });
        assert_eq!(
            leak.source,
            Source::BlobObject,
            "it comes from a blob in history; reported as file the user hunts the working tree for characters that are not there: {leak:?}"
        );
    }

    /// With the secret in a current file on **another branch**, it has to be visible from
    /// whichever ref is scanned.
    ///
    /// # What this pins
    ///
    /// A tree scan surface that follows "the ref being scanned" does not match the publish
    /// surface of `agit push -b other`, which is more than `other` alone: `push::refs_to_push`
    /// appends main unconditionally whenever it exists locally. So with the secret in a current
    /// file on `main` and `other` clean, `agit scan other` says clean while push stops it — and
    /// the user cannot reproduce locally why they were refused.
    ///
    /// Two cross-sections of one hole with the case above: one is not deep enough in history
    /// (tips only), the other is not wide enough across branches (only the ref being scanned).
    /// The correct scan surface is the same for both: **every reachable object this push may
    /// send**.
    #[test]
    fn a_secret_on_another_branch_file_is_scanned_from_any_ref() {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "other"]);
        // The secret is in a **current file** on main (not in a message), and only on main.
        std::fs::create_dir_all(d.path().join("memory")).unwrap();
        std::fs::write(
            d.path().join("memory/notes.md"),
            format!("deploy key: {AWS}\n"),
        )
        .unwrap();
        run(&["add", "memory/notes.md"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);
        // Currently on other, so the working tree is clean.
        run(&["checkout", "-q", "other"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        // Precondition 1: other's tree does not hold it.
        assert!(
            !repo
                .git_opt(&["ls-tree", "-r", "other"])
                .expect("other is readable")
                .contains("notes.md"),
            "precondition fails: other holds this file too, so this case tests nothing"
        );
        // Precondition 2: it is not in the working tree either.
        assert!(
            !d.path().join("memory/notes.md").exists(),
            "precondition fails: the file survived the checkout in the working tree"
        );

        let hits = scan_path_hits(&repo);
        assert!(
            hits.iter()
                .any(|h| h.rule == "aws-access-token" && h.source == Source::BlobObject),
            "`agit push -b other` pushes main along, so the scan surface has to cover it: {hits:?}"
        );
    }

    /// Two **different** secrets that redact identically both get reported.
    ///
    /// # What this pins
    ///
    /// A dedupe key that uses the redaction as content identity keeps only the first four and the
    /// last two characters. Two different AWS keys both start with `AKIA` by construction, so once
    /// the last two collide and they land on the same path and the same line the key is exactly
    /// the same: the second **real credential** is silently collapsed away while `truncated` stays
    /// false — the report is short one real hit and claims to be complete.
    ///
    /// So that field of the key is the fingerprint of the raw matched bytes
    /// ([`Hit::fingerprint`]), and the display layer still shows only the redacted fragment.
    #[test]
    fn two_secrets_that_redact_alike_are_both_reported() {
        // The first four (`AKIA`) and the last two (`JH`) match while the middle differs — after
        // redaction they are identical.
        const A: &str = "AKIA4X7QZ2M5RT6VW3JH";
        // The alphabet follows the rule: `aws-access-token`'s regex is `AKIA[A-Z2-7]{16}`, so a
        // casually written string containing `0`/`1`/`9` matches nothing and this case would be
        // green for no reason.
        const B: &str = "AKIA7DKV3QRZ5TMX6BJH";
        assert_ne!(A, B, "precondition: these are two different credentials");
        assert_eq!(
            redact(A),
            redact(B),
            "precondition: the two redact identically, which is what makes a redaction-keyed dedupe collide"
        );

        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // **Same path, same line**: rule, file and line all collide, and only the content field
        // can tell them apart.
        std::fs::create_dir_all(d.path().join("memory")).unwrap();
        std::fs::write(
            d.path().join("memory/notes.md"),
            format!("prod={A} staging={B}\n"),
        )
        .unwrap();
        run(&["add", "memory/notes.md"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        let count = |src: Source| {
            report
                .hits
                .iter()
                .filter(|h| h.rule == "aws-access-token" && h.source == src)
                .count()
        };
        assert_eq!(
            count(Source::File),
            2,
            "two different credentials must not collapse into one because their redacted fragments match: {:?}",
            report.hits
        );
        // The same content in history is also two hits on one line, and that pass must not
        // collapse them either.
        assert_eq!(
            count(Source::BlobObject),
            2,
            "the blob pass must not swallow the second one either: {:?}",
            report.hits
        );
        assert!(
            !report.truncated,
            "no cap was reached here, so the report must not call itself incomplete"
        );
    }

    /// Shared files (memory/, skills/, AGENTS.md) leave with a push too, so they are scanned.
    ///
    /// The designed surface is "outbound jsonl, shared files, commit messages", and a scan of the
    /// two jsonl files alone lets a credential written into `memory/notes.md` bypass the gate
    /// entirely.
    #[test]
    fn shared_files_are_in_the_surface_too() {
        for rel in ["AGENTS.md", "memory/notes.md", "skills/deploy/SKILL.md"] {
            let d = repo_dir();
            let p = d.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("deploy with this: {AWS}\n")).unwrap();

            let repo = crate::domain::repo::Repo::at(d.path());
            let hits = scan_agent_repo(&repo, &ScanPlan::full()).unwrap().hits;
            assert!(
                hits.iter().any(|h| h.file.as_deref() == Some(rel)),
                "the secret in {rel} must be scanned: {hits:?}"
            );
        }
    }

    /// `session/meta.json` shares a directory with the session itself, and the scan surface must
    /// not pull it in.
    #[test]
    fn the_meta_file_is_still_skipped() {
        let d = repo_dir();
        // A session identity is 40 hex characters, which matches the generic rules reliably, and
        // it is public by design.
        crate::domain::meta::ensure_session_dir(d.path()).unwrap();
        std::fs::write(
            d.path().join(crate::domain::meta::FILE),
            format!(
                "{{\"session\":\"agit-{}\",\"key\":\"{}\"}}\n",
                "b".repeat(40),
                AWS
            ),
        )
        .unwrap();
        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = scan_agent_repo(&repo, &ScanPlan::full()).unwrap().hits;
        assert!(
            hits.is_empty(),
            "meta.json is not in the scan surface: {hits:?}"
        );
    }

    /// Create a git repo with an identity and return a closure that runs git inside it.
    fn repo_with_git() -> (tempfile::TempDir, impl Fn(&[&str])) {
        let d = repo_dir();
        let root = d.path().to_path_buf();
        let run = move |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        (d, run)
    }

    /// Both blob paths of the repository-level entry point (working tree and reachable objects)
    /// share one structured scan view.
    #[test]
    fn repository_scan_ignores_envelope_hashes_but_not_envelope_content() {
        let (d, run) = repo_with_git();
        let clean = agent_envelope(serde_json::json!({
            "message": "scanner reported sourcegraph-access-token"
        }));
        let clean_line = crate::domain::storage::envelope_line(&clean);
        assert!(
            scan_text_with(&clean_line, &none(), Policy::STRICT)
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "precondition: without the structured scan view this event really is a false positive"
        );

        let meta = crate::domain::meta::Meta::new(
            clean.session_id.clone(),
            clean.source.clone(),
            "/worktree".into(),
        );
        crate::domain::meta::write(d.path(), &meta).unwrap();

        let clean_path = d.path().join("events/0/0/0/0/clean-event");
        std::fs::create_dir_all(clean_path.parent().unwrap()).unwrap();
        std::fs::write(&clean_path, &clean_line).unwrap();
        let repo = crate::domain::repo::Repo::at(d.path());
        let worktree = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            !worktree
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "the internal hash of a working-tree event is not a false positive: {:?}",
            worktree.hits
        );

        // An ordinary file in the same repo can forge an Envelope whose hash is self-consistent,
        // but its session is claimed by no meta, so it cannot borrow the structured view to hide
        // a 40-hex credential.
        let mut forged = agent_envelope(serde_json::json!({
            "message": "scanner reported sourcegraph-access-token"
        }));
        forged.session_id = format!("agit-{UNTRUSTED_HEX}");
        let forged_path = d.path().join("skills/forged-envelope.json");
        std::fs::create_dir_all(forged_path.parent().unwrap()).unwrap();
        std::fs::write(&forged_path, crate::domain::storage::envelope_line(&forged)).unwrap();
        let forged_report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            forged_report.hits.iter().any(|h| {
                h.file
                    .as_deref()
                    .is_some_and(|p| Path::new(p) == Path::new("skills/forged-envelope.json"))
                    && h.rule == "sourcegraph-access-token"
            }),
            "an Envelope forged in an ordinary file has no meta identity and cannot hide the session value: {:?}",
            forged_report.hits
        );
        std::fs::remove_file(forged_path).unwrap();

        let secret = agent_envelope(serde_json::json!({
            "message": format!("deploy with {AWS}")
        }));
        let secret_path = d.path().join("events/1/1/1/1/secret-event");
        std::fs::create_dir_all(secret_path.parent().unwrap()).unwrap();
        std::fs::write(&secret_path, crate::domain::storage::envelope_line(&secret)).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "add event fixtures"]);
        std::fs::remove_file(&clean_path).unwrap();
        std::fs::remove_file(&secret_path).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "remove event fixtures"]);

        let history = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            history
                .hits
                .iter()
                .any(|h| h.source == Source::BlobObject && h.rule == "aws-access-token"),
            "a real secret in a history event's content is still reported: {:?}",
            history.hits
        );
        assert!(
            !history
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "the internal hash of a history event is not a false positive: {:?}",
            history.hits
        );
    }

    /// A cross-repo merge keeps the source events' original envelopes; once the source ref is
    /// deleted, the second parent is still their provenance.
    #[test]
    fn cross_repo_merge_keeps_source_envelope_identity_trusted() {
        let source = repo_dir();
        let target = repo_dir();
        let git = |root: &Path, args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let configure = |root: &Path| {
            git(root, &["config", "user.email", "t@t"]);
            git(root, &["config", "user.name", "t"]);
            git(root, &["config", "commit.gpgsign", "false"]);
            git(root, &["branch", "-M", "main"]);
        };
        configure(source.path());
        configure(target.path());

        let write_snapshot = |root: &Path, meta: &crate::domain::meta::Meta, log: &str| {
            crate::domain::meta::write(root, meta).unwrap();
            for (rel, bytes) in crate::domain::storage::snapshot_files(log, log).unwrap() {
                let path = root.join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
        };

        let source_envelope = crate::domain::transcript::Envelope {
            source: "codex".into(),
            session_id: format!("agit-{UNTRUSTED_HEX}"),
            content: serde_json::json!({
                "message": "scanner reported sourcegraph-access-token"
            }),
            object_hash: String::new(),
        };
        let source_envelope = crate::domain::transcript::Envelope {
            object_hash: crate::domain::transcript::object_hash(&source_envelope.content),
            ..source_envelope
        };
        let source_line = crate::domain::storage::envelope_line(&source_envelope);
        assert!(
            scan_text_with(&source_line, &none(), Policy::STRICT)
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "precondition: without reading merge provenance the source session id reproduces the false positive"
        );
        let source_meta = crate::domain::meta::Meta::new(
            source_envelope.session_id.clone(),
            source_envelope.source.clone(),
            "/source".into(),
        );
        write_snapshot(source.path(), &source_meta, &source_line);
        git(source.path(), &["add", "-A"]);
        git(source.path(), &["commit", "-qm", "source turn"]);

        let target_envelope = agent_envelope(serde_json::json!({"message": "target turn"}));
        let target_line = crate::domain::storage::envelope_line(&target_envelope);
        let target_meta = crate::domain::meta::Meta::new(
            target_envelope.session_id.clone(),
            target_envelope.source.clone(),
            "/target".into(),
        );
        write_snapshot(target.path(), &target_meta, &target_line);
        git(target.path(), &["add", "-A"]);
        git(target.path(), &["commit", "-qm", "target turn"]);
        let target_head = git(target.path(), &["rev-parse", "HEAD"]);

        // Import the other repo's head as the merge's second parent; the temporary ref is
        // deleted once it is booked, so the source identity can only be recovered from the merge
        // ancestry and never by chance from some local branch tip.
        git(
            target.path(),
            &[
                "fetch",
                "-q",
                source.path().to_str().unwrap(),
                "refs/heads/main:refs/agit-test/source",
            ],
        );
        let source_head = git(target.path(), &["rev-parse", "refs/agit-test/source"]);
        let target_synthetic = |content: serde_json::Value| {
            let envelope = crate::domain::transcript::Envelope {
                source: target_meta.runtime.clone(),
                session_id: target_meta.session.clone(),
                object_hash: crate::domain::transcript::object_hash(&content),
                content,
            };
            crate::domain::storage::envelope_line(&envelope)
        };
        let start = target_synthetic(serde_json::json!({
            "type": "system",
            "subtype": "agit:__merge_start__",
            "source": "source/main",
        }));
        let summary = target_synthetic(serde_json::json!({
            "type": "user",
            "agit": "merge_summary",
            "message": {"role": "user", "content": "merged source conclusion"},
        }));
        let end = target_synthetic(serde_json::json!({
            "type": "system",
            "subtype": "agit:__merge_end__",
            "source": "source/main",
        }));
        let merged_log = format!("{target_line}{start}{source_line}{summary}{end}");
        let mut merged_meta = target_meta.clone();
        merged_meta.kind = crate::domain::meta::Kind::Merge;
        merged_meta.milestone = Some("merge source/main".into());
        write_snapshot(target.path(), &merged_meta, &merged_log);
        git(target.path(), &["add", "-A"]);
        let tree = git(target.path(), &["write-tree"]);
        let merge_commit = git(
            target.path(),
            &[
                "commit-tree",
                &tree,
                "-p",
                &target_head,
                "-p",
                &source_head,
                "-m",
                "agit: merge source/main -> main",
            ],
        );
        git(
            target.path(),
            &["update-ref", "refs/heads/main", &merge_commit, &target_head],
        );
        git(target.path(), &["reset", "-q", "--hard", "main"]);
        git(
            target.path(),
            &["update-ref", "-d", "refs/agit-test/source"],
        );
        assert_eq!(
            git(
                target.path(),
                &["for-each-ref", "--format=%(refname:short)", "refs/heads"]
            ),
            "main",
            "precondition: the source ref is deleted and only the merge ancestry remains"
        );

        let repo = crate::domain::repo::Repo::at(target.path());
        let merged = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            !merged
                .hits
                .iter()
                .any(|h| h.rule == "sourcegraph-access-token"),
            "the internal identity of a valid cross-repo source envelope does not fire the recursive false positive again: {:?}",
            merged.hits
        );

        // After one valid merge, an ordinary `git merge -s ours` inherits the first parent's
        // merge meta. It appends no complete merge block, so the second parent's meta alone
        // cannot authorize a new identity.
        const FORGED_HEX: &str = "89abcdef0123456789abcdef0123456789abcdef";
        let forged = crate::domain::transcript::Envelope {
            session_id: format!("agit-{FORGED_HEX}"),
            ..source_envelope.clone()
        };
        let forged_line = crate::domain::storage::envelope_line(&forged);
        let forged_meta = crate::domain::meta::Meta::new(
            forged.session_id.clone(),
            forged.source.clone(),
            "/forged-source".into(),
        );
        write_snapshot(source.path(), &forged_meta, &forged_line);
        git(source.path(), &["add", "-A"]);
        git(source.path(), &["commit", "-qm", "forged source turn"]);
        git(
            target.path(),
            &[
                "fetch",
                "-q",
                source.path().to_str().unwrap(),
                "refs/heads/main:refs/agit-test/forged-source",
            ],
        );
        let forged_source_head = git(
            target.path(),
            &["rev-parse", "refs/agit-test/forged-source"],
        );
        let inherited_tree = git(
            target.path(),
            &["rev-parse", &format!("{merge_commit}^{{tree}}")],
        );
        let ordinary_merge = git(
            target.path(),
            &[
                "commit-tree",
                &inherited_tree,
                "-p",
                &merge_commit,
                "-p",
                &forged_source_head,
                "-m",
                "ordinary git merge -s ours",
            ],
        );
        git(
            target.path(),
            &[
                "update-ref",
                "refs/heads/main",
                &ordinary_merge,
                &merge_commit,
            ],
        );
        git(target.path(), &["reset", "-q", "--hard", "main"]);
        git(
            target.path(),
            &["update-ref", "-d", "refs/agit-test/forged-source"],
        );

        let forged_path = target.path().join("skills/forged-after-merge.json");
        std::fs::create_dir_all(forged_path.parent().unwrap()).unwrap();
        std::fs::write(&forged_path, forged_line).unwrap();
        let forged_report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            forged_report.hits.iter().any(|h| {
                h.file
                    .as_deref()
                    .is_some_and(|p| Path::new(p) == Path::new("skills/forged-after-merge.json"))
                    && h.rule == "sourcegraph-access-token"
            }),
            "an ordinary two-parent commit with no complete LOG merge block cannot authorize the second parent's identity: {:?}",
            forged_report.hits
        );
    }

    #[test]
    fn source_log_ids_require_matching_event_objects() {
        let d = repo_dir();
        let git = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);

        let source_meta = crate::domain::meta::Meta::new(
            format!("agit-{UNTRUSTED_HEX}"),
            "codex".into(),
            "/source".into(),
        );
        let envelope = crate::domain::transcript::Envelope {
            source: source_meta.runtime.clone(),
            session_id: source_meta.session.clone(),
            content: serde_json::json!({"message": "source turn"}),
            object_hash: String::new(),
        };
        let envelope = crate::domain::transcript::Envelope {
            object_hash: crate::domain::transcript::object_hash(&envelope.content),
            ..envelope
        };
        let line = crate::domain::storage::envelope_line(&envelope);
        let event_id = crate::domain::storage::event_id(&line).unwrap();
        let event_path = d
            .path()
            .join(crate::domain::meta::event_path(&event_id).unwrap());
        std::fs::create_dir_all(event_path.parent().unwrap()).unwrap();
        std::fs::write(&event_path, &line).unwrap();
        crate::domain::meta::write(d.path(), &source_meta).unwrap();
        std::fs::write(
            d.path().join(crate::domain::meta::LOG_FILE),
            format!("{event_id}\n"),
        )
        .unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "source event"]);
        let complete_parent = git(&["rev-parse", "HEAD"]);

        let candidate =
            |source_parent: String, source_meta: crate::domain::meta::Meta| ValidatedMergeLog {
                merge_commit: complete_parent.clone(),
                source_parent,
                source_log_oid: event_id.clone(),
                target_session: format!("agit-{INTERNAL_HEX}"),
                target_runtime: "codex".into(),
                source_meta,
                source_event_ids: Some(std::sync::Arc::new(vec![event_id.clone()])),
                marker_ids: ["1".repeat(40), "2".repeat(40), "3".repeat(40)],
            };
        let repo = crate::domain::repo::Repo::at(d.path());
        let mut budget = ProvenanceReadBudget::new();
        assert_eq!(
            validate_merge_source_events_batch(
                &repo,
                vec![candidate(complete_parent.clone(), source_meta.clone())],
                &mut budget,
            )
            .len(),
            1,
            "a source event that exists and is self-consistent in identity, own hash and event id passes"
        );

        let forged_meta = crate::domain::meta::Meta::new(
            format!("agit-{}", "8".repeat(40)),
            source_meta.runtime.clone(),
            "/forged".into(),
        );
        let mut budget = ProvenanceReadBudget::new();
        assert!(
            validate_merge_source_events_batch(
                &repo,
                vec![candidate(complete_parent.clone(), forged_meta)],
                &mut budget,
            )
            .is_empty(),
            "a self-consistent event cannot vouch for a different meta identity"
        );

        std::fs::remove_file(&event_path).unwrap();
        git(&["add", "-A"]);
        git(&[
            "commit",
            "-qm",
            "remove event but retain dangling LOG identity",
        ]);
        let dangling_parent = git(&["rev-parse", "HEAD"]);
        let mut budget = ProvenanceReadBudget::new();
        assert!(
            validate_merge_source_events_batch(
                &repo,
                vec![candidate(dangling_parent, source_meta.clone())],
                &mut budget,
            )
            .is_empty(),
            "a dangling event id in the LOG establishes no source identity"
        );

        reset_trusted_provenance_git_process_count();
        let mut no_budget = ProvenanceReadBudget {
            remaining: 0,
            exhausted: false,
        };
        assert!(
            validate_merge_source_events_batch(
                &repo,
                vec![candidate(complete_parent.clone(), source_meta.clone())],
                &mut no_budget,
            )
            .is_empty(),
            "an insufficient cumulative budget grants less trust"
        );
        assert_eq!(
            trusted_provenance_payload_bytes(),
            0,
            "the budget is decided before any event body is read"
        );

        assert!(
            validate_source_sequence(b"", crate::domain::meta::LayoutVersion::V1, &source_meta,)
                .is_none(),
            "an empty source LOG establishes no identity"
        );
    }

    /// As the number of merges grows, provenance validation batches at a fixed size; a repeated
    /// LOG or event OID is read once, and the real total of bodies equals the set of unique
    /// objects rather than merely making the process count look steady.
    #[test]
    fn merge_provenance_reads_are_batched() {
        let (one_processes, one_bytes, one_expected_bytes) = merge_provenance_metrics(1);
        let (many_processes, many_bytes, many_expected_bytes) =
            merge_provenance_metrics(MERGE_META_BATCH / 8);
        assert_eq!(
            one_processes, 11,
            "within one batch, tip/meta/LOG/source-event/marker all use fixed batching"
        );
        assert_eq!(
            many_processes,
            one_processes,
            "growing from one merge to {} within the same batch adds no Git processes",
            MERGE_META_BATCH / 8
        );
        assert_eq!(
            one_bytes, one_expected_bytes,
            "a single merge reads only the unique objects"
        );
        assert_eq!(
            many_bytes, many_expected_bytes,
            "repeated parent LOGs, source LOGs and source events are deduplicated by OID"
        );
        assert!(
            many_bytes <= TRUSTED_PROVENANCE_MAX_BYTES,
            "reading provenance bodies stays inside the cumulative budget"
        );
    }

    fn merge_provenance_metrics(merges: usize) -> (usize, u64, u64) {
        let d = repo_dir();
        let git = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["branch", "-M", "main"]);

        let write_snapshot = |meta: &crate::domain::meta::Meta, log: &str| {
            crate::domain::meta::write(d.path(), meta).unwrap();
            for (rel, bytes) in crate::domain::storage::snapshot_files(log, log).unwrap() {
                let path = d.path().join(rel);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, bytes).unwrap();
            }
        };

        let target_meta = crate::domain::meta::Meta::new(
            format!("agit-{INTERNAL_HEX}"),
            "codex".into(),
            "/target".into(),
        );
        let target_envelope = crate::domain::transcript::Envelope {
            source: target_meta.runtime.clone(),
            session_id: target_meta.session.clone(),
            content: serde_json::json!({"message": "target root"}),
            object_hash: String::new(),
        };
        let target_envelope = crate::domain::transcript::Envelope {
            object_hash: crate::domain::transcript::object_hash(&target_envelope.content),
            ..target_envelope
        };
        let target_line = crate::domain::storage::envelope_line(&target_envelope);
        write_snapshot(&target_meta, &target_line);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "target root"]);
        let target_head = git(&["rev-parse", "HEAD"]);

        let source_meta = crate::domain::meta::Meta::new(
            format!("agit-{UNTRUSTED_HEX}"),
            "codex".into(),
            "/source".into(),
        );
        let source_envelope = crate::domain::transcript::Envelope {
            source: source_meta.runtime.clone(),
            session_id: source_meta.session.clone(),
            content: serde_json::json!({"message": "sourcegraph-access-token source turn"}),
            object_hash: String::new(),
        };
        let source_envelope = crate::domain::transcript::Envelope {
            object_hash: crate::domain::transcript::object_hash(&source_envelope.content),
            ..source_envelope
        };
        let source_line = crate::domain::storage::envelope_line(&source_envelope);
        write_snapshot(&source_meta, &source_line);
        git(&["add", "-A"]);
        let source_tree = git(&["write-tree"]);
        let source_commit = git(&["commit-tree", &source_tree, "-m", "source root"]);

        git(&["reset", "-q", "--hard", &target_head]);
        let mut merge_meta = target_meta.clone();
        merge_meta.kind = crate::domain::meta::Kind::Merge;
        merge_meta.milestone = Some("merge source".into());
        let target_synthetic = |content: serde_json::Value| {
            let envelope = crate::domain::transcript::Envelope {
                source: target_meta.runtime.clone(),
                session_id: target_meta.session.clone(),
                object_hash: crate::domain::transcript::object_hash(&content),
                content,
            };
            crate::domain::storage::envelope_line(&envelope)
        };
        let mut head = target_head.clone();
        let mut current_log = target_line;
        let sequence_line_bytes = crate::domain::meta::EVENT_ID_HEX_LEN + 1;
        let meta_bytes = |meta: &crate::domain::meta::Meta| {
            format!("{}\n", serde_json::to_string_pretty(meta).unwrap()).len()
        };
        // merge meta is read once as the branch tip and once in the merge-meta batch. Within that
        // batch all repeated meta OIDs are deduplicated.
        let mut expected_payload_bytes = (sequence_line_bytes * 2
            + source_line.len()
            + meta_bytes(&target_meta)
            + meta_bytes(&source_meta)
            + meta_bytes(&merge_meta) * 2) as u64;
        for i in 0..merges {
            let source_name = format!("source/{i}");
            let start = target_synthetic(serde_json::json!({
                "type": "system",
                "subtype": "agit:__merge_start__",
                "source": source_name,
            }));
            let summary = target_synthetic(serde_json::json!({
                "type": "user",
                "agit": "merge_summary",
                "message": {"role": "user", "content": format!("merge {i}")},
            }));
            let end = target_synthetic(serde_json::json!({
                "type": "system",
                "subtype": "agit:__merge_end__",
                "source": source_name,
            }));
            current_log = format!("{current_log}{start}{source_line}{summary}{end}");
            expected_payload_bytes = expected_payload_bytes
                .checked_add(((1 + (i + 1) * 4) * sequence_line_bytes) as u64)
                .and_then(|bytes| {
                    bytes.checked_add((start.len() + summary.len() + end.len()) as u64)
                })
                .unwrap();
            write_snapshot(&merge_meta, &current_log);
            git(&["add", "-A"]);
            let merge_tree = git(&["write-tree"]);
            head = git(&[
                "commit-tree",
                &merge_tree,
                "-p",
                &head,
                "-p",
                &source_commit,
                "-m",
                &format!("agit: merge source {i}"),
            ]);
        }
        git(&["update-ref", "refs/heads/main", &head, &target_head]);
        git(&["reset", "-q", "--hard", "main"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        reset_trusted_provenance_git_process_count();
        let trusted = branch_trusted_envelope_identities(&repo, &["main".into()]);
        assert!(
            trusted
                .get(&source_meta.session)
                .is_some_and(|runtimes| runtimes.contains(&source_meta.runtime)),
            "a valid source identity is still adopted after batched reads"
        );
        (
            trusted_provenance_git_process_count(),
            trusted_provenance_payload_bytes(),
            expected_payload_bytes,
        )
    }

    /// The reachable-object pass **cannot filter the scan surface by path label**.
    ///
    /// `rev-list --objects` labels a blob **once** (it deduplicates by oid itself and keeps only
    /// the first path seen during traversal when several references share the content). So "skip
    /// this blob when its label lands on an excluded path" drops **the in-surface copy** along
    /// with it — and the dropped bytes leave with the push all the same. Which path wins the label
    /// depends purely on tree traversal order, which means this gate's verdict depends on the
    /// lexicographic order of file names.
    #[test]
    fn a_blob_shared_with_an_excluded_path_is_still_scanned() {
        let (d, run) = repo_with_git();
        // Identical bytes at two paths: one excluded (session/meta.json), one in the surface.
        let payload = format!("deploy key: {AWS}\n");
        std::fs::create_dir_all(d.path().join("session")).unwrap();
        std::fs::create_dir_all(d.path().join("skills")).unwrap();
        std::fs::write(d.path().join(crate::domain::meta::FILE), &payload).unwrap();
        std::fs::write(d.path().join("skills/leak.md"), &payload).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);
        // Both are deleted from the working tree, so the working-tree pass scans nothing and
        // only the blob in history remains.
        std::fs::remove_file(d.path().join(crate::domain::meta::FILE)).unwrap();
        std::fs::remove_file(d.path().join("skills/leak.md")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "another entirely ordinary commit"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        // Precondition 1: neither copy is in the working tree.
        assert!(
            !d.path().join("skills/leak.md").exists()
                && !d.path().join(crate::domain::meta::FILE).exists(),
            "precondition fails: a copy survives in the working tree, so this case tests the working-tree pass"
        );
        // Precondition 2: the **only** label `rev-list --objects` gives this blob is the
        // excluded path. If that stops holding (traversal order changed), the assertion below
        // becomes vacuously true.
        let objects = repo
            .git_opt(&["rev-list", "--objects", "--branches"])
            .expect("rev-list is readable");
        assert!(
            objects
                .lines()
                .any(|l| l.ends_with(&format!(" {}", crate::domain::meta::FILE))),
            "precondition fails: rev-list did not label this blob as meta.json: {objects}"
        );
        assert!(
            !objects.lines().any(|l| l.ends_with(" skills/leak.md")),
            "precondition fails: the in-surface path also got a label, so filtering by label would miss nothing anyway: {objects}"
        );

        let hits = scan_agent_repo(&repo, &ScanPlan::full()).unwrap().hits;
        assert!(
            hits.iter()
                .any(|h| h.rule == "aws-access-token" && h.source == Source::BlobObject),
            "this content also sits at skills/leak.md, and which path the label lands on must not change the verdict: {hits:?}"
        );
    }

    /// A blob that **looks like a canonical meta** is scanned all the same.
    ///
    /// This pins that an "exclude by content" test must not exist. Fixing "filtering by rev-list's
    /// path label misses the in-surface copy" by moving the test from path to content (bytes
    /// compared to the canonical serialization) widens an exclusion that belonged to one path into
    /// a **global** one — and those bytes can be constructed on purpose: put the secret in
    /// `milestone`, save it as `skills/deploy/config.json`, and the scan says clean while push
    /// sends it.
    ///
    /// The exclusion is also load-bearing for nothing: a canonical meta matches `[]` under the
    /// current rule set (observed). So the reachable-object pass excludes nothing.
    #[test]
    fn a_meta_shaped_blob_outside_the_session_path_is_still_scanned() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);

        // A **canonical** meta document with the secret hidden in milestone — a byte round-trip
        // test calls it a meta.
        let mut m = crate::domain::meta::Meta::new(
            format!("agit-{}", "b".repeat(40)),
            "claude-code".into(),
            "/p".into(),
        );
        m.milestone = Some(format!("deploy {AWS}"));
        let canonical = crate::domain::meta::to_text(&m).unwrap();

        // But it lies at an ordinary path **inside the publish surface**, not at
        // session/meta.json.
        std::fs::create_dir_all(d.path().join("skills/deploy")).unwrap();
        std::fs::write(d.path().join("skills/deploy/config.json"), &canonical).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "add"]);
        // Deleted from the working tree: only the blob in history remains, isolating the
        // reachable-object path.
        std::fs::remove_file(d.path().join("skills/deploy/config.json")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-qm", "remove"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = scan_agent_repo(&repo, &ScanPlan::full()).unwrap().hits;
        assert!(
            hits.iter()
                .any(|h| h.rule == "aws-access-token" && h.source == Source::BlobObject),
            "looking like a meta is not an exemption — these bytes lie under skills/ and leave with the push: {hits:?}"
        );
    }

    /// "It parses as `Meta`" **cannot** be the test: every field of `Meta` has `serde(default)`
    /// and unknown fields are ignored, so **any** JSON object parses (`{}` included). Using it as
    /// the test removes every `.json` in the repo from the scan surface, a far larger hole than
    /// the one it replaces.
    ///
    /// The test is "these bytes are exactly what the meta writer would emit", so all of the
    /// following stay in the surface.
    #[test]
    fn a_json_file_that_merely_parses_as_meta_is_still_scanned() {
        for (rel, body) in [
            // A session field alone is not a meta document.
            (
                "memory/notes.json",
                format!("{{\"session\":\"whatever\",\"key\":\"{AWS}\"}}\n"),
            ),
            // Every field name is one Meta knows, but the shape is not what canonical
            // serialization produces.
            (
                "skills/cfg.json",
                format!("{{\"cwd\":\"/w\",\"milestone\":\"{AWS}\"}}\n"),
            ),
            // Canonical meta bytes plus one extra field: the extra field is dropped on parse, so
            // the round trip does not match.
            (
                "AGENTS.json",
                format!(
                    "{{\n  \"line\": \"session\",\n  \"kind\": \"turn\",\n  \"x\": \"{AWS}\"\n}}\n"
                ),
            ),
        ] {
            let (d, run) = repo_with_git();
            let p = d.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, &body).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", "an entirely ordinary commit"]);
            std::fs::remove_file(&p).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", "another entirely ordinary commit"]);

            let repo = crate::domain::repo::Repo::at(d.path());
            let hits = scan_agent_repo(&repo, &ScanPlan::full()).unwrap().hits;
            assert!(
                hits.iter()
                    .any(|h| h.rule == "aws-access-token" && h.source == Source::BlobObject),
                "{rel} is not a meta document and is scanned as usual: {hits:?}"
            );
        }
    }

    /// The cap acts **at the producing layer**, not as a `truncate` once the list exists.
    ///
    /// Hits are a function of the input, not a subset: in text made of one repeated token the hit
    /// count is on the order of the length. Truncating the `Vec` in the caller achieves nothing —
    /// the allocation over the line already happened before the truncation. Observed on the
    /// server: 64 MiB of dense input produces about 3.2 million entries and about 600 MB, while
    /// the report shows a few dozen.
    ///
    /// This pins two things: **the count really stops at the bound**, and **the duration drops
    /// with it** (the second is the evidence that it really stopped looking — a truncation alone
    /// leaves the duration unchanged).
    #[test]
    fn the_cap_stops_production_not_just_the_report() {
        let dense = format!("{AWS} ").repeat(50_000);
        let uncapped = scan_text_with(&dense, &none(), Policy::STRICT);
        assert!(
            uncapped.len() > 10_000,
            "precondition: the unbounded output far exceeds the cap, got {}",
            uncapped.len()
        );

        let t0 = std::time::Instant::now();
        let capped = scan_text_capped(&dense, &none(), Policy::STRICT, 50);
        let capped_ms = t0.elapsed();
        assert_eq!(capped.hits.len(), 50, "the count stops at the bound");

        let t1 = std::time::Instant::now();
        let _ = scan_text_with(&dense, &none(), Policy::STRICT);
        let uncapped_ms = t1.elapsed();
        assert!(
            capped_ms * 4 < uncapped_ms,
            "the cap makes the scan really stop early instead of just reporting fewer: capped {capped_ms:?} vs unbounded {uncapped_ms:?}"
        );
    }

    /// At `cap >= 1` the verdict is unchanged: a hit is still reported, and the bound presses on
    /// the count, not on "is there any".
    #[test]
    fn the_cap_never_flips_the_verdict() {
        let text = format!("harmless line\nkey: {AWS}\nmore harmless text\n");
        assert!(
            !scan_text_capped(&text, &none(), Policy::STRICT, 1)
                .hits
                .is_empty(),
            "even at cap=1 that hit is reported — the bound presses on the count, not the verdict"
        );
        assert!(
            scan_text_capped("nothing to see here\n", &none(), Policy::STRICT, 50)
                .hits
                .is_empty(),
            "clean text gains no hit out of nowhere from being bounded"
        );
    }

    /// The peak of a **single** dense carrier is bounded too, not only the final list.
    ///
    /// # Which stretch this pins
    ///
    /// `HitCollector` caps the final list alone. But scanning first materializes every hit of one
    /// carrier into a `Vec<Hit>` and only then hands it to the collector — a large file matching
    /// on every line has already produced `Raw`s / `Hit`s on the order of its line count before
    /// `extend` takes over, each carrying its own `String`. Capping the final list does not stop
    /// that stretch.
    ///
    /// So besides "the whole report is bounded", **the engine layer** is pinned directly:
    /// whatever cap it is given, it stops exactly there, however far the input exceeds it.
    #[test]
    fn a_single_dense_carrier_does_not_materialise_everything() {
        let d = repo_dir();
        let p = d.path().join("memory/notes.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        // One key per line, at ten times the cap in lines.
        let dense: String = (0..(MAX_REPORTED_HITS * 10))
            .map(|i| format!("step {i} {AWS}\n"))
            .collect();
        std::fs::write(&p, &dense).unwrap();

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = scan_agent_repo(&repo, &ScanPlan::full())
            .expect("the repo is healthy")
            .hits;
        assert!(!hits.is_empty(), "a hit that exists is reported");
        assert!(
            hits.len() <= MAX_REPORTED_HITS,
            "the whole report is bounded, got {}",
            hits.len()
        );

        // The engine layer: it stops at whatever bound it is given, rather than producing
        // everything and truncating.
        for cap in [1usize, 7, MAX_REPORTED_HITS] {
            assert_eq!(
                scan_text_capped(&dense, &none(), Policy::CLIENT, cap)
                    .hits
                    .len(),
                cap,
                "at cap={cap} the output stops exactly at the bound"
            );
        }
    }

    /// Being bounded **must not** flip the verdict from dirty to clean — not even when
    /// everything ahead is allowed by the allowlist.
    ///
    /// # The flip this pins
    ///
    /// [`Policy::CLIENT`] honours the allowlist and the inline pragma. With the bound on the
    /// **raw output** and the filter left behind it, this happens: the allowlist swallows the
    /// first `cap` hits, an empty list comes back, and a real hit sits right behind them — a bound
    /// added purely to save memory flipping the verdict from "dirty" to "clean". That is fail
    /// open, exactly what this gate must not do.
    ///
    /// So the filter moved into the producing loop and counts the hits **that pass it**.
    /// `cap = 1` is the harshest slot.
    #[test]
    fn the_bound_counts_reportable_hits_not_raw_ones() {
        // The leading ones are all distinct and all written into the allowlist; the real hit
        // sits **behind** them.
        //
        // These are rotations of the `AWS` string: an AWS key's alphabet is base32 (`[A-Z2-7]`)
        // and the rule also carries an entropy threshold — a casually invented string is filtered
        // out silently, and this test would then exercise a path that never fires.
        const WAIVED: [&str; 3] = [
            "AKIA6VW3JH4X7QZ2M5RT",
            "AKIAM5RT6VW3JH4X7QZ2",
            "AKIAZ2M5RT6VW3JH4X7Q",
        ];
        let mut text = String::new();
        for (i, k) in WAIVED.iter().enumerate() {
            text.push_str(&format!("waived {i} {k}\n"));
        }
        text.push_str(&format!("the real one {AWS}\n"));

        assert_eq!(
            scan_text(&text, &none()).len(),
            WAIVED.len() + 1,
            "precondition: each fake key matches on its own, or the flip is not under test"
        );
        let allow: HashSet<String> = WAIVED.iter().map(|s| (*s).to_string()).collect();

        for cap in 1..=(WAIVED.len() + 1) {
            let hits = scan_text_capped(&text, &allow, Policy::CLIENT, cap).hits;
            assert_eq!(
                hits.len(),
                1,
                "cap={cap}: the allowed ones cost no budget and the real hit is reported"
            );
            assert_eq!(
                hits[0].redacted,
                redact(AWS),
                "what is reported is the one that was not allowed"
            );
        }
    }

    /// A commit's author and committer identity are in the scan surface too, not just the
    /// message.
    ///
    /// The identity lines are fully controlled by whoever pushes (`git -c user.name=…`) and are
    /// published with every clone — `git log`'s first line is exactly that. Scanning only the
    /// message makes `agit scan` report clean on a repo that really leaks, and the server-side
    /// gate cannot see it either (both use the same set of fields).
    #[test]
    fn a_secret_in_the_commit_identity_is_scanned() {
        let d = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        // The secret is only in the author name; the message is clean.
        g(&["config", "user.name", AWS]);
        g(&["config", "commit.gpgsign", "false"]);
        g(&["commit", "-q", "--allow-empty", "-m", "an ordinary message"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            commit_message_hits(&repo, &none()).expect("the repo is healthy, so the scan succeeds");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the secret in the author name must be scanned: {hits:?}"
        );
    }

    /// **Any header** of a commit object is in the scan surface, not only the ones on some list
    /// of fields.
    ///
    /// # Triggering it needs no malformed object
    ///
    /// ```text
    /// git -c i18n.commitEncoding=<secret> commit --allow-empty -m clean
    /// ```
    ///
    /// One **ordinary git command** writes the line `encoding <secret>`. It is public with every
    /// clone and readable verbatim by `git cat-file commit`, yet it is not in the output of
    /// `%an/%ae/%cn/%ce/%B` — so `agit scan` reports clean and the server-side gate reports clean
    /// too (both use the same list).
    ///
    /// So what this pins is not "the encoding header" but **not relying on a list at all**.
    #[test]
    fn a_secret_in_any_commit_header_is_scanned() {
        let d = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        // Author, committer and message are all clean; the secret is only on the `encoding`
        // line.
        g(&[
            "-c",
            &format!("i18n.commitEncoding={AWS}"),
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "a perfectly ordinary message",
        ]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            commit_message_hits(&repo, &none()).expect("the repo is healthy, so the scan succeeds");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the secret on the `encoding` line must be scanned — an ordinary git command writes it: {hits:?}"
        );
    }

    /// `refs/replace/*` cannot hide the real object.
    ///
    /// # Why this is a hole and not "the client being conservative"
    ///
    /// git honours `refs/replace/*` by default: reading a replaced object yields the replacement.
    /// And `agit push`'s refspecs are `refs/heads/<branch>` and `refs/tags/*`, **not**
    /// `refs/replace/*` — the replace ref never leaves the local machine. So the local gate reads
    /// the replacement, says clean and allows, while what is pushed is **the real object** and the
    /// secret travels the network intact onto the hub.
    ///
    /// And this is more than an attack surface: `git filter-repo` and `git replace --graft` both
    /// leave `refs/replace/*` behind, so a user who just cleaned a secret out with filter-repo
    /// walks into this inconsistency.
    ///
    /// The fix is `--no-replace-objects` added uniformly in the layer where
    /// [`crate::domain::repo::Repo`] starts git, not at each call site.
    #[test]
    fn a_replaced_object_does_not_hide_the_real_content() {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // The real object: a secret in the message, right on main.
        std::fs::write(d.path().join("real.txt"), "the real payload\n").unwrap();
        run(&["add", "real.txt"]);
        run(&["commit", "-q", "-m", &format!("leak {AWS}")]);
        let dirty = run(&["rev-parse", "HEAD"]);
        let dirty_tree = run(&["rev-parse", "HEAD^{tree}"]);
        // The replacement: **a different tree**, a clean message, on no branch — push can never
        // send it.
        //
        // Sharing one tree between replacement and real object would leave this test covering
        // only the message half: with the same tree, whichever tree the scanner reads gives the
        // same file payload, so "does it read the real object for files too" is never asked. The
        // trees have to differ.
        std::fs::write(d.path().join("real.txt"), "a decoy payload\n").unwrap();
        run(&["add", "real.txt"]);
        let clean_tree = run(&["write-tree"]);
        // Restore the index, keeping only that replacement tree.
        run(&["reset", "-q", "--mixed", &dirty]);
        let clean = run(&[
            "commit-tree",
            &clean_tree,
            "-m",
            "a perfectly ordinary message",
        ]);
        run(&["replace", &dirty, &clean]);

        // Precondition: with replace in effect, reading that OID really yields the
        // replacement's body. The whole point of this test is that the scanner **must not** see
        // this.
        assert!(
            run(&["cat-file", "commit", &dirty]).contains("a perfectly ordinary message"),
            "precondition fails: git did not honour refs/replace, so this test tests nothing"
        );
        // Precondition: the two trees really differ. Identical, the assertion above still passes
        // while this case degrades to covering only the message.
        assert_ne!(
            dirty_tree, clean_tree,
            "precondition fails: replacement and real object share one tree, leaving the file payload half uncovered"
        );

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            commit_message_hits(&repo, &none()).expect("the repo is healthy, so the scan succeeds");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the scan reads the real object: the real object is what gets pushed, and allowing on the replacement hands the secret to the network: {hits:?}"
        );
    }

    /// The replacement's **tree** cannot hide the real file payload.
    ///
    /// # How this differs from the case above
    ///
    /// The case above pins the commit **body**. File payloads go through another gate: the
    /// pre-push scan reads the **working tree**, whose content comes from a checkout, and
    /// checkout honours `refs/replace/*`. So once a clean replacement tree is materialized into
    /// the working tree by `git reset --hard`, the working tree holds not one byte of the secret
    /// — while push sends the real commit, the remote receives a tree with the secret intact, and
    /// the replacement object never left the local machine.
    ///
    /// So "give the object-reading paths `--no-replace-objects`" is not enough: the outbound scan
    /// has to scan **the trees of the refs about to be pushed** directly, not only the working
    /// tree, which is a projection in the replacement's shape.
    #[test]
    fn a_replaced_tree_does_not_hide_the_real_file_payload() {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // The real object: the secret is in a **file**, and the message is entirely clean.
        std::fs::write(d.path().join("payload.txt"), format!("key = {AWS}\n")).unwrap();
        run(&["add", "payload.txt"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);
        let dirty = run(&["rev-parse", "HEAD"]);
        // The replacement: a clean tree and a clean message.
        std::fs::write(d.path().join("payload.txt"), "key = nothing to see here\n").unwrap();
        run(&["add", "payload.txt"]);
        let clean_tree = run(&["write-tree"]);
        let clean = run(&[
            "commit-tree",
            &clean_tree,
            "-m",
            "an entirely ordinary commit",
        ]);
        run(&["replace", &dirty, &clean]);
        // An ordinary `git reset --hard` materializes the replacement into the working tree —
        // no special technique needed.
        run(&["reset", "-q", "--hard", "HEAD"]);

        // Precondition 1: the working tree really holds not one byte of the secret. Otherwise
        // this test tests something else.
        let on_disk = std::fs::read_to_string(d.path().join("payload.txt")).unwrap();
        assert!(
            !on_disk.contains(AWS),
            "precondition fails: the secret survives in the working tree, so replace did not fool the checkout: {on_disk:?}"
        );
        // Precondition 2: the secret is still in the real tree, which is exactly the bytes push
        // sends.
        assert!(
            run(&[
                "--no-replace-objects",
                "show",
                &format!("{dirty}:payload.txt")
            ])
            .contains(AWS),
            "precondition fails: the real tree holds no secret, so this test tests nothing"
        );

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = scan_agent_repo(&repo, &ScanPlan::full())
            .expect("the repo is healthy, so the scan succeeds")
            .hits;
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the outbound scan sees the file payload in the real tree: the working tree has the replacement's shape, and the real object is what gets pushed: {hits:?}"
        );
    }

    /// `git replace --graft` swaps the parent pointers, and the scanner's **enumeration** must
    /// not be fooled by it either.
    ///
    /// The case above pins whose body gets read; this one is a notch harder: graft changes the
    /// parent pointers, and one `git replace --graft <tip>` (with no parent given) collapses
    /// `rev-list --branches` from five commits to one. The commit with the secret deep in the
    /// history then **never entered the scan surface at all** — not scanned and unreported, but
    /// never listed. And push sends the real commit graph, all five of them.
    #[test]
    fn a_grafted_history_does_not_hide_the_commits_it_cuts_off() {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // The secret sits **deep** in the history under a few entirely ordinary commits.
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("deep leak {AWS}"),
        ]);
        for i in 0..4 {
            run(&[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("ordinary {i}"),
            ]);
        }
        let tip = run(&["rev-parse", "HEAD"]);
        // No parent given = the tip becomes an orphan and the whole history vanishes from the
        // graph.
        run(&["replace", "--graft", &tip]);

        // Precondition: the graft really cut the history. Otherwise this test runs empty.
        assert_eq!(
            run(&["rev-list", "--branches"]).lines().count(),
            1,
            "precondition fails: the graft did not cut the history, so this test tests nothing"
        );

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            commit_message_hits(&repo, &none()).expect("the repo is healthy, so the scan succeeds");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "enumeration walks the real commit graph: the stretch the graft cut off is pushed all the same: {hits:?}"
        );
    }

    /// Unreadable history **errors**; it does not report clean.
    ///
    /// Handing back an empty `Vec` when `git_opt` returns `None` makes `agit scan` say "clean" as
    /// soon as git errors — a scanner's worst failure. `rev-list` with `-n` and no rev argument
    /// errors outright, and swallowing that takes the whole commit scan out silently.
    ///
    /// "There is no commit yet" is a different thing and a legitimate state, see
    /// [`an_empty_repo_has_no_history_to_scan`].
    #[test]
    fn a_repo_whose_history_cannot_be_read_is_not_reported_clean() {
        let d = tempfile::tempdir().unwrap();
        let g = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        g(&["commit", "-q", "--allow-empty", "-m", "one"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        // HEAD exists, so the real scan path runs; the selector points at a ref that does not
        // exist, so git fails.
        assert!(
            message_hits(&repo, &["definitely-not-a-ref"], &none()).is_err(),
            "unreadable has to error — reporting clean calls something never looked at 'looked at, and fine'"
        );
    }

    /// The reported line number and label go together: line N of `commit object <sha>` is
    /// countable in `git cat-file commit <sha>`.
    ///
    /// # Why this is pinned separately
    ///
    /// The scanned text is the raw object body, which carries a whole header block that `git
    /// log`'s message does not. So the same hit has **different** line numbers in the two places:
    /// a secret on line 3 of the message is somewhere in the teens inside the object. With the
    /// label saying just "commit", the user counts lines in `git log`, they do not line up, and
    /// they conclude the report is broken — pointing at the wrong place and missing a hit are the
    /// same class of harm.
    ///
    /// The two numbers are made deliberately different here, which is what reveals which one is
    /// reported.
    #[test]
    fn the_reported_line_and_label_point_at_the_object() {
        let d = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        g(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("line one\nline two\nkey {AWS} here\nline four"),
        ]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = commit_message_hits(&repo, &none()).expect("the repo is healthy");
        let hit = hits
            .iter()
            .find(|h| h.rule == "aws-access-token")
            .expect("the secret on line 3 of the message must be scanned");

        let file = hit.file.as_deref().unwrap_or("");
        assert!(
            file.starts_with("commit object "),
            "the label says this is an object, not a message: {file:?}"
        );

        let sha = run(&["rev-parse", "HEAD"]).trim().to_string();
        let raw = run(&["cat-file", "commit", &sha]);
        let line = raw
            .lines()
            .nth(hit.line - 1)
            .unwrap_or_else(|| panic!("line {} is past the object's line count", hit.line));
        assert!(
            line.contains(AWS),
            "line {} was reported, but that line of `git cat-file commit` is {line:?}",
            hit.line
        );

        assert_ne!(
            hit.line, 3,
            "in the message it is line 3; reporting 3 means a projection is scanned, not the object"
        );
    }

    /// A NUL in a commit body **does not hide** what follows it.
    ///
    /// # The same hole as the tag case
    ///
    /// Framing the commit path by `rev-list --header` + NUL breaks because a commit body may
    /// contain a NUL: `git hash-object -t commit --literally` produces one and `git push` accepts
    /// it. The record is then truncated at the first NUL, the suffix (possibly holding a secret)
    /// escapes the scan surface, **and nothing errors** — a scanner's worst failure. (Harsher in
    /// practice: `rev-list --header` itself cuts the body at the NUL, and not one byte of what
    /// follows appears in its output.)
    ///
    /// The tag path frames by the **byte count** of `%(raw:size)`, and the commit path now
    /// matches it: `rev-list` gives only OIDs (hex, unambiguous to split by line), and the body is
    /// taken by the length `cat-file --batch` reports.
    ///
    /// The test is "frame by length", not "frame by separator": a body may hold any byte,
    /// separators included.
    #[test]
    fn a_nul_byte_in_a_commit_body_does_not_hide_the_rest() {
        use std::io::Write as _;
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        // Only `--literally` writes a commit containing a NUL (stock git's `fsck` calls it
        // `nulInCommit`, and it pushes all the same).
        let write_object = |kind: &str, content: &[u8]| -> String {
            let mut c = std::process::Command::new("git")
                .args(["hash-object", "-t", kind, "--literally", "-w", "--stdin"])
                .current_dir(d.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.as_mut().unwrap().write_all(content).unwrap();
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success(), "git hash-object -t {kind}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        // An empty tree: the content does not matter, the secret is in the body.
        let tree = write_object("tree", b"");
        let mut body = Vec::new();
        body.extend_from_slice(format!("tree {tree}\n").as_bytes());
        body.extend_from_slice(b"author t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\n");
        // Everything **before** the NUL is clean; the secret is **after** it.
        body.extend_from_slice(b"a perfectly ordinary summary\x00");
        body.extend_from_slice(format!("key {AWS}\n").as_bytes());
        let sha = write_object("commit", &body);
        run(&["update-ref", "HEAD", &sha]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = commit_message_hits(&repo, &none()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the stretch after the NUL leaves with the push all the same and must be scanned: {hits:?}"
        );
    }

    /// `agit scan` and `agit push` see **the same** commit range.
    ///
    /// `scan.rs` positions itself as "a standalone entry point to push's scan that CI can run on
    /// its own". That position requires the two paths to have **the same** scan surface — a scan
    /// that covers less lets CI go green on a repo push will stop, and the user cannot reproduce
    /// the refusal locally.
    ///
    /// The secret is deliberately placed **deep in the history** (not on the tip): an
    /// implementation that looks at one message cannot see it, while push can.
    #[test]
    fn the_scan_path_and_push_see_the_same_commit_range() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("deep: key {AWS}"),
        ]);
        for i in 0..4 {
            run(&["commit", "-q", "--allow-empty", "-m", &format!("later {i}")]);
        }

        let repo = crate::domain::repo::Repo::at(d.path());

        let via_scan = scan_path_hits(&repo);
        let via_push = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");

        assert!(
            via_push.hits.iter().any(|h| h.rule == "aws-access-token"),
            "precondition: the push path sees the one deep in the history"
        );
        assert!(
            via_scan.iter().any(|h| h.rule == "aws-access-token"),
            "`agit scan` sees the same one, or CI goes green on a repo push will stop: {via_scan:?}"
        );
    }

    /// An annotated tag's message is in the scan surface.
    ///
    /// A tag object is an object of its own and the commit graph cannot see it. Without this, the
    /// server refuses while the local `agit scan` says clean — and the user cannot reproduce the
    /// refusal.
    #[test]
    fn a_secret_in_an_annotated_tag_message_is_scanned() {
        let (d, _) = tagged_repo(&["-m", &format!("cut release; key {AWS}")], "t");
        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the secret in a tag message must be scanned: {hits:?}"
        );
    }

    /// A tag object's **header** is in the scan surface too, not just the message.
    ///
    /// `git -c user.name=<secret> tag -a v1 -m release` writes it into the `tagger` line with one
    /// stock git command, message entirely clean. Same class as `encoding` on the commit side.
    #[test]
    fn a_secret_in_the_tag_header_is_scanned() {
        let (d, _) = tagged_repo(&["-m", "release"], AWS);
        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the secret on the tagger line must be scanned: {hits:?}"
        );
    }

    /// A tag's label and line number go together too — the same contract as the commit case.
    #[test]
    fn the_tag_hit_points_at_the_object() {
        let (d, oid) = tagged_repo(&["-m", &format!("line one\nline two\nkey {AWS} here")], "t");
        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        let hit = hits
            .iter()
            .find(|h| h.rule == "aws-access-token")
            .expect("it must be scanned");

        let file = hit.file.as_deref().unwrap_or("");
        assert!(
            file.starts_with("tag object "),
            "the label says this is a tag object: {file:?}"
        );

        let raw = std::process::Command::new("git")
            .args(["cat-file", "tag", &oid])
            .current_dir(d.path())
            .output()
            .unwrap();
        let raw = String::from_utf8_lossy(&raw.stdout);
        let line = raw
            .lines()
            .nth(hit.line - 1)
            .unwrap_or_else(|| panic!("line {} is past the object's line count", hit.line));
        assert!(
            line.contains(AWS),
            "line {} was reported, but that line of `git cat-file tag` is {line:?}",
            hit.line
        );
    }

    /// A lightweight tag has no object of its own and must not be reported again on this path —
    /// the commit path already scanned it.
    #[test]
    fn a_lightweight_tag_is_not_scanned_twice() {
        let d = repo_dir();
        let g = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        g(&["config", "commit.gpgsign", "false"]);
        g(&["commit", "-q", "--allow-empty", "-m", &format!("key {AWS}")]);
        g(&["tag", "lightweight"]); // No -a: it points straight at the commit.

        let repo = crate::domain::repo::Repo::at(d.path());
        assert!(
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap())
                .expect("the repo is healthy")
                .is_empty(),
            "a lightweight tag has no object of its own, so this path reports nothing"
        );
    }

    /// The `agit scan` path reports secrets inside tag objects too.
    ///
    /// # What this pins
    ///
    /// With the tag scan hanging off the push path alone, the two local gates do not look at the
    /// same scan surface: push scans tags and scan does not, so `agit scan` says clean while
    /// `agit push` stops it — exactly the "the two verdicts disagree" this scan exists to remove,
    /// with push in the server's place. And the tag-specific hint inside `agit scan` then
    /// **could never fire**.
    ///
    /// This module's header says scan and push run the same code, and that sentence has to be
    /// true.
    #[test]
    fn the_scan_path_reports_tag_hits_too() {
        let (d, _) = tagged_repo(&["-m", &format!("cut release; key {AWS}")], "t");
        let repo = crate::domain::repo::Repo::at(d.path());

        let hits = scan_path_hits(&repo);
        assert!(
            hits.iter().any(|h| {
                h.rule == "aws-access-token"
                    && h.file
                        .as_deref()
                        .is_some_and(|f| f.starts_with("tag object "))
            }),
            "the `agit scan` path reports hits inside tags, or it and push do not look at the \
             same scan surface (and scan's tag hint never fires): {hits:?}"
        );
    }

    /// One NUL in a tag body must not let **every tag behind it** escape the scan surface
    /// silently.
    ///
    /// # What this pins
    ///
    /// Splitting one `for-each-ref` output into records by `split('\0')` + `chunks(3)` violates
    /// the very rule its comment states ("never assume there is no NUL"): one NUL in a body shifts
    /// everything by one field, every record after it takes its `kind` from another field, is
    /// treated as a lightweight tag and is **skipped silently**, with neither an error nor a hit.
    ///
    /// And getting a NUL into a tag body only takes `git tag -a x -F <a file containing a NUL>`,
    /// which stock git accepts. So one tag collapses the entire tag scan surface, and collapses it
    /// without a sound.
    ///
    /// Splitting on the raw bytes by the **byte count** `%(raw:size)` reports fixes that; and now
    /// bodies do not come out of `for-each-ref` at all (the enumeration gives only oids and types,
    /// see [`stream_tag_object_oids`]), with `cat-file --batch` framing them by the byte count it
    /// reports itself. What is guarded throughout is one invariant — **framing does not look at
    /// what is inside a body** — and not any one implementation.
    #[test]
    fn a_nul_byte_in_one_tag_does_not_hide_the_others() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);

        // The poisoned tag sorts first (tags are ordered by refname) and its body carries a real
        // NUL.
        let msg = d.path().join("msg");
        std::fs::write(&msg, b"note\x00padding\n").unwrap();
        run(&["tag", "-a", "aaa-with-nul", "-F", msg.to_str().unwrap()]);
        // This later one is the one that has to be scanned.
        run(&["tag", "-a", "zzz-secret", "-m", &format!("key {AWS}")]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "a tag ordered after the poisoned one is scanned all the same: {hits:?}"
        );
    }

    /// With extremely dense hits the amount collected is bounded while **the verdict is
    /// unchanged**.
    ///
    /// Streaming the input bounds only the input side: hits are a function of the input, not a
    /// subset, and a history matching on every line produces `Hit`s on the order of its line
    /// count. Here every commit matches, and the assertion is that the amount collected stops at
    /// the cap rather than following the history length — together with the assertion that it is
    /// **still non-empty**, because one hit is enough to refuse.
    #[test]
    fn a_dense_history_is_bounded_but_still_rejected() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // Every commit matches, in numbers far past the cap.
        for i in 0..(MAX_REPORTED_HITS + 60) {
            run(&[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("{i} key {AWS}"),
            ]);
        }

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = commit_message_hits(&repo, &none()).expect("the repo is healthy");
        assert!(
            !hits.is_empty(),
            "a hit that exists is reported — the cap must not change the verdict"
        );
        assert!(
            hits.len() <= MAX_REPORTED_HITS,
            "the amount collected is bounded, got {}",
            hits.len()
        );
    }

    /// On the object path it is not only the report that is bounded: **the enumeration itself**
    /// stops the moment the collector fills.
    ///
    /// # What this pins
    ///
    /// With the scan surface being every reachable object rather than the tree at each branch tip,
    /// the first publish of a deep-history repo makes `rev-list --objects` list **every**
    /// historical object. Completing the scan surface must not be paid for with an unbounded read,
    /// so every layer on this path is bounded: streaming enumeration, OIDs batched by
    /// [`OBJECT_BATCH`], a single blob's output capped by `remaining()`, and `rev-list` aborted as
    /// soon as the collector fills.
    ///
    /// The object count built here exceeds one batch, so the collector fills **before the
    /// enumeration finishes** — and at that moment [`EnoughHits`] aborts `rev-list`. And "stopped
    /// because it is full" has to stay separable from "git faulted": the `expect` below pins
    /// exactly that, since getting it wrong ends this path in a scan failure (calling a good repo
    /// broken), while swallowing both together calls a fault clean.
    #[test]
    fn the_object_scan_stops_enumerating_once_it_is_full() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // One blob and one hit per file; the count crosses one batch, so the cap is reached
        // before the enumeration finishes.
        //
        // The `{i}` in the body is not decoration: git is content-addressed, so a thousand
        // **identical** files are one blob and `rev-list --objects` prints it once — and this case
        // would never cross a batch.
        let dir = d.path().join("memory");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..(OBJECT_BATCH + 8) {
            std::fs::write(
                dir.join(format!("n{i}.md")),
                format!("note {i}\nkey {AWS}\n"),
            )
            .unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "an entirely ordinary commit"]);

        // Delete them all and commit again: the working tree is clean from here on, the
        // collector **does not** fill during the working-tree pass, and the object pass really
        // runs — which is what this case pins. (The content stays fully reachable.)
        std::fs::remove_dir_all(&dir).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop the notes"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let report = scan_agent_repo(&repo, &ScanPlan::full())
            .expect("stopping early because it is full is not a scan failure");
        assert!(
            !report.hits.is_empty(),
            "a hit that exists is reported — the cap does not change the verdict"
        );
        // Precondition: every hit comes from the **object** pass. One from the working tree means
        // the collector filled inside walkdir, and this case never reached the enumeration it is
        // meant to test.
        assert!(
            report.hits.iter().all(|h| h.source == Source::BlobObject),
            "this case tests the object enumeration pass, so the working tree has to be clean"
        );
        assert!(
            report.hits.len() <= MAX_REPORTED_HITS,
            "the amount collected is bounded, got {}",
            report.hits.len()
        );
        assert!(
            report.truncated,
            "the scan stopped early on the cap, so the report must not claim this is all of it"
        );
    }

    /// The **working-tree file** path is bounded too.
    ///
    /// # What this pins
    ///
    /// A cap that lives on the commit path alone (two hand-written `if`s inside `scan_messages`)
    /// leaves the working-tree path entirely unbounded: with one key per line in
    /// `memory/notes.md`, `Vec<Hit>` grows linearly with the file's line count and each entry
    /// carries three `String`s of its own. The same boundary written in three places, relying on
    /// all three to remember it, is not a contract that holds — the three paths now share one
    /// `HitCollector`.
    ///
    /// Two assertions: **bounded**, and **the verdict unchanged** (non-empty — one hit is enough
    /// to refuse).
    #[test]
    fn a_dense_file_is_bounded_but_still_rejected() {
        let d = repo_dir();
        let p = d.path().join("memory/notes.md");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        // One key per line, at far more lines than the cap.
        let dense: String = (0..(MAX_REPORTED_HITS + 50))
            .map(|i| format!("step {i}: export AWS_ACCESS_KEY_ID={AWS}\n"))
            .collect();
        std::fs::write(&p, dense).unwrap();

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = scan_agent_repo(&repo, &ScanPlan::full())
            .expect("the repo is healthy")
            .hits;
        assert!(
            !hits.is_empty(),
            "a hit that exists is reported — the cap must not change the verdict"
        );
        assert!(
            hits.len() <= MAX_REPORTED_HITS,
            "the working-tree path's collected amount is bounded, got {}",
            hits.len()
        );
    }

    /// The **tag object** path is bounded too.
    ///
    /// agit cuts a version tag every turn, so "a large number of tags" is not a constructed
    /// extreme but the normal case; when every tag's message matches, `hits.push(h)` on this path
    /// has no test in front of it.
    #[test]
    fn many_dirty_tags_are_bounded_but_still_rejected() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        // One key in each tag body, at far more tags than the cap.
        for i in 0..(MAX_REPORTED_HITS + 50) {
            run(&[
                "tag",
                "-a",
                &format!("agit-{i:040}"),
                "-m",
                &format!("release {i}; key {AWS}"),
            ]);
        }

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        assert!(
            !hits.is_empty(),
            "a hit that exists is reported — the cap must not change the verdict"
        );
        assert!(
            hits.len() <= MAX_REPORTED_HITS,
            "the tag path's collected amount is bounded, got {}",
            hits.len()
        );
    }

    /// Truncation has to **be said**.
    ///
    /// # Why the flag itself gets a test
    ///
    /// Once the cap fills, the hit list looks exactly like "this is all of it". The user then
    /// fixes the report entry by entry, pushes again, and is stopped by the same gate — a loop
    /// with no way out. The verdict is unaffected (non-empty is a refusal), but "there is more"
    /// has to reach the layer that prints the report, or the report is lying.
    ///
    /// The reverse is pinned too: with no truncation the flag has to be false, or every scan
    /// carries a "there is more" nobody reads for long.
    #[test]
    fn the_report_says_when_it_is_incomplete() {
        // No truncation: one hit, flag false.
        let clean = repo_dir();
        std::fs::create_dir_all(clean.path().join("memory")).unwrap();
        std::fs::write(
            clean.path().join("memory/notes.md"),
            format!("just the one: {AWS}\n"),
        )
        .unwrap();
        let repo = crate::domain::repo::Repo::at(clean.path());
        let report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            !report.hits.is_empty(),
            "precondition: this one gets scanned"
        );
        assert!(
            !report.truncated,
            "below the cap nothing may claim there is more: {} hits",
            report.hits.len()
        );

        // Truncation: hits far past the cap, flag true.
        let dense = repo_dir();
        std::fs::create_dir_all(dense.path().join("memory")).unwrap();
        let text: String = (0..(MAX_REPORTED_HITS + 50))
            .map(|i| format!("step {i}: export AWS_ACCESS_KEY_ID={AWS}\n"))
            .collect();
        std::fs::write(dense.path().join("memory/notes.md"), text).unwrap();
        let repo = crate::domain::repo::Repo::at(dense.path());
        let report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            report.truncated,
            "once full, the caller has to learn the list is incomplete: {} hits",
            report.hits.len()
        );
    }

    /// A collector that fills **exactly** still reports truncation.
    ///
    /// # This pins the fast path
    ///
    /// `HitCollector::push` sets `truncated` when full, but the caller's fast path
    /// (`if out.is_full() { ... }`) never reaches `push` — what it skips is **the scanning work
    /// behind it**. So when the working tree fills the collector exactly and one tag hit sits
    /// behind it, the report claims `truncated = false`: the user fixes the list entry by entry,
    /// pushes again, and is stopped by the same gate.
    ///
    /// So the flag is set the moment `is_full()` returns true — "I skipped work because I am
    /// full" and "I recorded that the list is incomplete" are one action, and half of it is not a
    /// thing.
    #[test]
    fn an_exactly_full_collector_still_reports_truncation() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);

        // **Exactly** MAX_REPORTED_HITS working-tree hits: one more takes the already-correct
        // `push` path and this test no longer reaches the fast path.
        let text: String = (0..MAX_REPORTED_HITS)
            .map(|i| format!("row {i} {AWS}\n"))
            .collect();
        assert_eq!(
            scan_text(&text, &none()).len(),
            MAX_REPORTED_HITS,
            "precondition: this file produces **exactly** the cap's worth of hits"
        );
        std::fs::create_dir_all(d.path().join("memory")).unwrap();
        std::fs::write(d.path().join("memory/notes.md"), &text).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "base"]);
        // There really is a hit behind it: the list really is incomplete.
        run(&["tag", "-a", "agit-leak", "-m", &format!("key {AWS}")]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let report = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert_eq!(
            report.hits.len(),
            MAX_REPORTED_HITS,
            "precondition: the working-tree pass fills it exactly"
        );
        assert!(
            report.truncated,
            "a hit inside a tag was skipped, so the report must not claim this is all of it"
        );
    }

    /// The carrier in `--json` is **structured** and does not rest on parsing a human-readable
    /// string.
    ///
    /// These three strings are part of the external contract: a JSON consumer tells "working-tree
    /// file" from "commit object" by them. Changing any of them quietly breaks downstream, so they
    /// are pinned here.
    ///
    /// (`run()` in `scan.rs` needs a resolved context and `$AGIT_HOME`, which is awkward to set up
    /// in a unit test; this pins the value it writes into the JSON.)
    #[test]
    fn the_json_source_field_names_the_carrier() {
        assert_eq!(Source::File.as_str(), "file");
        assert_eq!(Source::CommitObject.as_str(), "commit_object");
        assert_eq!(Source::TagObject.as_str(), "tag_object");
    }

    /// A long history must not let one message slip through.
    ///
    /// # Why this is pinned separately
    ///
    /// An `-n 200` on the "not yet pushed" path has two problems: `-n` limits the **total** rather
    /// than the per-branch count (observed: `-n 4 --branches` gives four commits interleaved
    /// across two branches), and past it the miss is **silent** — the scan surface becomes a
    /// proper subset of the publish surface. And this push sends the whole history.
    ///
    /// The cost is near zero: 2000 commits take one process and 73 ms (observed). Keeping a cap
    /// that allows silently for that much cost is the wrong direction.
    ///
    /// The secret is deliberately on the **oldest** commit — exactly the position any "look only
    /// at the most recent N" implementation cannot see.
    #[test]
    fn a_secret_deep_in_history_is_not_skipped_by_a_cap() {
        const DEPTH: usize = 260; // Deeper than a cap of 200.
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // The oldest one carries the secret.
        run(&[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("oldest, key {AWS}"),
        ]);
        for i in 0..DEPTH {
            run(&["commit", "-q", "--allow-empty", "-m", &format!("round {i}")]);
        }

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = commit_message_hits(&repo, &none()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the first push sends the whole history, so the oldest one is scanned too: {hits:?}"
        );
    }

    /// An unpushed commit message **on another branch** is scanned too.
    ///
    /// One bug on two levels with the tag case: both levels depend on the invariant "the scan
    /// surface covers every branch", and giving branch coverage to only one of them does not
    /// deliver it. `agit push` appends main unconditionally whenever it exists locally, so pushing
    /// from a session branch sends an unpushed message on main that carries a secret, while a scan
    /// taken over `origin/<current branch>..HEAD` cannot see it.
    #[test]
    fn an_unpushed_commit_on_another_branch_is_still_scanned() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "-M", "main"]);
        run(&["checkout", "-q", "-b", "side"]);
        run(&["commit", "-q", "--allow-empty", "-m", "side work"]);
        // The secret is only on the unpushed commit that main alone has.
        run(&["checkout", "-q", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", &format!("key {AWS}")]);
        run(&["checkout", "-q", "side"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = commit_message_hits(&repo, &none()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "push sends main along, so an unpushed message on main is scanned too: {hits:?}"
        );
    }

    /// With HEAD unborn, unpushed commit messages **on other branches** are still scanned.
    ///
    /// Both scan paths select with `--branches` and neither touches HEAD — so "is HEAD born" is a
    /// precondition for neither. The tag slot has a test; the commit slot is easy to miss, because
    /// a change tends to happen only in the layer being looked at.
    #[test]
    fn an_unborn_head_does_not_blank_out_the_commit_surface() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", &format!("key {AWS}")]);
        run(&["branch", "-M", "main"]);
        // HEAD points at a branch with no commit; the unpushed message on main is pushed all the
        // same.
        run(&["checkout", "-q", "--orphan", "draft"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = commit_message_hits(&repo, &none()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "an unborn HEAD does not mean other branches have no unpushed commits — push sends them anyway: {hits:?}"
        );
    }

    /// Failing to list local branches **errors**; it is not treated as "there are no branches"
    /// and allowed.
    ///
    /// With no `head_is_born` fast path, `local_branches` is the only entry test on the tag path.
    /// `.unwrap_or_default()` conflates "git failed" into an empty `Vec`, and an empty `Vec` makes
    /// `scan_tag_objects` return empty, which is clean. `scan_messages` in this same module says
    /// it outright: failing to read is not "there is no message here" — that is a scanner's worst
    /// failure.
    #[test]
    fn a_repo_whose_branches_cannot_be_listed_is_not_reported_clean() {
        let d = tempfile::tempdir().unwrap(); // Deliberately **not** git init.
        let repo = crate::domain::repo::Repo::at(d.path());
        assert!(
            local_branches(&repo).is_err(),
            "failing to list branches has to error — treating it as 'no branches' calls something never looked at fine"
        );
    }

    /// With HEAD unborn, tags **on other branches** are still scanned.
    ///
    /// A `head_is_born` gate at the top of `scan_tag_objects` was justified by
    /// "`--merged <branch>` fails on an unborn branch". Scanning by branch set removes that
    /// justification, and keeping the gate is harmful: after `git checkout --orphan`, tags on
    /// **every** branch go unscanned and it returns empty silently, while push's publish surface
    /// is unaffected by HEAD.
    #[test]
    fn an_unborn_head_does_not_blank_out_the_tag_surface() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "-M", "main"]);
        run(&["tag", "-a", "agit-leak", "-m", &format!("key {AWS}")]);
        // HEAD points at a branch with no commit.
        run(&["checkout", "-q", "--orphan", "draft"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits = tag_hits(&repo, &none(), &tag_scan_branches(&repo).unwrap())
            .expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "an unborn HEAD does not mean other branches have no tags — push sends them anyway: {hits:?}"
        );
    }

    /// **Bytes that are not valid UTF-8** in a tag body must not make the tags behind it
    /// disappear either.
    ///
    /// # The same hole as the NUL case
    ///
    /// Splitting by separator is broken by one NUL; splitting by the byte count of `%(raw:size)`
    /// instead still splits a string taken **after** `from_utf8_lossy`. And lossy replaces every
    /// illegal byte with a three-byte U+FFFD, so the string's length no longer equals the length
    /// git reported: splitting by length is shifted all the same, and a `split_at` landing inside
    /// a U+FFFD panics outright.
    ///
    /// Switching to splitting by length only changes the trigger, and the hole remains — **because
    /// what is split is still a converted view while the length used is the raw one**. So the
    /// split happens on raw bytes, and the conversion happens last and applies to one body only.
    ///
    /// That rule is now held by `cat-file --batch` (bodies are raw bytes, framed by the length git
    /// reports), and `from_utf8_lossy` applies only to the one body handed to the scanning engine
    /// — this test guards that invariant, not any one implementation.
    ///
    /// Building such a tag only takes `git tag -a x -F <any binary file>`.
    #[test]
    fn non_utf8_bytes_in_one_tag_do_not_hide_the_others() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);

        // The poisoned tag sorts first (ordered by refname) and its body holds bytes that are
        // genuinely illegal in UTF-8.
        let msg = d.path().join("m");
        std::fs::write(&msg, b"bad bytes: \xff\xfe\xff\xfe here\n").unwrap();
        run(&["tag", "-a", "aaa-bad-bytes", "-F", msg.to_str().unwrap()]);
        run(&["tag", "-a", "zzz-secret", "-m", &format!("key {AWS}")]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "a tag ordered after the poisoned one is scanned all the same: {hits:?}"
        );
    }

    /// The scan surface has to be **a superset of the publish surface**: a tag on another branch
    /// is scanned too.
    ///
    /// # Why
    ///
    /// The tags `agit push` pushes are `tags_to_push(repo, refs_to_push(branches, has_main))` —
    /// `branches` can be several given with `-b` or everything under `--all`, and **main is
    /// appended unconditionally whenever it exists locally**. A scan that looks only at
    /// `current_branch()` makes the scan surface a **proper subset** of the publish surface: a tag
    /// on main carrying a secret makes `agit push` from a session branch report clean, push it,
    /// and be refused by the server.
    ///
    /// This is exactly the shape `scan_tag_objects`'s docs describe: a test written once on each
    /// side drifts apart sooner or later, and the drift is always a miss. So what is scanned is
    /// **every local branch**, which is a superset of any single push.
    #[test]
    fn a_tag_on_another_branch_is_still_scanned() {
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        run(&["branch", "-M", "main"]);
        // Diverge first, then tag the commit main alone has — so it is unreachable from side.
        run(&["checkout", "-q", "-b", "side"]);
        run(&["commit", "-q", "--allow-empty", "-m", "side work"]);
        run(&["checkout", "-q", "main"]);
        run(&["commit", "-q", "--allow-empty", "-m", "main only"]);
        run(&["tag", "-a", "agit-leak", "-m", &format!("key {AWS}")]);
        run(&["checkout", "-q", "side"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        // Precondition: it is invisible from the current branch — otherwise this test shows no
        // difference.
        let from_current =
            tag_hits(&repo, &none(), &["side".to_string()]).expect("the repo is healthy");
        assert!(
            from_current.is_empty(),
            "precondition: agit-leak is unreachable from side, or this tests nothing across branches"
        );

        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "push sends main along, so a tag on main is scanned too: {hits:?}"
        );
    }

    /// As tags multiply, the cost of scanning must not follow the tag count.
    ///
    /// # Why a **timed** test
    ///
    /// agit **cuts a version tag every turn** (`refs/tags/agit-<40hex>`), so an agent that runs a
    /// few hundred turns has a few hundred tags, and `agit scan` runs before every push.
    ///
    /// Starting processes per tag (list the names, then run `cat-file -t`, `cat-file tag` and
    /// `rev-parse` for each) takes **13 seconds** for 500 tags (observed) — that is not "a little
    /// slower", that is the user starting to route around this gate, and a gate that gets routed
    /// around is no gate. Reading it all with one `for-each-ref --format` takes **0.06 seconds**.
    ///
    /// The assertion uses a very loose ceiling (two seconds): what it catches is a regression of
    /// the **order of magnitude** that going back to a process per tag produces, not a wobble of
    /// tens of milliseconds. A slow CI machine must not turn it falsely red.
    #[test]
    fn scanning_many_tags_does_not_cost_a_process_per_tag() {
        const TAGS: usize = 300;
        let d = repo_dir();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        for i in 0..TAGS {
            run(&["tag", "-a", &format!("agit-{i:040}"), "-m", "clean"]);
        }

        let repo = crate::domain::repo::Repo::at(d.path());
        let t0 = std::time::Instant::now();
        let hits =
            tag_hits(&repo, &none(), &local_branches(&repo).unwrap()).expect("the repo is healthy");
        let took = t0.elapsed();

        assert!(hits.is_empty(), "these tags are all clean: {hits:?}");
        assert!(
            took < std::time::Duration::from_secs(2),
            "scanning {TAGS} tags took {took:?} — back to a few git processes per tag?"
        );
    }

    /// Create a repo with an annotated tag and return (directory, the tag object's oid).
    fn tagged_repo(tag_args: &[&str], tagger: &str) -> (tempfile::TempDir, String) {
        let d = repo_dir();
        let run = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(d.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "clean commit"]);

        let mut args: Vec<String> = vec![
            "-c".into(),
            format!("user.name={tagger}"),
            "tag".into(),
            "-a".into(),
            "v1".into(),
        ];
        args.extend(tag_args.iter().map(|s| (*s).to_string()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&refs);

        let oid = run(&["rev-parse", "refs/tags/v1"]);
        (d, oid)
    }

    /// **A broken HEAD is not an empty repo.**
    ///
    /// A test of "`rev-parse --verify HEAD` failed = empty repo" is not enough: a corrupt HEAD,
    /// wrong permissions on `.git`, and not being in a repo at all all make `rev-parse` fail, so a
    /// fault gets treated as an empty history and the scan silently reports clean — the very shape
    /// this path exists to remove.
    ///
    /// The test needs two signals: only `symbolic-ref` succeeding while `rev-parse` fails is "an
    /// unborn branch".
    #[test]
    fn a_broken_head_is_not_mistaken_for_an_empty_repo() {
        let d = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .output()
            .unwrap();
        // The empty repo is clean at this step (an unborn branch, which is legitimate).
        let repo = crate::domain::repo::Repo::at(d.path());
        assert!(
            commit_message_hits(&repo, &none()).is_ok(),
            "an unborn branch is a legitimate state"
        );

        // Now corrupt HEAD: symbolic-ref fails, so this is no longer "an empty repo" but
        // "unreadable".
        std::fs::write(
            d.path().join(".git/HEAD"),
            "this is not a ref
",
        )
        .unwrap();
        assert!(
            commit_message_hits(&repo, &none()).is_err(),
            "a broken HEAD has to error — treating it as an empty repo calls something never looked at fine"
        );
    }

    /// A freshly initialized agent with no commit is not an error; it has no history to scan.
    #[test]
    fn an_empty_repo_has_no_history_to_scan() {
        let d = tempfile::tempdir().unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(d.path())
            .output()
            .unwrap();
        let repo = crate::domain::repo::Repo::at(d.path());
        assert!(
            commit_message_hits(&repo, &none())
                .expect("an empty repo is not a failure")
                .is_empty()
        );
    }

    /// push and scan look at one scan surface, or what `agit scan` reports and what push stops do
    /// not line up.
    #[test]
    fn the_publish_surface_is_one_predicate() {
        assert!(in_publish_surface(crate::domain::meta::LOG_FILE));
        assert!(in_publish_surface(crate::domain::meta::VIEW_FILE));
        assert!(in_publish_surface("AGENTS.md"));
        assert!(in_publish_surface("memory/decisions.md"));
        assert!(in_publish_surface("skills/x/SKILL.md"));
        assert!(!in_publish_surface(crate::domain::meta::FILE));
        assert!(!in_publish_surface(".git/config"));
    }

    #[test]
    fn catches_common_shapes() {
        for (input, rule) in [
            (AWS, "aws-access-token"),
            (GHP, "github-pat"),
            (NPM, "npm-access-token"),
            // A shape met for real in a session transcript (an HF token typed straight into a
            // prompt).
            (HF, "huggingface-access-token"),
            // agit's own token appearing in a transcript means somebody (or some agent) read
            // credentials.json into the context. gitleaks cannot have this rule.
            (AGIT, "agit-token"),
        ] {
            let hits = scan_text(input, &none());
            assert!(
                hits.iter().any(|h| h.rule == rule),
                "`{input}` matches `{rule}`, got {hits:?}"
            );
        }
    }

    /// gitleaks' `private-key` only recognizes a complete PEM, while the most common shape in a
    /// transcript is the truncated one, so a rule of ours recognizes the header alone. Losing it
    /// loses detection for a whole class of private key.
    #[test]
    fn a_truncated_private_key_header_still_trips_the_gate() {
        let hits = scan_text(
            "-----BEGIN RSA PRIVATE KEY-----\n(output truncated)",
            &none(),
        );
        assert!(
            hits.iter().any(|h| h.rule == "agit-private-key-header"),
            "{hits:?}"
        );
    }

    /// gitleaks has no generic "credentials in a URL" rule; this one is ours.
    #[test]
    fn credentials_inside_a_url_are_caught_but_placeholders_are_not() {
        let hits = scan_text(
            "git clone https://bob:s3cr3t-Xyz9@example.com/r.git",
            &none(),
        );
        assert!(
            hits.iter().any(|h| h.rule == "agit-credentials-in-url"),
            "{hits:?}"
        );

        for placeholder in [
            "https://user:password@host/r.git",
            "https://user:${GIT_PASS}@host/r.git",
            "https://user:<your-token>@host/r.git",
        ] {
            let hits = scan_text(placeholder, &none());
            assert!(
                !hits.iter().any(|h| h.rule == "agit-credentials-in-url"),
                "a placeholder is not reported: {placeholder} → {hits:?}"
            );
        }
    }

    #[test]
    fn never_echoes_the_full_secret() {
        // The most important one: reports go into CI logs, and a whole secret appearing there
        // leaks it a second time.
        let hits = scan_text(GHP, &none());
        assert!(!hits.is_empty());
        for h in &hits {
            assert!(
                !h.redacted.contains(GHP),
                "redaction failed: {}",
                h.redacted
            );
            assert!(h.redacted.contains('*'));
        }
    }

    #[test]
    fn inline_pragma_and_allowlist_suppress() {
        assert!(
            scan_text(&format!("token = {AWS} # agit:allow-secret"), &none()).is_empty(),
            "an inline annotation skips the whole line"
        );
        let mut allow = HashSet::new();
        allow.insert(AWS.to_string());
        assert!(scan_text(AWS, &allow).is_empty());
    }

    /// The server runs the same engine but honours no local waiver — otherwise anyone could push
    /// a secret with one annotation.
    #[test]
    fn strict_policy_ignores_every_local_waiver() {
        let line = format!("token = {AWS} # agit:allow-secret");
        let mut allow = HashSet::new();
        allow.insert(AWS.to_string());
        assert!(
            scan_text(&line, &allow).is_empty(),
            "the local gate allows it"
        );
        assert!(
            !scan_text_with(&line, &allow, Policy::STRICT).is_empty(),
            "the authoritative gate ignores inline annotations and the allowlist"
        );
    }

    #[test]
    fn plain_technical_prose_does_not_trigger() {
        // False positives spend trust. Ordinary technical prose has to come out clean.
        let prose = "We keep the token in an environment variable instead of hard-coding a \
                     password.\nThis function returns the hash of the secret.";
        assert!(
            scan_text(prose, &none()).is_empty(),
            "ordinary technical prose is not a false positive"
        );
    }

    /// The biggest risk of moving to 221 rules is not a miss, it is **a flood of false
    /// positives**.
    ///
    /// `generic-api-key` looks further as soon as it sees `key|token|secret|password|auth…`
    /// followed by an `=`, and a transcript is full of such sentences. A push that reports dozens
    /// of fake leaks teaches the user exactly one thing, `AGIT_ALLOW_SECRETS=1`, and after that a
    /// real leak is not stopped either. So the "looks like one but is not" shapes below have to
    /// pass cleanly.
    #[test]
    fn a_transcript_full_of_lookalikes_stays_quiet() {
        let corpus = concat!(
            // Environment-variable references and placeholders: the value is not in the text.
            "export GITHUB_TOKEN=$GITHUB_TOKEN\n",
            "api_key = os.environ[\"API_KEY\"]\n",
            "password: ${DB_PASSWORD}\n",
            "auth_token = \"{{ vault.secret }}\"\n",
            "AWS_SECRET_ACCESS_KEY=<your-secret-here>\n",
            // Ordinary git and package-manager output: plenty of high-entropy hex with no
            // credential meaning.
            "commit 4f2a9c1e7b3d8056af12cd34ef56ab78901234cd\n",
            "  tree e91b0c2d4a6f8135792bcde04613f8a5c7d92e0b\n",
            "added 214 packages in 3s / audited 1204 packages\n",
            "sha256-9f8e7d6c5b4a3021ffeeddccbbaa99887766554433221100aabbccddeeff0011\n",
            // Prose and commands that really occur in a session.
            "user: help me see why the auth middleware 401s, the token should come from the header\n",
            "assistant: your credentials go through the keychain, no plaintext password belongs in the code\n",
            "$ curl -H \"Authorization: Bearer $TOKEN\" https://api.example.com/v1/me\n",
            "$ git remote add origin git@github.com:acme/widget.git\n",
            // Paths, UUIDs, timestamps.
            "/Users/nana/Library/Application Support/agit/credentials.json\n",
            "session 3f6b1c2a-8d40-4e7b-9a15-2c0de4f8b731 at 2026-08-16T09:12:44Z\n",
        );
        let hits = scan_text(corpus, &none());
        assert!(
            hits.is_empty(),
            "a flood of false positives gets the gate switched off entirely: {hits:?}"
        );
    }

    #[test]
    fn short_values_do_not_trigger_generic_rule() {
        assert!(scan_text("password = short", &none()).is_empty());
        assert!(scan_text("api_key = abc123", &none()).is_empty());
    }

    /// The entropy threshold stops placeholders, not secrets. Without it, `generic-api-key`
    /// reports every `token = <anything>` line as a leak and the gate gets switched off
    /// entirely.
    #[test]
    fn low_entropy_placeholders_are_not_reported() {
        for fake in [
            "api_key = \"xxxxxxxxxxxxxxxxxxxx\"",
            "password: aaaaaaaaaaaaaaaaaaaa",
            "token = \"changeme_changeme_changeme\"",
        ] {
            assert!(
                scan_text(fake, &none()).is_empty(),
                "a placeholder is not reported: {fake}"
            );
        }
    }

    /// The gitleaks aws rule carries its own `.+EXAMPLE$` allowance — `AKIAIOSFODNN7EXAMPLE` from
    /// the official AWS docs is everywhere, and reporting it only trains users to ignore reports.
    #[test]
    fn rule_owned_allowlists_are_honored() {
        assert!(scan_text("AKIAIOSFODNN7EXAMPLE", &none()).is_empty());
        // A non-example key of the same shape is reported all the same.
        assert!(!scan_text(AWS, &none()).is_empty());
    }

    /// One leak takes one line of the report: `token: npm_…` matches both the catch-all
    /// `generic-api-key` and the specific `npm-access-token`, and the more specific one is kept.
    #[test]
    fn one_secret_is_reported_once_by_the_most_specific_rule() {
        let hits = scan_text(&format!("noting the deploy token: {NPM}"), &none());
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].rule, "npm-access-token");
    }

    #[test]
    fn line_numbers_start_at_one() {
        let hits = scan_text(&format!("clean\n{AWS}"), &none());
        assert_eq!(hits[0].line, 2);
    }

    /// A newline inside a jsonl body is the two characters `\` and `n`, so a token right after it
    /// has no left `\b` boundary, and a whole rule misses it silently.
    #[test]
    fn tokens_right_after_json_escapes_are_caught() {
        let line = format!(r#"{{"content":"first line\n{NPM} tail"}}"#);
        let hits = scan_text(&line, &none());
        assert!(
            hits.iter().any(|h| h.rule == "npm-access-token"),
            "a secret adjacent to an escape must match: {hits:?}"
        );

        let (text, n) = scrub(&line);
        assert!(n >= 1);
        assert!(!text.contains(NPM));
        assert!(text.contains("[redacted:npm-access-token]"));
        // The replacement touches only the token itself and keeps the surrounding escape
        // sequences unchanged.
        assert!(text.contains(r#""first line\n"#), "{text}");
    }

    /// End to end: changing the view must not move the line numbers.
    #[test]
    fn view_normalization_keeps_line_numbers() {
        let text = format!("a\n{{\"content\":\"x\\t{AWS}\"}}\nb");
        let hits = scan_text(&text, &none());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].line, 2);
    }

    /// scrub honours no local waiver — those two switches are "do not stop my push", not "carry
    /// the secret into the published copy".
    #[test]
    fn scrub_ignores_local_waivers() {
        let (text, n) = scrub(&format!("token = {AWS} # agit:allow-secret"));
        assert_eq!(n, 1);
        assert!(!text.contains(AWS), "{text}");
    }

    /// Performance is a hard requirement, not a nice-to-have.
    ///
    /// Observed: running all 221 regexes over a 2.6MB transcript takes 16.6 seconds, and 27ms
    /// behind the keyword prefilter. One push scans a whole repo, and a gate that takes seconds
    /// gets routed around directly (`AGIT_ALLOW_SECRETS=1` is right there in the hint), so slow
    /// means absent.
    ///
    /// On this machine it is 21ms (dev) / 25ms (release) observed, and the budget is set an order
    /// of magnitude above that at 300ms for slower CI. The broken shapes are nowhere near the
    /// budget and cannot go falsely green: without the prefilter it is 16.6 seconds, and with too
    /// small a DFA cache 1.4 seconds. The test also asserts that the prefilter really wakes only a
    /// few rules — which is where that number comes from.
    #[test]
    fn scans_a_megabyte_transcript_fast() {
        // Build text that looks like a real transcript: prose + code + tool output, with plenty
        // of the key / token / secret words that trip the prefilter.
        let chunk = concat!(
            r#"{"role":"assistant","content":"I moved the api_key in config to be read from an "#,
            r#"environment variable, so the token stays out of the repo. Next: run pytest."}"#,
            "\n",
            r#"{"role":"tool","content":"$ npm run build\n> tsc -p .\nDone in 3.2s"}"#,
            "\n",
            r#"{"role":"user","content":"change that password part too, do not hard-code a secret"}"#,
            "\n",
        );
        let mut big = String::with_capacity(2 << 20);
        while big.len() < (2 << 20) {
            big.push_str(chunk);
        }
        assert!(big.len() >= 2 * 1024 * 1024);

        // Run a short stretch of the same content first to pay the one-time costs: lazy
        // compilation (1.37 seconds for all of them) and lazy DFA state construction both happen
        // once per process, so measuring them measures startup, while one push scans many files
        // — what decides the experience is the steady state.
        let _ = scan_text(&chunk.repeat(64), &none());

        let t0 = std::time::Instant::now();
        let hits = scan_text(&big, &none());
        let dt = t0.elapsed();
        // dev and release are the same order of magnitude here because Cargo.toml turns on
        // opt-level = 3 for the regex dependencies specifically — without it debug takes 147ms,
        // and the number then reports the profile rather than the engine.
        let budget = std::time::Duration::from_millis(300);
        assert!(
            dt < budget,
            "scanning {} bytes took {dt:?} (budget {budget:?}) — the prefilter, lazy compilation or the DFA cache limit is broken ({} hits)",
            big.len(),
            hits.len()
        );

        let picked = rules::candidates(&big);
        assert!(
            picked.len() < 40,
            "the prefilter wakes only a few rules, got {}/{}",
            picked.len(),
            rules::count()
        );
    }

    /// A small helper that runs git, used alongside building a session-shaped history.
    fn git_in(dir: &std::path::Path) -> impl Fn(&[&str]) -> String + '_ {
        move |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
    }

    /// The branch tips the destination itself reports — in production from
    /// `git ls-remote --heads`, and here the same question asked of a local bare repo.
    fn ask(hub: &std::path::Path) -> Vec<String> {
        let out = std::process::Command::new("git")
            .args(["ls-remote", "--heads", &hub.to_string_lossy()])
            .output()
            .unwrap();
        assert!(out.status.success(), "git ls-remote");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_string)
            .collect()
    }

    /// After a remote is deleted and recreated, the scan surface **must not** shrink because of
    /// the local tracking refs.
    ///
    /// # What this pins
    ///
    /// A scan surface of `rev-list --objects --branches --not --remotes=origin` subtracts **the
    /// local remote-tracking refs**, and those describe the remote of the last fetch/push, not the
    /// destination of this push. The two come apart along real paths: `AGIT_HUB_URL=… agit push`
    /// switches hub, and `agit repo delete` defaults "delete the local copy too?" to false.
    ///
    /// The cleanest case is reproduced here: after a push, the remote is deleted and recreated
    /// empty. The tracking refs do not change by a word, so the old surface is **empty** (which is
    /// what the second precondition below asserts), while `git push` sends the whole history
    /// again, that secret blob included.
    ///
    /// The secret is deliberately left in history only (the later commit deletes the file), so the
    /// working-tree pass cannot cover for the object pass — what is under test is the object scan
    /// surface itself.
    #[test]
    fn a_recreated_remote_does_not_shrink_the_scan_surface() {
        let d = tempfile::tempdir().unwrap();
        let work = d.path().join("work");
        let hub = d.path().join("hub.git");
        std::fs::create_dir_all(&work).unwrap();
        let run = git_in(&work);
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("payload.txt"), format!("key = {AWS}\n")).unwrap();
        run(&["add", "payload.txt"]);
        run(&["commit", "-q", "-m", "add payload"]);
        // Delete the file: the working tree is clean from here on, and the secret lives only in
        // that still-reachable blob in history.
        std::fs::remove_file(work.join("payload.txt")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop payload"]);

        std::process::Command::new("git")
            .args(["init", "-q", "--bare", &hub.to_string_lossy()])
            .output()
            .unwrap();
        run(&["remote", "add", "origin", &hub.to_string_lossy()]);
        run(&["push", "-q", "origin", "main"]);

        // The remote is deleted and recreated empty. **Nothing local changed.**
        std::fs::remove_dir_all(&hub).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q", "--bare", &hub.to_string_lossy()])
            .output()
            .unwrap();

        // Precondition 1: the tracking refs are still there, unchanged — nothing looks wrong
        // locally.
        assert!(
            !run(&["for-each-ref", "--format=%(refname)", "refs/remotes/origin"]).is_empty(),
            "precondition fails: the tracking refs are gone, so this test cannot reach the wrong test"
        );
        // Precondition 2: under the old test the scan surface is **empty**. That is the bug
        // itself.
        assert!(
            run(&[
                "rev-list",
                "--objects",
                "--branches",
                "--not",
                "--remotes=origin"
            ])
            .is_empty(),
            "precondition fails: the old surface is non-empty, so this test is green on the old implementation too"
        );
        // Precondition 3: and yet this push really does send that blob.
        assert!(
            !run(&["rev-list", "--objects", "--branches"]).is_empty(),
            "precondition fails: the full surface is empty too"
        );

        let repo = crate::domain::repo::Repo::at(&work);
        let dest = Destination::advertised(&repo, ask(&hub)).expect("the repo is healthy");
        assert_eq!(
            dest,
            Destination::Unknown,
            "an empty repo has nothing, so the destination narrows no scan surface"
        );
        let hits = scan_agent_repo(&repo, &ScanPlan::to(dest))
            .expect("the repo is healthy")
            .hits;
        assert!(
            hits.iter().any(|h| h.rule == "aws-access-token"),
            "the destination is an empty repo, this push sends the whole history, and that blob must be scanned: {hits:?}"
        );
    }

    /// Only a destination that is really there and really has those commits may have them
    /// subtracted from the scan surface.
    ///
    /// The case above pins "do not subtract wrongly"; this one pins "what should be subtracted
    /// still is" — otherwise every push falls back to a full scan, and a full scan is superlinear
    /// in this product's repo shape (see [`ScanLimits`]).
    #[test]
    fn a_live_remote_still_shrinks_the_scan_surface() {
        let d = tempfile::tempdir().unwrap();
        let work = d.path().join("work");
        let hub = d.path().join("hub.git");
        std::fs::create_dir_all(&work).unwrap();
        let run = git_in(&work);
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(work.join("payload.txt"), format!("key = {AWS}\n")).unwrap();
        run(&["add", "payload.txt"]);
        run(&["commit", "-q", "-m", "add payload"]);
        std::fs::remove_file(work.join("payload.txt")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop payload"]);

        std::process::Command::new("git")
            .args(["init", "-q", "--bare", &hub.to_string_lossy()])
            .output()
            .unwrap();
        run(&["remote", "add", "origin", &hub.to_string_lossy()]);
        run(&["push", "-q", "origin", "main"]);

        let repo = crate::domain::repo::Repo::at(&work);
        let dest = Destination::advertised(&repo, ask(&hub)).expect("the repo is healthy");
        assert!(
            dest.narrows(),
            "the destination really has these commits, so it narrows the scan surface"
        );
        let hits = scan_agent_repo(&repo, &ScanPlan::to(dest))
            .expect("the repo is healthy")
            .hits;
        assert!(
            hits.is_empty(),
            "the destination already has these bytes and none of them go over the network this time — stopping here only stops the user on history they cannot change: {hits:?}"
        );
    }

    /// Build a **session-shaped** history: `session/log.jsonl` is append-only with one commit per
    /// turn.
    ///
    /// This shape is the product's default shape and the source of the superlinear work: the blob
    /// of turn n has n lines, so the sum of bytes over all reachable blobs is on the order of
    /// turns².
    fn append_only_session(dir: &std::path::Path, rounds: usize, secret_at: Option<usize>) {
        let run = git_in(dir);
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(dir.join("session")).unwrap();
        let mut log = String::new();
        for i in 0..rounds {
            let body = match secret_at {
                Some(n) if n == i => format!("{{\"role\":\"user\",\"text\":\"key {AWS}\"}}\n"),
                _ => format!(
                    "{{\"role\":\"user\",\"text\":\"turn {i} {}\"}}\n",
                    "x".repeat(200)
                ),
            };
            log.push_str(&body);
            std::fs::write(dir.join("session/log.jsonl"), &log).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", &format!("turn {i}")]);
        }
        // The secret is left in history only: the last turn wipes the log, so the working-tree
        // pass takes no part in the verdict.
        if secret_at.is_some() {
            std::fs::write(dir.join("session/log.jsonl"), "{\"role\":\"user\"}\n").unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-q", "-m", "rotate log"]);
        }
    }

    /// A long history either gets scanned inside the budget or **says outright that it was not**
    /// — no silently grinding through it, and no silently allowing it.
    ///
    /// # What this pins
    ///
    /// None of the four layers that bound cost bounds **work**: the first two only keep the input
    /// out of memory in one piece, the fourth counts `Hit`s, and the third (abort the enumeration
    /// once the hit cap fills) **never fires** on a clean repo — the only early exit is dead on the
    /// most common path. So the duration is decided by how much work the user did: 1500 turns is
    /// 1337 MiB / 38.72 s observed, and extrapolating by the square, 6000 turns is about 21 GiB.
    ///
    /// The assertions come in two halves, because "bounded" means two things:
    ///
    /// 1. **A real agent passes as usual**: under the default budget this history is scanned to
    ///    the end and `unscanned` is empty. (This half guards against a bound so tight it keeps
    ///    ordinary users out.)
    /// 2. **Over the limit it does no work, and says so**: with the budget pressed below this
    ///    history, no object is read — the observable test being that the secret buried in the
    ///    history is **not** reported while `over_budget` is set. Returning no hits silently is
    ///    fail open, which is the failure this gate must not have.
    ///
    /// Deliberately **not written as a duration assertion**: that kind of assertion is randomly
    /// red on CI, and what is pinned here is how much work was done, not how many seconds it
    /// took.
    #[test]
    fn a_clean_repo_with_long_history_stays_within_budget() {
        let d = tempfile::tempdir().unwrap();
        append_only_session(d.path(), 40, Some(5));
        let repo = crate::domain::repo::Repo::at(d.path());

        let revs = Destination::Unknown.revs();
        let sel: Vec<&str> = revs.iter().map(String::as_str).collect();
        let branches = local_branches(&repo).expect("the repo is healthy");
        let total = estimate_object_bytes(&repo, &sel, &branches, &ScanLimits::DEFAULT, 0)
            .expect("the repo is healthy");
        assert!(
            total > 0 && total < ScanLimits::DEFAULT.budget_bytes,
            "precondition: this history is inside the default budget (estimated {total} bytes)"
        );

        // 1. Default budget: it scans to the end, nothing goes unread, and that secret is
        //    reported.
        let full = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            full.unscanned.is_empty(),
            "a session history of ordinary length reaches no cap: {:?}",
            full.unscanned
        );
        assert!(
            full.hits.iter().any(|h| h.rule == "aws-access-token"),
            "precondition: a secret really is buried in this history"
        );

        // 2. Budget pressed below it: **no object is read**, and it says so.
        let plan = ScanPlan {
            dest: Destination::Unknown,
            limits: ScanLimits {
                budget_bytes: total / 2,
                ..ScanLimits::DEFAULT
            },
        };
        let tight = scan_agent_repo(&repo, &plan).expect("over budget is not a scan failure");
        assert!(
            tight.unscanned.over_budget.is_some(),
            "over budget is passed out explicitly, not left as a few fewer hits: {:?}",
            tight.unscanned
        );
        assert!(
            !tight.hits.iter().any(|h| h.rule == "aws-access-token"),
            "over budget the history is not read anyway, or the bound means nothing: {:?}",
            tight.hits
        );
    }

    /// Tree bodies stream out of `cat-file` too, so they have to **count against the budget**.
    ///
    /// # What this pins
    ///
    /// The enumeration step of [`scan_publish_blobs`] cannot tell trees apart: `rev-list
    /// --objects` prints a subdirectory as `<oid> <directory name>`, carrying a path like a blob,
    /// so it enters the batch like one, `cat-file --batch` is asked for its body, and it is only
    /// dropped on `kind != "blob"` after being read. And with [`estimate_object_bytes`] counting
    /// only `blob | commit | tag`, a repo with small blobs and large trees estimates far below the
    /// real work and the budget gate is a formality.
    ///
    /// That shape is what is built here: 300 small files with 40-character names in one directory,
    /// whose blobs add up to about a thousand bytes, while that one tree is an order of magnitude
    /// larger than all of them together — an entry is "mode + name + NUL + binary sha", and with
    /// long names and short bodies the directory itself is the bulk. v1's
    /// `events/a/b/c/d/<id>` sharding has the same shape.
    ///
    /// All three halves are asserted:
    ///
    /// 1. The estimate covers at least that tree (the ground truth is asked of git directly, not
    ///    recomputed in the test).
    /// 2. End to end: with the budget pressed to that tree's size, the scan **does not read** and
    ///    says so — the observable test being that the secret in history is not reported while
    ///    `over_budget` is set.
    /// 3. The reverse half: under the default budget this repo is scanned to the end and the
    ///    secret is reported as usual. Without it, an implementation whose estimate returns
    ///    `u64::MAX` also passes the first two.
    #[test]
    fn tree_bytes_are_inside_the_budget_because_the_scan_streams_them() {
        let d = tempfile::tempdir().unwrap();
        let run = git_in(d.path());
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::create_dir_all(d.path().join("wide")).unwrap();
        for i in 0..300u32 {
            std::fs::write(d.path().join(format!("wide/{i:0>40}")), format!("{i}\n")).unwrap();
        }
        // The secret is left in history only: the working-tree pass takes no part in the
        // verdict.
        std::fs::write(d.path().join("secret.txt"), format!("key = {AWS}\n")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "wide"]);
        std::fs::remove_file(d.path().join("secret.txt")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop secret"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let wide_tree = repo.git(&["rev-parse", "HEAD:wide"]).unwrap();
        let wide_bytes: u64 = repo
            .git(&["cat-file", "-s", wide_tree.trim()])
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let revs = Destination::Unknown.revs();
        let sel: Vec<&str> = revs.iter().map(String::as_str).collect();
        let branches = local_branches(&repo).expect("the repo is healthy");
        let estimate = estimate_object_bytes(&repo, &sel, &branches, &ScanLimits::DEFAULT, 0)
            .expect("the repo is healthy");
        assert!(
            estimate >= wide_bytes,
            "that tree's {wide_bytes} bytes stream through cat-file in full while the estimate is only {estimate}"
        );

        // 1. Budget pressed to that one tree's size: blobs plus commits add up to far less, so
        //    only counting the tree crosses the line.
        let plan = ScanPlan {
            dest: Destination::Unknown,
            limits: ScanLimits {
                budget_bytes: wide_bytes,
                ..ScanLimits::DEFAULT
            },
        };
        let tight = scan_agent_repo(&repo, &plan).expect("over budget is not a scan failure");
        assert!(
            tight.unscanned.over_budget.is_some(),
            "tree bytes that cannot move the budget make this gate something other than 'compute the work first, then decide': {:?}",
            tight.unscanned
        );
        assert!(
            !tight.hits.iter().any(|h| h.rule == "aws-access-token"),
            "over budget no object is read: {:?}",
            tight.hits
        );

        // 2. Under the default budget it scans and reports as usual — this bound does not keep a
        //    good repo out.
        let full = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            full.unscanned.is_empty(),
            "a tree of a few tens of thousands of bytes is far inside the default budget: {:?}",
            full.unscanned
        );
        assert!(
            full.hits.iter().any(|h| h.rule == "aws-access-token"),
            "precondition: a secret really is buried in this history"
        );
    }

    /// The **root** tree is not counted — its path in [`scan_publish_blobs`]'s enumeration line is
    /// empty and that side never reads it.
    ///
    /// The easiest mistake in the other direction, once trees count against the budget, is
    /// counting every tree: the estimate then pushes bytes nobody reads into the budget and, large
    /// enough, keeps a good repo out. The repo here is wide only at the root (there is no
    /// subdirectory), so the root tree is the only bulk — counted, the estimate jumps above it.
    #[test]
    fn the_root_tree_stays_out_of_the_budget_because_nothing_reads_it() {
        let d = tempfile::tempdir().unwrap();
        let run = git_in(d.path());
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        for i in 0..300u32 {
            std::fs::write(d.path().join(format!("{i:0>40}")), format!("{i}\n")).unwrap();
        }
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "wide root"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let root_tree = repo.git(&["rev-parse", "HEAD^{tree}"]).unwrap();
        let root_bytes: u64 = repo
            .git(&["cat-file", "-s", root_tree.trim()])
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let revs = Destination::Unknown.revs();
        let sel: Vec<&str> = revs.iter().map(String::as_str).collect();
        let branches = local_branches(&repo).expect("the repo is healthy");
        let estimate = estimate_object_bytes(&repo, &sel, &branches, &ScanLimits::DEFAULT, 0)
            .expect("the repo is healthy");
        assert!(
            estimate < root_bytes,
            "nobody reads the root tree ({root_bytes} bytes), so it does not go into the budget; estimate {estimate}"
        );
    }

    /// A single object over the line keeps its body out of memory, and the fact that it **went
    /// unscanned** travels out.
    ///
    /// The end-to-end half: the assertions at the `git_cat_file_batch` layer live in
    /// `domain::repo`, and what is pinned here is that it reaches [`ScanReport::unscanned`]. A
    /// 300 MiB blob in history that has already been deleted drives maxRSS from 52 MB to 682 MB
    /// (observed), and the report says not one word about it.
    #[test]
    fn an_oversized_object_is_reported_as_unscanned() {
        let d = tempfile::tempdir().unwrap();
        let run = git_in(d.path());
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        // One blob over the line, with the secret inside it.
        let big = format!("{}\nkey = {AWS}\n", "x".repeat(64 * 1024));
        std::fs::write(d.path().join("big.txt"), &big).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "big"]);
        // Delete it: the working-tree pass takes no part, and the object path is under test.
        std::fs::remove_file(d.path().join("big.txt")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "drop"]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let plan = ScanPlan {
            dest: Destination::Unknown,
            limits: ScanLimits {
                max_object_bytes: 4 * 1024,
                ..ScanLimits::DEFAULT
            },
        };
        let report =
            scan_agent_repo(&repo, &plan).expect("crossing the line is not a scan failure");
        assert!(
            !report.unscanned.oversized.is_empty(),
            "an object over the line is booked in unscanned, or something never read is reported clean"
        );
        assert!(
            report.hits.is_empty(),
            "precondition: the secret sits inside the object over the line, so it cannot be reported — which is what unscanned is for"
        );
        // Under the line it is reported as usual: the cap governs whether it is read, not how
        // accurately it is scanned.
        let loose = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            loose.hits.iter().any(|h| h.rule == "aws-access-token"),
            "precondition: under the default cap this object is readable: {:?}",
            loose.hits
        );
    }

    /// **The same bytes in a different carrier must not change the verdict.**
    ///
    /// Tags are the last of the three body-reading paths to stay unbounded: going through
    /// `git_bytes(for-each-ref …%(raw))`, whose call site is not even passed `limits`, they are
    /// governed by neither [`ScanLimits::max_object_bytes`] nor booked in [`Unscanned`]. The same
    /// bytes, observed:
    ///
    /// | carrier | maxRSS | verdict |
    /// |---|---|---|
    /// | 200 MiB annotated tag | 610 MiB | `clean scan`, booked nowhere |
    /// | 66 MiB blob | 50 MiB | refused, booked in `oversized` |
    ///
    /// Both halves are asserted: a tag over the line has **not one byte read** (the secret buried
    /// in its body cannot be reported), and that fact **is said** (`oversized` is non-empty). With
    /// only the first half asserted, an implementation that silently skips every tag passes too,
    /// and that is the failure this gate must not have.
    #[test]
    fn an_oversized_tag_body_is_refused_like_an_oversized_blob() {
        let d = tempfile::tempdir().unwrap();
        let run = git_in(d.path());
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        run(&["config", "commit.gpgsign", "false"]);
        run(&["config", "tag.gpgsign", "false"]);
        run(&["commit", "-q", "--allow-empty", "-m", "base"]);
        // A tag body over the line with the secret inside it. The working tree and the commit
        // messages are clean, so any hit in the list can only come from this tag.
        let msg = format!("{}\nkey = {AWS}\n", "x".repeat(64 * 1024));
        run(&["tag", "-a", "agit-big", "-m", &msg]);

        let repo = crate::domain::repo::Repo::at(d.path());
        let plan = ScanPlan {
            dest: Destination::Unknown,
            limits: ScanLimits {
                max_object_bytes: 4 * 1024,
                ..ScanLimits::DEFAULT
            },
        };
        let report =
            scan_agent_repo(&repo, &plan).expect("crossing the line is not a scan failure");
        assert!(
            report.hits.is_empty(),
            "not one byte of a tag body over the line is read in: {:?}",
            report.hits
        );
        assert!(
            !report.unscanned.oversized.is_empty(),
            "a tag that went unread is booked, or the report calls something it never looked at clean"
        );
        // Under the line it is reported as usual: the cap governs whether it is read, not how
        // accurately it is scanned.
        let loose = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            loose.hits.iter().any(|h| h.source == Source::TagObject),
            "precondition: under the default cap this tag is readable: {:?}",
            loose.hits
        );
    }

    /// The working-tree pass is governed by the same bound, and **keeps its own ledger**.
    ///
    /// With no test on file size in front of `read_to_string`, a 300 MiB `artifact.log` in the
    /// working tree (**uncommitted**, so the object path cannot see it at all) drives maxRSS to
    /// 682 MB (observed) — the number [`ScanLimits::DEFAULT`]'s docs cite. And this pass runs
    /// **before** the object scan, so the "read no object at all when over budget" fast path
    /// cannot save it.
    ///
    /// The third assertion pins that the ledgers stay **separate**: paths and oids differ in how
    /// they are located and in what fixes them, and mixed into one `Vec` the report suggests
    /// `git cat-file -p artifact.log`.
    #[test]
    fn an_oversized_working_tree_file_is_refused_and_accounted_for() {
        let d = tempfile::tempdir().unwrap();
        let run = git_in(d.path());
        run(&["init", "-q", "-b", "main"]);
        // No add, no commit: what this tests is the working-tree pass.
        let big = format!("{}\nkey = {AWS}\n", "x".repeat(64 * 1024));
        std::fs::write(d.path().join("artifact.log"), &big).unwrap();

        let repo = crate::domain::repo::Repo::at(d.path());
        let plan = ScanPlan {
            dest: Destination::Unknown,
            limits: ScanLimits {
                max_object_bytes: 4 * 1024,
                ..ScanLimits::DEFAULT
            },
        };
        let report =
            scan_agent_repo(&repo, &plan).expect("crossing the line is not a scan failure");
        assert!(
            report.hits.is_empty(),
            "not one byte of a working-tree file over the line is read in: {:?}",
            report.hits
        );
        assert!(
            report
                .unscanned
                .oversized_files
                .iter()
                .any(|(p, n)| p == "artifact.log" && *n == big.len() as u64),
            "a working-tree file that went unread is booked **by path**: {:?}",
            report.unscanned
        );
        assert!(
            report.unscanned.oversized.is_empty(),
            "a path does not belong in the oid ledger — `git cat-file -p artifact.log` is advice that goes nowhere: {:?}",
            report.unscanned
        );
        // Under the line it is reported as usual: the cap governs whether it is read.
        let loose = scan_agent_repo(&repo, &ScanPlan::full()).expect("the repo is healthy");
        assert!(
            loose
                .hits
                .iter()
                .any(|h| h.source == Source::File && h.rule == "aws-access-token"),
            "precondition: under the default cap this file is readable: {:?}",
            loose.hits
        );
    }

    /// **Quantity is a kind of size.** A pile of files each within the per-file cap must not
    /// outrun the cumulative budget by all being individually compliant.
    ///
    /// # This pins two things, and they are two halves of one bug
    ///
    /// A working-tree pass that **reads first and books after** — `read_to_string` pulls the whole
    /// file into memory and only then `spent += size`, while the total budget reaches its verdict
    /// once the whole working tree has been walked and the object pass begins — gives:
    ///
    /// 1. **It keeps reading past the budget.** Every file is smaller than `max_object_bytes`, so
    ///    that bound stops none of them, and the cumulative budget is consulted only at the end.
    ///    Enough files read bytes far past the budget first and are told "over budget" after —
    ///    the promise of doing no work is not kept by a word. The observable test is that **fewer
    ///    hits are reported than there are files**: once it really stops, the secrets in the files
    ///    behind it cannot be reported.
    /// 2. **A file that fails to read is not booked.** For a non-UTF-8 or unreadable file the cost
    ///    of `read_to_string` has already been paid (the whole file went through memory), yet
    ///    taking the `else { continue }` path books not one byte. This half is pinned with a
    ///    looser budget of its own: all the text files together stay inside it and only counting
    ///    the binary one crosses the line — an implementation that does not book it reports no
    ///    `over_budget` here.
    ///
    /// Deliberately **not written as a duration assertion**: that kind of assertion is randomly
    /// red on CI (same reason as
    /// [`a_clean_repo_with_long_history_stays_within_budget`]). Both assertions are also
    /// **independent of traversal order**: `walkdir` guarantees no order within a directory, and
    /// "how many were read" and "how many bytes were booked in total" are the same under any
    /// order.
    #[test]
    fn a_pile_of_small_files_cannot_outrun_the_budget() {
        /// The number of text files. Each is far below `max_object_bytes` — the per-file bound
        /// is dead here.
        const FILES: usize = 8;

        let d = tempfile::tempdir().unwrap();
        let run = git_in(d.path());
        run(&["init", "-q", "-b", "main"]);
        // No commit at all: the history is empty, so `over_budget` can only come from the
        // working-tree pass.
        let body = format!("{}\nkey = {AWS}\n", "x".repeat(4000));
        for i in 0..FILES {
            std::fs::write(d.path().join(format!("f{i}.log")), &body).unwrap();
        }
        let per = std::fs::metadata(d.path().join("f0.log")).unwrap().len();
        // The one that fails to read as UTF-8, at the same size as a text file — 0xFF is always
        // illegal in UTF-8.
        std::fs::write(d.path().join("blob.bin"), vec![0xFFu8; per as usize]).unwrap();
        let repo = crate::domain::repo::Repo::at(d.path());
        let plan = |budget: u64| ScanPlan {
            dest: Destination::Unknown,
            limits: ScanLimits {
                budget_bytes: budget,
                ..ScanLimits::DEFAULT
            },
        };

        // Precondition: with budget to spare every one of these files is readable and no hit is
        // missing. The per-file cap takes no part in this test at any point — it cannot stop
        // "each compliant, together over the limit".
        let loose = scan_agent_repo(&repo, &plan(u64::MAX)).expect("the repo is healthy");
        assert_eq!(
            loose
                .hits
                .iter()
                .filter(|h| h.source == Source::File)
                .count(),
            FILES,
            "precondition fails: not all of these files yield a secret in the first place: {:?}",
            loose.hits
        );
        assert!(
            loose.unscanned.oversized_files.is_empty(),
            "precondition fails: these files must not reach the per-file cap, or a different bound is what gets tested: {:?}",
            loose.unscanned
        );

        // 1. Budget enough for three files: it **stops**, and the secrets in the files behind
        //    cannot be reported.
        let tight =
            scan_agent_repo(&repo, &plan(3 * per)).expect("over budget is not a scan failure");
        assert!(
            tight.unscanned.over_budget.is_some(),
            "crossing the budget is passed out explicitly: {:?}",
            tight.unscanned
        );
        assert!(
            tight
                .hits
                .iter()
                .filter(|h| h.source == Source::File)
                .count()
                < FILES,
            "it kept reading past the budget: a pile of files under the per-file cap ran straight \
             through it. {} hits reported, which means all {FILES} files were read: {:?}",
            tight.hits.len(),
            tight.hits
        );

        // 2. Budget set exactly between "all the text files" and "plus the binary one": a file
        //    that failed to read has to be booked too, or this pass reads a whole file for
        //    nothing and still cannot report crossing the line.
        let all_text = FILES as u64 * per;
        let edge = scan_agent_repo(&repo, &plan(all_text + per / 2))
            .expect("over budget is not a scan failure");
        assert!(
            edge.unscanned.over_budget.is_some(),
            "a file that fails to read as UTF-8 was still read — its cost is booked into the \
             budget, or one `continue` lets any number of bytes through for free: {:?}",
            edge.unscanned
        );
    }
}
