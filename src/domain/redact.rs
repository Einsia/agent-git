//! Content redaction: erasing the coordinates before publishing.
//!
//! Secret replacement is delegated to the local privacy service, which persists reversible
//! mappings before returning replacements. Text the service did not protect in full is never
//! passed on: it is withheld, and the reader sees [`WITHHELD`] instead. Persona masking remains
//! independent.
//!
//! # Why stand-ins and not truncation
//!
//! The referential integrity of a transcript is worth more than any single fact: the same
//! absolute path recurs in the cwd field, in tool arguments and in error stacks, and deleting
//! them or replacing each with a different placeholder makes a conversation unreadable. So there
//! are only two rules —
//!
//! 1. **Every occurrence of one entity maps to the same stand-in** (`nana` is always
//!    `/home/nana` and always the `nana` on a command line; together they become `~` / `user`).
//! 2. **The mapping is deterministic**: the same input always produces the same output. That is
//!    what makes "append and rerun" possible — redacting again once the transcript has grown
//!    leaves the old prefix byte-for-byte unchanged, so a continuing push is not stopped by a
//!    byte comparison.
//!
//! # Order preservation
//!
//! Stand-ins are numbered in **order of first appearance** (`~user1`, `[ip1]`), not sorted and
//! not hashed: a hash does not depend on the order of first appearance, but it needs consensus
//! across the whole table (the same stand-in name has to agree on two machines), while
//! first-appearance order is the stronger one on the property we actually care about — the
//! prefix does not change.
//!
//! # What this deliberately does not do
//!
//! - Email addresses are left alone. A public git history normally carries the author email, and
//!   erasing every one on sight costs more than that — but once the local username becomes
//!   `user`, `nana@x.com` turns into `user@x.com` on its own.
//! - Private / loopback / documentation-range IPs are left alone (10.0.0.0/8, 192.168.0.0/16,
//!   172.16.0.0/12, 127.0.0.0/8, 169.254.0.0/16, and the DNS addresses every tutorial in the
//!   world uses). They locate nobody, and erasing them only turns "cannot reach 10.x" in a log
//!   into nonsense.
//! - No NER. What structural rules can take (paths, secrets, IPs) is taken by structural rules;
//!   what they cannot (person names, organization names, the business discussed in a chat) must
//!   not be guessed at — a redactor that pretends to read semantics leaves what it missed
//!   looking like it has "already been scrubbed".

use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
#[cfg(feature = "cli")]
use zeroize::Zeroizing;

/// What a reader sees in place of text whose privacy check did not complete.
pub const WITHHELD: &str = "[privacy check pending: text withheld]";

/// How much undelivered stream text waits for the privacy worker to return before it is
/// dropped; a live item that outlasts the worker's absence shows [`WITHHELD`] once instead.
#[cfg(feature = "cli")]
const STREAM_HOLD_BYTES: usize = 1024 * 1024;

/// What one redaction pass produced.
#[derive(Debug, Clone)]
pub struct Report {
    pub text: String,
    /// Number of secret hits.
    pub secrets: usize,
    /// Total occurrences of paths / usernames / hostnames replaced.
    pub paths: usize,
    /// Occurrences of public IPs replaced.
    pub ips: usize,
    /// Opaque ids of the device-local registered rules. Only for the local session supervisor to
    /// deduplicate warnings; never written to a log, never sent over the network with ordinary
    /// RC events.
    pub registered_ids: Vec<String>,
}

/// JSON form of [`Report`]. Matching happens on decoded strings so a secret
/// containing quotes, backslashes or newlines has exactly the same semantics as
/// it does in the runtime, rather than being compared with JSON escape bytes.
#[derive(Debug, Clone)]
pub struct JsonReport {
    pub value: serde_json::Value,
    pub secrets: usize,
    pub paths: usize,
    pub ips: usize,
    pub registered_ids: Vec<String>,
}

#[cfg(feature = "cli")]
impl JsonReport {
    /// A value whose privacy check did not complete: `{"protection_error": …}` in place of the
    /// value, never the value itself. Remote-control consumers that recognize the key show
    /// [`PROTECTION_ERROR_TEXT`] and drop the raw record; any other consumer receives only the
    /// marker.
    fn withheld() -> Self {
        Self {
            value: serde_json::json!({ "protection_error": PROTECTION_ERROR_TEXT }),
            secrets: 0,
            paths: 0,
            ips: 0,
            registered_ids: vec![],
        }
    }
}

#[cfg(feature = "rc")]
pub(crate) struct NativeJson {
    pub value: serde_json::Value,
    pub secret_projection: bool,
    pub registered_ids: Vec<String>,
    /// Whether a withheld record can never pass, so retrying it is pointless.
    pub permanent: bool,
    /// Whether the privacy check ran on this withheld record and failed, with a deadline or a
    /// protocol error, rather than not running for contention or a back-off. Only such failures
    /// count toward giving a record up.
    pub failed: bool,
    /// Why this withheld record's check did not complete, when the worker said.
    pub reason: Option<crate::domain::privacy::projector::Reason>,
}

#[cfg(feature = "rc")]
impl NativeJson {
    /// A record whose privacy check did not complete: the failure marker, never the record.
    fn withheld() -> Self {
        Self {
            value: serde_json::json!({ "protection_error": PROTECTION_ERROR_TEXT }),
            secret_projection: true,
            registered_ids: Vec::new(),
            permanent: false,
            failed: false,
            reason: None,
        }
    }

    /// A record too large for one protection unit: withheld, and never worth retrying.
    fn withheld_permanently() -> Self {
        Self {
            value: serde_json::json!({ "protection_error": TOO_LARGE_TEXT }),
            permanent: true,
            ..Self::withheld()
        }
    }
}

#[cfg(feature = "rc")]
pub(crate) const PROTECTION_ERROR_TEXT: &str = "[privacy check pending: content withheld]";

/// What a reader sees in place of a record too large for the privacy check ever to pass.
#[cfg(feature = "rc")]
pub(crate) const TOO_LARGE_TEXT: &str = "[content withheld: too large for the privacy check]";

/// What a reader sees in place of a record whose privacy check was given up on: it is not
/// retried, unlike [`PROTECTION_ERROR_TEXT`].
#[cfg(feature = "rc")]
pub(crate) const GIVEN_UP_TEXT: &str = "[content withheld: its privacy check did not finish]";

#[cfg(all(test, feature = "rc"))]
thread_local! {
    /// Makes the native records on this thread whose JSON contains this text transiently
    /// withheld (every record for `""`), so tests can exercise the paths that hold and retry such
    /// records; unit tests cannot host the privacy worker.
    pub(crate) static WITHHOLD_NATIVE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
    /// The reason the records [`WITHHOLD_NATIVE`] withholds report: a protocol error unless this
    /// names another, such as the lock contention that only delays a record.
    pub(crate) static WITHHOLD_NATIVE_REASON: std::cell::Cell<
        Option<crate::domain::privacy::projector::Reason>,
    > = const { std::cell::Cell::new(None) };
    /// Makes the text and JSON checks on this thread whose input contains this text come back
    /// incomplete for this reason, as they do while the privacy worker is busy or backing off.
    pub(crate) static WITHHOLD_LIVE: std::cell::Cell<
        Option<(&'static str, crate::domain::privacy::projector::Reason)>,
    > = const { std::cell::Cell::new(None) };
}

#[cfg(all(test, feature = "rc"))]
fn withheld_live(input: &str) -> Option<Withheld> {
    WITHHOLD_LIVE
        .with(std::cell::Cell::get)
        .filter(|(needle, _)| input.contains(needle))
        .map(|(_, reason)| Withheld {
            reason: Some(reason),
        })
}

/// A text or value whose privacy check did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Withheld {
    /// What the worker reported; none when it gave no reason.
    #[cfg(feature = "cli")]
    pub(crate) reason: Option<crate::domain::privacy::projector::Reason>,
}

impl Withheld {
    #[cfg(feature = "cli")]
    fn of(outcome: &crate::domain::privacy::projector::Outcome) -> Self {
        Self {
            reason: outcome.reason,
        }
    }
}

impl std::fmt::Display for Withheld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        #[cfg(feature = "cli")]
        {
            use crate::domain::privacy::{projector::Reason, service::reason_name};
            let detail = match self.reason {
                Some(Reason::Deadline) => "the processing deadline expired",
                Some(Reason::Oversize) => {
                    "the content cannot be safely divided within the size limit"
                }
                Some(Reason::UnsupportedScalar) => {
                    "a blocked numeric or boolean data value cannot use string-only recovery; use a string value or review the privacy rule"
                }
                Some(Reason::DeviceRules | Reason::LegacyRules | Reason::SourceIncomplete) => {
                    "privacy rules could not be fully loaded; restore access to the rules"
                }
                Some(Reason::LockBusy | Reason::StoreBusy) => {
                    "the privacy worker or dictionary is unavailable"
                }
                Some(Reason::Backoff) => "the privacy worker is waiting after a failure",
                Some(Reason::SpawnFailed) => "the privacy worker could not start",
                Some(Reason::ProtocolError) => "the privacy worker returned an invalid response",
                Some(Reason::FindingsCap) => "the privacy scan exceeded its findings limit",
                Some(Reason::UnheldPlaceholders) => {
                    "it holds placeholders this computer never made, so they stay as they are"
                }
                None => "the worker did not report a reason",
            };
            write!(
                f,
                "privacy check failed ({}): {detail}",
                reason_name(self.reason)
            )
        }
        #[cfg(not(feature = "cli"))]
        write!(f, "privacy check did not complete")
    }
}

impl std::error::Error for Withheld {}

/// The device-local persona: "who this machine is", read out of the environment.
#[derive(Debug, Clone, Default)]
pub struct Persona {
    pub username: Option<String>,
    pub home: Option<String>,
    pub hostname: Option<String>,
}

impl Persona {
    /// Bootstraps from the environment: USER/LOGNAME, HOME, HOSTNAME (or the first line of
    /// /etc/hostname).
    pub fn this_machine() -> Self {
        let username = std::env::var("USER")
            .ok()
            .or_else(|| std::env::var("LOGNAME").ok())
            .filter(|s| !s.is_empty());
        let home = std::env::var("HOME").ok().filter(|s| s.starts_with('/'));
        let hostname = std::env::var("HOSTNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .and_then(|t| t.lines().next().map(str::trim).map(String::from))
                    .filter(|s| !s.is_empty())
            });
        Persona {
            username,
            home,
            hostname,
        }
    }
}

#[derive(Clone)]
pub struct Redactor {
    persona: Persona,
    repo: Option<std::path::PathBuf>,
    preserve_device_identity: bool,
    #[cfg(feature = "rc")]
    native: std::sync::Arc<std::sync::Mutex<(String, String)>>,
}

/// The name appearing in `/home/<name>` / `/Users/<name>`. The reserved macOS shared
/// directories are not people.
static HOME_USER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(?:home|Users)/([A-Za-z0-9][A-Za-z0-9._-]*)").unwrap());
static WIN_USER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)([A-Za-z]:[\\/]Users[\\/])([A-Za-z0-9][A-Za-z0-9._-]*)").unwrap()
});
static IPV4_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b(?:[0-9]{1,3}\.){3}[0-9]{1,3}\b").unwrap());

/// IPs that are not redacted: private, loopback, link-local and documentation ranges, plus the
/// public DNS every tutorial uses.
fn public_ip(ip: &str) -> bool {
    let parts: Vec<u8> = match ip
        .split('.')
        .map(|p| p.parse::<u8>())
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) if v.len() == 4 => v,
        _ => return false,
    };
    let [a, b, c, d] = [parts[0], parts[1], parts[2], parts[3]];
    if a == 127 || a == 10 || a == 0 || a >= 224 {
        return false; // loopback / private / 0.x / multicast and reserved ranges
    }
    if a == 192 && b == 168 {
        return false; // 192.168.0.0/16
    }
    if a == 172 && (16..=31).contains(&b) {
        return false; // 172.16.0.0/12
    }
    if a == 169 && b == 254 {
        return false; // link-local
    }
    if a == 192 && b == 0 && c == 2 {
        return false; // TEST-NET-1 documentation range
    }
    // The public DNS that is everywhere in docs and tutorials: erasing it only hurts
    // readability and locates nobody.
    if matches!(
        (a, b, c, d),
        (8, 8, 8, 8) | (8, 8, 4, 4) | (1, 1, 1, 1) | (1, 0, 0, 1)
    ) {
        return false;
    }
    true
}

/// Collects (start, end, replacement) and applies them to the original text in reverse order.
///
/// Matching always runs on the escape view from [`secrets::view_of`] (which handles the boundary
/// lost to the two-character `\n` in jsonl); a span corresponds to the original at equal length,
/// so the replacement lands on the same range of the original.
fn apply_spans(out: &mut String, spans: &[(usize, usize, String)], count: &mut usize) {
    #[cfg(feature = "secret-vault")]
    let opaque: Vec<_> = crate::domain::privacy::projector::tokens(out)
        .map(|(s, e, _)| (s, e))
        .collect();
    for (s, e, r) in spans.iter().rev() {
        #[cfg(feature = "secret-vault")]
        if opaque.iter().any(|(start, end)| *s < *end && *e > *start) {
            continue;
        }
        out.replace_range(*s..*e, r);
        *count += 1;
    }
}

/// Replacement wrapped in word boundaries (the regex crate has no lookaround, so the boundary is
/// "capture what precedes + check what follows by hand").
fn replace_token(text: &str, token: &str, with: &str, count: &mut usize) -> String {
    // Boundary character set: path separators, dots and colons all count as "outside", so
    // /etc/<user>, <user>@host and <user>:<group> all match while <user>name and my<user> do
    // not.
    let re = Regex::new(&format!(r"(^|[^A-Za-z0-9._-]){}", regex::escape(token))).unwrap();
    let view = crate::domain::secrets::view_of(text);
    let mut spans: Vec<(usize, usize, String)> = vec![];
    let mut pos = 0;
    while let Some(m) = re.find_at(&view, pos) {
        let tok_start = m.end() - token.len();
        // Trailing boundary: an alphanumeric / dot / underscore / hyphen right after the
        // token means the token is only part of a longer string.
        let after = view[m.end()..].chars().next();
        if let Some(ch) = after
            && (ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            pos = m.end();
            continue;
        }
        spans.push((tok_start, m.end(), with.to_string()));
        // The next token's boundary character may be exactly the separator this match
        // consumed (the colon in `nana:nana`) — resume from the separator itself, never past
        // it.
        pos = tok_start + token.len();
    }
    let mut out = text.to_string();
    apply_spans(&mut out, &spans, count);
    out
}

impl Redactor {
    pub fn new(persona: Persona) -> Self {
        Redactor {
            persona,
            repo: None,
            preserve_device_identity: false,
            #[cfg(feature = "rc")]
            native: Default::default(),
        }
    }

    pub fn for_repository(mut self, repo: Option<&std::path::Path>) -> Self {
        self.repo = repo.map(std::path::Path::to_owned);
        self
    }

    pub fn with_repository(self, root: &std::path::Path) -> crate::Result<Self> {
        Ok(self.for_repository(Some(root)))
    }

    /// [`Self::scrub`], with text whose privacy check did not complete reported as an error
    /// instead of withheld.
    pub fn try_scrub(&self, text: &str) -> crate::Result<Report> {
        self.protect_text(text)
            .map_err(|_| anyhow::anyhow!("privacy check pending; the text was not protected"))
    }
    /// [`Self::scrub_json`], with a value whose privacy check did not complete reported as an
    /// error instead of withheld.
    pub fn try_scrub_json(&self, value: &serde_json::Value) -> crate::Result<JsonReport> {
        self.protect_json(value)
            .map_err(|_| anyhow::anyhow!("privacy check pending; the value was not protected"))
    }

    /// Publication must finish protection before writing output. Its processing allowance grows
    /// with the input, and a refusal retains the worker's reason without exposing its content.
    #[cfg(feature = "cli")]
    pub(crate) fn try_scrub_json_durable(
        &self,
        value: &serde_json::Value,
    ) -> crate::Result<JsonReport> {
        self.protect_json_with_budget(value, true)
            .map_err(anyhow::Error::new)
    }

    /// The redactor for what remote control shows: a person's own sessions, seen live by them
    /// or by the colleagues in their workspace. It keeps this device's paths and names as they
    /// are, and it runs no privacy check, so nothing it passes is held back or replaced by a
    /// placeholder. What leaves this device to be kept where others can read it goes through
    /// `agit save`, turn-end settlement or push, which check it in full with a redactor of their
    /// own.
    #[cfg(feature = "rc")]
    pub(crate) fn for_device_control(mut self) -> Self {
        self.preserve_device_identity = true;
        self
    }

    #[cfg(feature = "rc")]
    pub(crate) fn bind_native_session(&self, session: &str) {
        if let Ok(mut native) = self.native.try_lock() {
            native.1 = session.into();
        }
    }
    #[cfg(feature = "secret-vault")]
    pub(crate) fn publication_values_in(
        &self,
        texts: &[&str],
    ) -> crate::Result<std::collections::BTreeSet<String>> {
        let tokens = texts
            .iter()
            .flat_map(|text| crate::domain::privacy::projector::tokens(text))
            .map(|(_, _, token)| token)
            .collect::<std::collections::BTreeSet<_>>();
        if tokens.is_empty() {
            return Ok(Default::default());
        }
        #[cfg(feature = "cli")]
        {
            let values = crate::domain::privacy::service::manage(
                self.repo.as_deref(),
                crate::domain::privacy::management::Command {
                    action: "used_values".into(),
                    global: self.repo.is_none(),
                    id: None,
                    name: None,
                    secret: Some(tokens.into_iter().collect::<Vec<_>>().join("\n")),
                },
            )?;
            Ok(serde_json::from_value(values)?)
        }
        #[cfg(not(feature = "cli"))]
        {
            Ok(Default::default())
        }
    }

    #[cfg(feature = "rc")]
    pub(crate) fn scrub_native_batch_profiled(
        &self,
        records: &[(&serde_json::Value, &[&str])],
        timing: &mut dyn FnMut(&'static str, f64),
    ) -> Vec<NativeJson> {
        if self.preserve_device_identity {
            return records
                .iter()
                .map(|(value, _)| NativeJson {
                    value: (*value).clone(),
                    secret_projection: false,
                    registered_ids: Vec::new(),
                    permanent: false,
                    failed: false,
                    reason: None,
                })
                .collect();
        }
        let started = std::time::Instant::now();
        let native = self
            .native
            .try_lock()
            .map(|native| native.clone())
            .unwrap_or_default();
        // A request is the batch serialized whole and stays within one protection unit: the
        // batch without records, each record with its pointers, and two separators for every
        // entry after the first. A record that does not fit alone can never pass, so it is
        // withheld for good, while the others are protected in groups that fit.
        let base = serde_json::to_string(&crate::domain::privacy::projector::NativeBatch {
            runtime: native.0.clone(),
            session: native.1.clone(),
            records: Vec::new(),
            pointers: Vec::new(),
        })
        .map_or(usize::MAX, |text| text.len());
        let limit = crate::domain::privacy::projector::MAX_UNIT_BYTES;
        let mut out: Vec<Option<NativeJson>> = records.iter().map(|_| None).collect();
        let mut group = Vec::new();
        let mut group_bytes = base;
        for (index, (value, pointers)) in records.iter().enumerate() {
            let bytes = [
                serde_json::to_string(value).map(|text| text.len()),
                serde_json::to_string(pointers).map(|text| text.len()),
            ]
            .into_iter()
            .try_fold(0usize, |sum, part| {
                part.map(|part| sum.saturating_add(part))
            })
            .unwrap_or(usize::MAX);
            if base.saturating_add(bytes) > limit {
                out[index] = Some(NativeJson::withheld_permanently());
                continue;
            }
            if !group.is_empty() && group_bytes.saturating_add(bytes + 2) > limit {
                self.protect_native_group(&native, records, &std::mem::take(&mut group), &mut out);
                group_bytes = base;
            }
            group_bytes += if group.is_empty() { bytes } else { bytes + 2 };
            group.push(index);
        }
        self.protect_native_group(&native, records, &group, &mut out);
        timing(
            "dictionary_protect_ms",
            started.elapsed().as_secs_f64() * 1000.0,
        );
        out.into_iter()
            .map(|record| record.unwrap_or_else(NativeJson::withheld))
            .collect()
    }

    /// Protect `records[group]` in one request and store each result at its index in `out`.
    #[cfg(feature = "rc")]
    fn protect_native_group(
        &self,
        native: &(String, String),
        records: &[(&serde_json::Value, &[&str])],
        group: &[usize],
        out: &mut [Option<NativeJson>],
    ) {
        if group.is_empty() {
            return;
        }
        let batch = crate::domain::privacy::projector::NativeBatch {
            runtime: native.0.clone(),
            session: native.1.clone(),
            records: group.iter().map(|&i| records[i].0.clone()).collect(),
            pointers: group
                .iter()
                .map(|&i| records[i].1.iter().map(|p| (*p).to_owned()).collect())
                .collect(),
        };
        let projected = serde_json::to_string(&batch).ok().map(|input| {
            crate::domain::privacy::service::transform_live(
                self.repo.as_deref(),
                &input,
                crate::domain::privacy::projector::Mode::ProtectNative,
            )
        });
        #[cfg(test)]
        let projected = projected.filter(|_| {
            !WITHHOLD_NATIVE
                .with(std::cell::Cell::get)
                .is_some_and(|text| {
                    batch
                        .records
                        .iter()
                        .any(|record| record.to_string().contains(text))
                })
        });
        // A batch that cannot be put into a request is as malformed as a reply that cannot be read.
        let reason = projected.as_ref().map_or(
            Some(crate::domain::privacy::projector::Reason::ProtocolError),
            |outcome| outcome.reason,
        );
        #[cfg(test)]
        let reason = match WITHHOLD_NATIVE_REASON.with(std::cell::Cell::get) {
            Some(withheld) if projected.is_none() => Some(withheld),
            _ => reason,
        };
        // Only a check that ran and ended with a deadline or a protocol error is this group's
        // failure; contention, a back-off or a busy dictionary only delays it.
        let failed = matches!(
            reason,
            Some(
                crate::domain::privacy::projector::Reason::Deadline
                    | crate::domain::privacy::projector::Reason::ProtocolError
            )
        );
        let values = projected
            .filter(crate::domain::privacy::service::passed)
            .and_then(|outcome| {
                if outcome.status == crate::domain::privacy::projector::Status::Skipped {
                    // Only a unit-test binary passes an unprocessed outcome: its worker cannot
                    // run, and the content is the batch itself.
                    return Some(batch.records.clone());
                }
                serde_json::from_str::<Vec<serde_json::Value>>(&outcome.content).ok()
            })
            .filter(|values| values.len() == group.len());
        let Some(values) = values else {
            // One record must not keep the others from passing: each is tried on its own.
            if group.len() > 1 {
                for &index in group {
                    self.protect_native_group(native, records, &[index], out);
                }
            } else {
                out[group[0]] = Some(NativeJson {
                    failed,
                    reason,
                    ..NativeJson::withheld()
                });
            }
            return;
        };
        for (mut value, &index) in values.into_iter().zip(group) {
            let secret_projection = value != *records[index].0;
            self.scrub_json_inner(&mut value, &mut JsonTotals::default());
            out[index] = Some(NativeJson {
                value,
                secret_projection,
                registered_ids: Vec::new(),
                permanent: false,
                failed: false,
                reason: None,
            });
        }
    }

    pub fn this_machine() -> Self {
        Self::new(Persona::this_machine())
    }

    pub fn try_this_machine() -> crate::Result<Self> {
        Ok(Self::this_machine())
    }

    pub fn stream(&self) -> StreamRedactor {
        StreamRedactor {
            redactor: self.clone(),
            pending: String::new(),
            withheld: false,
            #[cfg(feature = "cli")]
            released: String::new(),
        }
    }

    /// Redact one text. Deterministic: same input, same persona ⇒ same output. Text whose
    /// privacy check does not complete becomes [`WITHHELD`].
    pub fn scrub(&self, text: &str) -> Report {
        self.protect_text(text)
            .unwrap_or_else(|_| withheld_report())
    }

    /// The text protected as one whole, or why its privacy check did not complete. Text is not
    /// records: a credential such as a PEM private key spans lines, and only a pass that sees the
    /// whole text replaces it from BEGIN through END.
    pub(crate) fn protect_text(&self, text: &str) -> Result<Report, Withheld> {
        self.protect_text_in("", text)
    }

    /// [`Self::protect_text`] for text that continues `context`, text a stream already
    /// released: rules match across the context, and only `text` is protected.
    fn protect_text_in(&self, context: &str, text: &str) -> Result<Report, Withheld> {
        if self.preserve_device_identity {
            return Ok(Report {
                text: text.to_owned(),
                ..empty_report()
            });
        }
        #[cfg(all(test, feature = "rc"))]
        if let Some(withheld) = withheld_live(text) {
            return Err(withheld);
        }
        #[cfg(feature = "cli")]
        let protected = if context.is_empty() {
            crate::domain::privacy::service::transform_live(
                self.repo.as_deref(),
                text,
                crate::domain::privacy::projector::Mode::ProtectText,
            )
        } else {
            crate::domain::privacy::service::transform_in(
                self.repo.as_deref(),
                context,
                text,
                crate::domain::privacy::projector::Mode::ProtectText,
            )
        };
        #[cfg(not(feature = "cli"))]
        let _ = context;
        #[cfg(feature = "cli")]
        if !crate::domain::privacy::service::passed(&protected) {
            return Err(Withheld::of(&protected));
        }
        #[cfg(feature = "cli")]
        let (out, secrets) = (protected.content, protected.replacements);
        #[cfg(not(feature = "cli"))]
        let (out, secrets) = (text.to_owned(), 0);
        let mut report = self.persona_only(&out);
        report.secrets = secrets;
        #[cfg(feature = "secret-vault")]
        if secrets > 0 {
            report.registered_ids = crate::domain::privacy::projector::tokens(&out)
                .map(|(_, _, token)| token.to_owned())
                .collect();
        }
        Ok(report)
    }

    /// Whether `name` can be published as it is: neither the detector, nor persona masking, nor
    /// a value the user registered would rewrite it. The detector and persona masking run in this
    /// process. Registered values live only in the privacy worker's dictionary, so the worker is
    /// asked too: a worker that does not answer gets one more try once its back-off is over, and
    /// if it still cannot answer the check fails with a message that says retrying works.
    #[cfg(feature = "cli")]
    pub(crate) fn publishes_unchanged(&self, name: &str) -> crate::Result<bool> {
        let scan = if crate::infra::config::privacy_entropy_replace() {
            crate::domain::privacy::detector::scan(name, 1)
        } else {
            crate::domain::privacy::detector::scan_high_confidence(name, 1)
        };
        if !scan.findings.is_empty() || self.persona_only(name).text != name {
            return Ok(false);
        }
        anyhow::ensure!(
            scan.complete,
            "the privacy check for `{name}` could not complete; retrying the push usually works"
        );
        let mut pending = None;
        for attempt in 0..2 {
            let protected = crate::domain::privacy::service::transform(
                self.repo.as_deref(),
                name,
                crate::domain::privacy::projector::Mode::ProtectText,
            );
            if crate::domain::privacy::service::passed(&protected) {
                return Ok(protected.content == name);
            }
            pending = Some(crate::domain::privacy::service::Pending::of(&protected));
            if attempt == 0 {
                crate::domain::privacy::service::wait_for_retry();
            }
        }
        Err(
            anyhow::Error::new(pending.expect("an attempt ran")).context(format!(
                "the privacy check for `{name}` could not run, so nothing was published"
            )),
        )
    }

    /// The text a session branch is named from: the first prompt as it is published, with every
    /// replaced value left out. `None` when the privacy check cannot run, so the caller names the
    /// branch without the prompt's words.
    #[cfg(feature = "cli")]
    pub(crate) fn branch_name_source(&self, prompt: &str) -> Option<String> {
        branch_text(prompt, |text| {
            let protected = crate::domain::privacy::service::transform(
                self.repo.as_deref(),
                text,
                crate::domain::privacy::projector::Mode::ProtectText,
            );
            crate::domain::privacy::service::passed(&protected).then_some(protected.content)
        })
    }

    /// Whether a branch name carries any part of `prompt` that publication replaces: a
    /// high-confidence finding or a value the person registered.
    #[cfg(feature = "cli")]
    pub(crate) fn name_carries_replaced(&self, name: &str, prompt: &str) -> crate::Result<bool> {
        let scan = crate::domain::privacy::detector::scan_high_confidence(prompt, 64);
        anyhow::ensure!(
            scan.complete,
            "the privacy check for `{name}` could not complete; retrying the push usually works"
        );
        let mut replaced: Vec<Zeroizing<String>> = scan
            .findings
            .iter()
            .map(|span| Zeroizing::new(prompt[span.start..span.end].to_owned()))
            .collect();
        let protected = crate::domain::privacy::service::transform(
            self.repo.as_deref(),
            prompt,
            crate::domain::privacy::projector::Mode::ProtectText,
        );
        if !crate::domain::privacy::service::passed(&protected) {
            return Err(
                anyhow::Error::new(crate::domain::privacy::service::Pending::of(&protected))
                    .context(format!(
                        "the privacy check for `{name}` could not run, so nothing was published"
                    )),
            );
        }
        let pieces = replaced_values(prompt, &protected.content, |tokens| {
            restore_all(tokens, |text| {
                let restored = crate::domain::privacy::service::transform(
                    self.repo.as_deref(),
                    text,
                    crate::domain::privacy::projector::Mode::HydrateText,
                );
                (crate::domain::privacy::service::passed(&restored)
                    && restored.replacements == tokens.len())
                .then(|| Zeroizing::new(restored.content))
            })
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the privacy check for `{name}` could not complete; retrying the push usually works"
            )
        })?;
        replaced.extend(pieces);
        Ok(carries(name, &replaced))
    }

    #[cfg(feature = "cli")]
    pub(crate) fn scrub_persona(&self, text: &str) -> Report {
        self.persona_only(text)
    }

    fn persona_only(&self, text: &str) -> Report {
        let mut paths = 0;
        let mut out = text.to_owned();
        let secrets = 0;
        let registered_ids = Vec::new();

        // ── 2. The full home prefix ──
        if let Some(home) = &self.persona.home
            && home.len() > 1
        {
            paths += out.matches(home.as_str()).count();
            out = out.replace(home.as_str(), "~");
        }

        // ── 3. /home/<name>, /Users/<name>, C:\Users\<name> ──
        let persona_user = self.persona.username.as_deref();
        let mut aliases: HashMap<String, usize> = HashMap::new();
        {
            let view = crate::domain::secrets::view_of(&out);
            for m in HOME_USER_RE.captures_iter(&view) {
                let name = m[1].to_string();
                if Some(name.as_str()) == persona_user
                    || matches!(name.as_str(), "Shared" | "Guest")
                {
                    continue;
                }
                let next = aliases.len() + 1;
                aliases.entry(name).or_insert(next);
            }
            let mut spans: Vec<(usize, usize, String)> = vec![];
            for c in HOME_USER_RE.captures_iter(&view) {
                let m = c.get(0).unwrap();
                let name = &c[1];
                let repl = if Some(name) == persona_user {
                    Some("~".to_string())
                } else {
                    aliases.get(name).map(|n| format!("~user{n}"))
                };
                if let Some(r) = repl {
                    spans.push((m.start(), m.end(), r));
                }
            }
            apply_spans(&mut out, &spans, &mut paths);

            let view = crate::domain::secrets::view_of(&out);
            let mut spans: Vec<(usize, usize, String)> = vec![];
            for c in WIN_USER_RE.captures_iter(&view) {
                let m = c.get(0).unwrap();
                let name = c[2].to_string();
                let repl = if Some(name.as_str()) == persona_user {
                    "~".to_string()
                } else if matches!(name.as_str(), "Shared" | "Guest") {
                    continue;
                } else {
                    let next = aliases.len() + 1;
                    let n = *aliases.entry(name).or_insert(next);
                    format!(r"C:\Users\user{n}")
                };
                spans.push((m.start(), m.end(), repl));
            }
            apply_spans(&mut out, &spans, &mut paths);
        }

        // ── 4. Bare username and hostname — outside /home too: chown user:group, ssh user@host ──
        if let Some(user) = persona_user
            && user.len() >= 3
        {
            out = replace_token(&out, user, "user", &mut paths);
        }
        if let Some(host) = &self.persona.hostname
            && host.len() >= 3
        {
            out = replace_token(&out, host, "host", &mut paths);
        }

        // ── 5. Public IPs ──
        let mut ips = 0usize;
        let mut ip_alias: HashMap<String, usize> = HashMap::new();
        // The stand-in table is collected in full before anything is replaced: replacing while
        // scanning makes the numbering of a later occurrence of the same IP drift.
        {
            let view = crate::domain::secrets::view_of(&out);
            for m in IPV4_RE.find_iter(&view) {
                let ip = m.as_str();
                if public_ip(ip) {
                    let next = ip_alias.len() + 1;
                    ip_alias.entry(ip.to_string()).or_insert(next);
                }
            }
            let mut spans: Vec<(usize, usize, String)> = vec![];
            for m in IPV4_RE.find_iter(&view) {
                if let Some(n) = ip_alias.get(m.as_str()) {
                    spans.push((m.start(), m.end(), format!("[ip{n}]")));
                }
            }
            apply_spans(&mut out, &spans, &mut ips);
        }

        Report {
            text: out,
            secrets,
            paths,
            ips,
            registered_ids,
        }
    }

    /// Scrub semantic JSON string values while preserving schema keys. This is the
    /// only safe boundary for serialized runtime events: matching the wire text
    /// would miss `\"`, `\\` and `\n` inside a registered literal.
    pub fn scrub_json(&self, value: &serde_json::Value) -> JsonReport {
        #[cfg(feature = "cli")]
        return self
            .protect_json(value)
            .unwrap_or_else(|_| JsonReport::withheld());
        #[cfg(not(feature = "cli"))]
        self.protect_json(value)
            .expect("without the privacy service every value passes")
    }

    /// The value with every string protected, or why its privacy check did not complete.
    pub(crate) fn protect_json(&self, value: &serde_json::Value) -> Result<JsonReport, Withheld> {
        self.protect_json_with_budget(value, false)
    }

    fn protect_json_with_budget(
        &self,
        value: &serde_json::Value,
        durable: bool,
    ) -> Result<JsonReport, Withheld> {
        if self.preserve_device_identity {
            return Ok(JsonReport {
                value: value.clone(),
                secrets: 0,
                paths: 0,
                ips: 0,
                registered_ids: vec![],
            });
        }
        #[cfg(all(test, feature = "rc"))]
        if let Some(withheld) = withheld_live(&value.to_string()) {
            return Err(withheld);
        }
        #[cfg(feature = "cli")]
        let transform = if durable {
            crate::domain::privacy::service::transform
        } else {
            crate::domain::privacy::service::transform_live
        };
        #[cfg(feature = "cli")]
        let protected = transform(
            self.repo.as_deref(),
            &value.to_string(),
            crate::domain::privacy::projector::Mode::ProtectJsonl,
        );
        #[cfg(feature = "cli")]
        if !crate::domain::privacy::service::passed(&protected) {
            return Err(Withheld::of(&protected));
        }
        #[cfg(feature = "cli")]
        let mut value: serde_json::Value =
            serde_json::from_str(&protected.content).map_err(|_| Withheld {
                reason: Some(crate::domain::privacy::projector::Reason::ProtocolError),
            })?;
        #[cfg(not(feature = "cli"))]
        let _ = durable;
        #[cfg(not(feature = "cli"))]
        let mut value = value.clone();
        let mut totals = JsonTotals::default();
        #[cfg(feature = "cli")]
        {
            totals.secrets = protected.replacements;
            if protected.replacements > 0 {
                totals.registered_ids.extend(
                    crate::domain::privacy::projector::tokens(&protected.content)
                        .map(|(_, _, token)| token.to_owned()),
                );
            }
        }
        self.scrub_json_inner(&mut value, &mut totals);
        Ok(JsonReport {
            value,
            secrets: totals.secrets,
            paths: totals.paths,
            ips: totals.ips,
            registered_ids: totals.registered_ids.into_iter().collect(),
        })
    }

    fn scrub_json_inner(&self, value: &mut serde_json::Value, totals: &mut JsonTotals) {
        match value {
            serde_json::Value::String(text) => {
                let report = self.persona_only(text);
                totals.add(&report);
                *text = report.text;
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    self.scrub_json_inner(value, totals);
                }
            }
            serde_json::Value::Object(map) => {
                for value in map.values_mut() {
                    self.scrub_json_inner(value, totals);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
}

#[derive(Default)]
struct JsonTotals {
    secrets: usize,
    paths: usize,
    ips: usize,
    registered_ids: std::collections::HashSet<String>,
}

impl JsonTotals {
    fn add(&mut self, report: &Report) {
        self.secrets = self.secrets.saturating_add(report.secrets);
        self.paths = self.paths.saturating_add(report.paths);
        self.ips = self.ips.saturating_add(report.ips);
        self.registered_ids
            .extend(report.registered_ids.iter().cloned());
    }
}

/// A bounded held tail gives the worker lookahead across transport chunks, and every request
/// carries the text released before it as context, so a value is recognized even when its
/// name, keyword or command line left in an earlier release; text leaves in whole lines. Text
/// leaves only once the worker has processed it: while the worker is unavailable the text waits,
/// and text that waits too long, or is still unprocessed when the item ends, is withheld rather
/// than released. Once a stream withholds text it withholds the rest of the item too: the
/// dropped text may have opened a secret, such as the BEGIN line of a key, that what follows
/// cannot be recognized without. Released text never ends inside a private key: the worker holds
/// a key back from its BEGIN line until its footer is complete, and if released text opens a key
/// anyway, or the item ends inside one, the rest of the item is withheld.
pub struct StreamRedactor {
    redactor: Redactor,
    pending: String,
    /// Whether this stream withheld text, after which it delivers nothing but one marker.
    withheld: bool,
    /// The last text this stream released, as it was before protection: the context every
    /// request carries.
    #[cfg(feature = "cli")]
    released: String,
}

impl StreamRedactor {
    pub fn push(&mut self, chunk: &str) -> Report {
        if self.withheld {
            return empty_report();
        }
        self.pending.push_str(chunk);
        if self.redactor.preserve_device_identity {
            return self.flush();
        }
        #[cfg(feature = "cli")]
        {
            // Text that waits beyond the hold, processed or not, is withheld with the rest of
            // the item; this also keeps every request below one protection unit.
            if self.pending.len() > STREAM_HOLD_BYTES {
                return self.withhold();
            }
            let outcome = crate::domain::privacy::service::transform_in(
                self.redactor.repo.as_deref(),
                &self.released,
                &self.pending,
                crate::domain::privacy::projector::Mode::ProtectStream,
            );
            // Only text the worker protected with the whole policy leaves; anything else waits.
            let processed = crate::domain::privacy::service::passed(&outcome)
                .then(|| outcome.consumed.unwrap_or(self.pending.len()));
            let Some(consumed) = processed else {
                return empty_report();
            };
            if consumed > self.pending.len() || !self.pending.is_char_boundary(consumed) {
                return self.withhold();
            }
            let released = format!("{}{}", self.released, &self.pending[..consumed]);
            if opens_key(&released) {
                return self.withhold();
            }
            let Some(keep) = context_start(&released) else {
                return self.withhold();
            };
            self.released = released[keep..].to_owned();
            self.pending.drain(..consumed);
            let mut report = self.redactor.persona_only(&outcome.content);
            report.secrets = outcome.replacements;
            if outcome.replacements > 0 {
                report.registered_ids = crate::domain::privacy::projector::tokens(&outcome.content)
                    .map(|(_, _, token)| token.to_owned())
                    .collect();
            }
            report
        }
        #[cfg(not(feature = "cli"))]
        self.flush()
    }

    pub fn flush(&mut self) -> Report {
        if self.withheld || self.pending.is_empty() {
            self.pending.clear();
            return empty_report();
        }
        let ready = std::mem::take(&mut self.pending);
        if self.redactor.preserve_device_identity {
            return Report {
                text: ready,
                ..empty_report()
            };
        }
        #[cfg(feature = "cli")]
        {
            // The item ends here, so a key it opened and never closed is withheld whole,
            // whatever the detector made of it.
            let released = std::mem::take(&mut self.released);
            if opens_key(&format!("{released}{ready}")) {
                return self.withhold();
            }
            self.redactor
                .protect_text_in(&released, &ready)
                .unwrap_or_else(|_| self.withhold())
        }
        #[cfg(not(feature = "cli"))]
        self.redactor
            .protect_text(&ready)
            .unwrap_or_else(|_| self.withhold())
    }

    /// [`WITHHELD`], once: the stream delivers nothing more for this item.
    fn withhold(&mut self) -> Report {
        self.pending.clear();
        if std::mem::replace(&mut self.withheld, true) {
            empty_report()
        } else {
            withheld_report()
        }
    }
}

/// How much released text the stream keeps at least and sends with each request as context, so
/// a rule whose name or keyword left in an earlier release still recognizes the value that
/// follows, and a key header split across two releases is still seen whole.
#[cfg(feature = "cli")]
const STREAM_CONTEXT_BYTES: usize = 16 * 1024;

/// How many line breaks back a command can sit before a header that completes it: the curl
/// header rules let up to five continuation lines, each after one or two line breaks (a blank
/// line), and one more break come between `curl` and `-H "Authorization: …"`, with any text on
/// those lines.
#[cfg(feature = "cli")]
const CONTEXT_LINE_BREAKS: usize = 11;

/// The most context one request carries, so the context and the held text still fit one unit.
#[cfg(feature = "cli")]
const STREAM_CONTEXT_CAP: usize =
    crate::domain::privacy::projector::MAX_UNIT_BYTES - 2 * STREAM_HOLD_BYTES;

/// Where the context the next request carries starts in `released`: [`STREAM_CONTEXT_BYTES`]
/// back, and further back only to the last command within [`CONTEXT_LINE_BREAKS`] line breaks of
/// the end, since only a command's header can complete a match that far from its start. `None`
/// when that command lies beyond [`STREAM_CONTEXT_CAP`], so a header still to come could no
/// longer be recognized.
#[cfg(feature = "cli")]
pub(crate) fn context_start(released: &str) -> Option<usize> {
    static COMMAND: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bcurl\b").expect("the command pattern is valid"));
    let ceil = |mut at: usize| {
        while !released.is_char_boundary(at) {
            at += 1;
        }
        at
    };
    let window = ceil(released.len().saturating_sub(STREAM_CONTEXT_BYTES));
    let mut line = released.len();
    // The final line break ends the current line; the ones before it bound the lines a command
    // may have started on.
    for _ in 0..=CONTEXT_LINE_BREAKS {
        match released[..line].rfind('\n') {
            Some(at) => line = at,
            None => {
                line = 0;
                break;
            }
        }
    }
    let reach = if line == 0 { 0 } else { line + 1 };
    if reach >= window {
        return Some(window);
    }
    // Matched over the text that follows too, so a window edge inside the command's name still
    // finds the command that starts before it.
    let Some(start) = COMMAND
        .find_iter(&released[reach..])
        .map(|command| reach + command.start())
        .take_while(|&at| at < window)
        .last()
    else {
        return Some(window);
    };
    (start >= released.len().saturating_sub(STREAM_CONTEXT_CAP)).then_some(start)
}

/// A private key's header; its footer is the same line with `END` for `BEGIN`. The shortest
/// header at each place is the one taken, so a header found in the whole text is the one found in
/// any prefix that holds it.
#[cfg(feature = "cli")]
static KEY_HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)-----BEGIN[ A-Z0-9_-]{0,100}?PRIVATE KEY(?: BLOCK)?-----")
        .expect("the private key header pattern is valid")
});

/// The private keys of one text, found once: each header's end and where its key closes, just
/// past the first matching footer after the header, or `None` when no footer follows. A key whose
/// footer ends the text is closed there; one with no footer is open to the end and beyond it.
#[cfg(feature = "cli")]
pub(crate) struct PrivateKeys {
    /// In the order of their headers, which is also the order of the headers' ends.
    keys: Vec<(usize, Option<usize>)>,
    len: usize,
}

#[cfg(feature = "cli")]
impl PrivateKeys {
    pub(crate) fn new(text: &str) -> Self {
        let lower = text.to_ascii_lowercase();
        // Every footer-shaped line, by its text, at every place one starts. Each `-----end` is
        // looked at once over a bounded stretch, so the work grows with the text, not with the
        // number of keys times its length.
        let mut footers: HashMap<&str, Vec<usize>> = HashMap::new();
        for (at, _) in lower.match_indices("-----end") {
            let rest = &lower[at + "-----end".len()..];
            let run = rest
                .bytes()
                .take(100 + "private key".len())
                .take_while(|byte| matches!(byte, b' ' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
                .count();
            for (middle, _) in rest[..run].match_indices("private key") {
                let tail = &rest[middle + "private key".len()..];
                for ending in ["-----", " block-----"] {
                    if tail.starts_with(ending) {
                        let length = "-----end".len() + middle + "private key".len() + ending.len();
                        footers.entry(&lower[at..at + length]).or_default().push(at);
                    }
                }
            }
        }
        let keys = KEY_HEADER
            .find_iter(&lower)
            .map(|header| {
                let footer = header.as_str().replacen("-----begin", "-----end", 1);
                let close = footers.get(footer.as_str()).and_then(|starts| {
                    starts
                        .get(starts.partition_point(|start| *start < header.end()))
                        .map(|start| start + footer.len())
                });
                (header.end(), close)
            })
            .collect();
        Self {
            keys,
            len: text.len(),
        }
    }

    /// Where text released up to `at` may end so that no private key is left open: `None` when no
    /// key is open at `at`, otherwise the first point past it where no key is open, or the end of
    /// the text when one of them is never closed. A part released or omitted up to an open key's
    /// middle leaves the rest of its body to a later request whose context no longer holds the
    /// header, so nothing recognizes it as a key. Past one key's footer another key may be open
    /// that began inside it, so the end moves on, over each key at most once.
    pub(crate) fn end_after(&self, at: usize) -> Option<usize> {
        let (mut end, mut until) = (at, None);
        let (mut next, mut reach) = (0, 0);
        loop {
            while let Some(&(header_end, close)) = self.keys.get(next) {
                if header_end > end {
                    break;
                }
                // A key that never closes is open at every point after its header, the end of
                // the text included.
                let Some(close) = close else {
                    return Some(self.len);
                };
                reach = reach.max(close);
                next += 1;
            }
            if reach <= end {
                return until;
            }
            end = reach;
            until = Some(end);
            if end == self.len {
                return until;
            }
        }
    }
}

/// Whether `text` opens a private key and does not close it.
#[cfg(feature = "cli")]
fn opens_key(text: &str) -> bool {
    PrivateKeys::new(text).end_after(text.len()).is_some()
}

fn withheld_report() -> Report {
    Report {
        text: WITHHELD.to_owned(),
        ..empty_report()
    }
}

fn empty_report() -> Report {
    Report {
        text: String::new(),
        secrets: 0,
        paths: 0,
        ips: 0,
        registered_ids: vec![],
    }
}

/// A prompt with every high-confidence finding removed here, then protected by `protect` (the
/// privacy worker, which also replaces the values the person registered), with its placeholders
/// dropped. `None` when `protect` cannot run.
#[cfg(feature = "cli")]
fn branch_text(prompt: &str, protect: impl FnOnce(&str) -> Option<String>) -> Option<String> {
    let scan = crate::domain::privacy::detector::scan_high_confidence(prompt, 64);
    if !scan.complete {
        return None;
    }
    let mut kept = String::new();
    let mut at = 0;
    for span in &scan.findings {
        if span.start >= at {
            kept.push_str(&prompt[at..span.start]);
            kept.push(' ');
            at = span.end;
        }
    }
    kept.push_str(&prompt[at..]);
    let protected = protect(&kept)?;
    let mut words = String::new();
    let mut at = 0;
    for (start, end, _) in crate::domain::privacy::projector::tokens(&protected) {
        words.push_str(&protected[at..start]);
        words.push(' ');
        at = end;
    }
    words.push_str(&protected[at..]);
    Some(words)
}

/// Whether `name` carries any of `values`, compared case-insensitively with every run of other
/// characters read as one `-`, since that is how a prompt becomes a branch name. A long word of
/// the name inside one long word of a value counts too: a name cut to length may hold only part
/// of a token.
#[cfg(feature = "cli")]
fn carries(name: &str, values: &[impl AsRef<str>]) -> bool {
    let name = name_words(name);
    values.iter().any(|value| {
        let value = Zeroizing::new(name_words(value.as_ref()));
        !value.is_empty()
            && (name.contains(value.as_str())
                || value
                    .split('-')
                    .filter(|part| part.len() >= 16)
                    .any(|part| {
                        name.split('-')
                            .any(|word| word.len() >= 8 && part.contains(word))
                    }))
    })
}

/// Lowercase ASCII words joined by single `-`s: how a branch name is spelled from text.
#[cfg(feature = "cli")]
fn name_words(text: &str) -> String {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>()
        .join("-")
}

/// Every value `protected` replaced in `original`: each placeholder's own value, and the text
/// each run of adjacent placeholders stands for. `restore` is asked once, for the distinct
/// placeholders in order, and its answers are reused for repeats: each ask reloads the whole
/// dictionary. `None` when a value can't be restored or the two texts can't be aligned, so
/// nothing is guessed.
#[cfg(feature = "cli")]
fn replaced_values(
    original: &str,
    protected: &str,
    restore: impl FnOnce(&[&str]) -> Option<Vec<Zeroizing<String>>>,
) -> Option<Vec<Zeroizing<String>>> {
    let mut values: Vec<Zeroizing<String>> = replaced_pieces(original, protected)?
        .into_iter()
        .map(Zeroizing::new)
        .collect();
    // A placeholder the original already holds is text, not a value protection replaced.
    let literal: Vec<&str> = crate::domain::privacy::projector::tokens(original)
        .map(|(_, _, token)| token)
        .collect();
    let mut distinct: Vec<&str> = Vec::new();
    for (_, _, token) in crate::domain::privacy::projector::tokens(protected) {
        if !distinct.contains(&token) && !literal.contains(&token) {
            distinct.push(token);
        }
    }
    if distinct.is_empty() {
        return Some(values);
    }
    let restored = restore(&distinct)?;
    if restored.len() != distinct.len() {
        return None;
    }
    for value in restored {
        if value.is_empty()
            || crate::domain::privacy::projector::tokens(&value)
                .next()
                .is_some()
            || !original.contains(value.as_str())
        {
            return None;
        }
        values.push(value);
    }
    Some(values)
}

/// `tokens` restored by one call of `hydrate` on them all, each kept apart by a separator no
/// value holds. `None` when the answer doesn't split back into one value per token.
#[cfg(feature = "cli")]
fn restore_all(
    tokens: &[&str],
    hydrate: impl FnOnce(&str) -> Option<Zeroizing<String>>,
) -> Option<Vec<Zeroizing<String>>> {
    let separator = format!("\u{1f}{}\u{1f}", uuid::Uuid::new_v4().simple());
    let restored = hydrate(&tokens.join(&separator))?;
    let values: Vec<Zeroizing<String>> = restored
        .split(separator.as_str())
        .map(|value| Zeroizing::new(value.to_owned()))
        .collect();
    (values.len() == tokens.len()).then_some(values)
}

/// The pieces of `original` that `protected` (its protected form) shows as placeholders. A run of
/// adjacent placeholders is one piece: the text it stands for in `original`. `None` when the two
/// can't be aligned without guessing.
#[cfg(feature = "cli")]
fn replaced_pieces(original: &str, protected: &str) -> Option<Vec<String>> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (start, end, _) in crate::domain::privacy::projector::tokens(protected) {
        match runs.last_mut() {
            Some(run) if run.1 == start => run.1 = end,
            _ => runs.push((start, end)),
        }
    }
    let mut pieces = Vec::new();
    let mut at = 0;
    let mut kept_until = 0;
    for (index, &(start, end)) in runs.iter().enumerate() {
        let before = &protected[kept_until..start];
        at += original.get(at..)?.find(before)? + before.len();
        let next_start = runs.get(index + 1).map_or(protected.len(), |next| next.0);
        let after = &protected[end..next_start];
        let hidden_end = if after.is_empty() {
            // Only the last run can end the text; any other is followed by kept text.
            if index + 1 != runs.len() {
                return None;
            }
            original.len()
        } else {
            at + original.get(at..)?.find(after)?
        };
        if hidden_end == at {
            return None;
        }
        pieces.push(original[at..hidden_end].to_owned());
        at = hidden_end;
        kept_until = end;
    }
    Some(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona() -> Persona {
        Persona {
            username: Some("nana".into()),
            home: Some("/cluster/home/nana".into()),
            hostname: Some("iZj6cprod07".into()),
        }
    }

    fn scrub(p: Persona, text: &str) -> Report {
        Redactor::new(p).scrub(text)
    }

    #[test]
    fn home_prefix_collapses_to_tilde() {
        let r = scrub(persona(), r#"cwd is /cluster/home/nana/projects/AgentGit"#);
        assert!(r.text.contains("cwd is ~/projects/AgentGit"), "{}", r.text);
        assert!(!r.text.contains("nana"));
        assert!(r.paths > 0);
    }

    #[test]
    fn bare_username_and_hostname_are_masked() {
        let r = scrub(persona(), "chown nana:nana /srv\nssh nana@iZj6cprod07");
        assert!(r.text.contains("chown user:user"), "{}", r.text);
        assert!(r.text.contains("ssh user@host"), "{}", r.text);
        // Word boundary: a substring inside a longer string must not be caught.
        let keep = scrub(Persona::default(), "nana is a name only via persona");
        assert_eq!(keep.text, "nana is a name only via persona");
    }

    #[test]
    fn other_home_users_get_stable_aliases() {
        let text = "alice: /home/alice/a, bob: /home/bob/b, again /home/alice/c";
        let r = scrub(persona(), text);
        assert!(
            r.text.contains("~user1/a") && r.text.contains("~user1/c"),
            "{}",
            r.text
        );
        assert!(r.text.contains("~user2/b"), "{}", r.text);
    }

    #[test]
    fn personas_own_home_under_users_dir_is_tilde() {
        let r = scrub(persona(), "cd /Users/nana/work");
        assert_eq!(r.text, "cd ~/work");
    }

    #[test]
    fn windows_home_masked() {
        let p = Persona {
            username: Some("alice".into()),
            home: None,
            hostname: None,
        };
        let r = scrub(p, r"C:\Users\alice\proj and C:\Users\bob\proj");
        assert_eq!(r.text, r"~\proj and C:\Users\user1\proj");
    }

    #[test]
    fn public_ips_masked_but_private_and_doc_kept() {
        let r = scrub(
            persona(),
            "ssh 47.91.17.103; LAN 10.0.0.8 192.168.1.1; docs 8.8.8.8 192.0.2.1; again 47.91.17.103",
        );
        assert!(r.text.contains("[ip1]"), "{}", r.text);
        assert!(!r.text.contains("47.91.17.103"));
        assert_eq!(
            r.text.matches("[ip1]").count(),
            2,
            "one IP maps to one stand-in"
        );
        for keep in ["10.0.0.8", "192.168.1.1", "8.8.8.8", "192.0.2.1"] {
            assert!(
                r.text.contains(keep),
                "a private or documentation address must not be erased: {keep}\n{}",
                r.text
            );
        }
    }

    #[test]
    fn deterministic_on_same_input() {
        let text = "/home/nana/x and /home/alice/y via 47.91.17.103 and ghp_".to_string()
            + &"z".repeat(36);
        let a = scrub(persona(), &text);
        let b = scrub(persona(), &text);
        assert_eq!(a.text, b.text);
    }

    const KEY_ID: &str = "AKIA2E7YQXK4NMZ5VJ3T";
    const TOKEN: &str = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
    const REGISTERED: &str = "Larkspur-Overture";

    fn placeholder() -> String {
        format!(
            "{{{{AGIT_SECRET_V2:{}:{}}}}}",
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4()
        )
    }

    fn values() -> Vec<String> {
        [KEY_ID, TOKEN, REGISTERED].map(str::to_owned).to_vec()
    }

    /// A new session's branch is named from its opening request as published: the request's
    /// words stay, and nothing publication replaces reaches the name, in any spelling.
    #[test]
    fn a_new_branch_is_named_from_the_published_request() {
        let request = format!("deploy with {KEY_ID} and {TOKEN} using {REGISTERED} please");
        let text = branch_text(&request, |text| {
            Some(text.replace(REGISTERED, &placeholder()))
        })
        .unwrap();
        let name = crate::commands::front::slug(&text, 200);
        assert!(name.starts_with("deploy-with"), "{name}");
        assert!(name.ends_with("please"), "{name}");
        assert!(!carries(&name, &values()), "{name}");
        assert!(
            branch_text(&request, |_| None).is_none(),
            "a check that can't run"
        );
    }

    /// Before a branch new to the Hub is published, a name carrying anything publication replaces
    /// in its session's request is found, whatever its case and separators.
    #[test]
    fn a_branch_name_carrying_a_replaced_value_is_found_in_any_spelling() {
        for name in [
            "deploy-akia2e7yqxk4nmz5vj3t",
            "deploy_AKIA2E7YQXK4NMZ5VJ3T",
            "use-ghp-r7kq2mxv9lpz4tnc8wjf3bhy6svd1age5ukr",
            "with-larkspur-overture",
            "with larkspur overture",
            "ghp-r7kq2mxv9lpz4tnc8wjf3bhy6",
        ] {
            assert!(carries(name, &values()), "{name}");
        }
        for name in ["deploy-with-please", "overture-notes", "akia-rotation"] {
            assert!(!carries(name, &values()), "{name}");
        }
        // Two values side by side are one replaced span, and a name holding both is found.
        let joined = replaced_pieces(
            "use LarkspurOverture now",
            &format!("use {}{} now", placeholder(), placeholder()),
        )
        .expect("adjacent placeholders align");
        assert_eq!(joined, vec!["LarkspurOverture".to_owned()]);
        assert!(carries("use-larkspuroverture-abcd", &joined));
        // Each of two adjacent values is found on its own too.
        let (first, second) = (placeholder(), placeholder());
        let restore = |tokens: &[&str]| {
            tokens
                .iter()
                .map(|token| {
                    (*token == first)
                        .then(|| Zeroizing::new("Larkspur".to_owned()))
                        .or_else(|| (*token == second).then(|| Zeroizing::new("Nova".to_owned())))
                })
                .collect()
        };
        let values = replaced_values(
            "use LarkspurNova now",
            &format!("use {first}{second} now"),
            restore,
        )
        .expect("each value restored");
        for name in [
            "use-larkspur-abcd",
            "use-nova-abcd",
            "use-larkspurnova-abcd",
        ] {
            assert!(carries(name, &values), "{name}");
        }
        assert!(!carries("use-abcd", &values));
        // A value that can't be restored is no answer.
        assert!(
            replaced_values(
                "use LarkspurNova now",
                &format!("use {first}{second} now"),
                |_| None
            )
            .is_none()
        );
        // Text that can't be aligned without guessing is no answer at all.
        assert_eq!(
            replaced_pieces("use something else", &format!("use {} now", placeholder())),
            None
        );
        let token = placeholder();
        let original = format!("use {REGISTERED} now");
        assert_eq!(
            replaced_pieces(&original, &format!("use {token} now")),
            Some(vec![REGISTERED.to_owned()])
        );
    }

    /// A request repeating its placeholders many times asks the privacy log once, for each
    /// distinct placeholder, and the repeats reuse those answers.
    #[test]
    fn repeated_placeholders_are_restored_in_one_ask() {
        let (first, second) = (placeholder(), placeholder());
        let original = format!("use {REGISTERED} and Nova ").repeat(1024);
        let protected = format!("use {first} and {second} ").repeat(1024);
        let dictionary = [(first.as_str(), REGISTERED), (second.as_str(), "Nova")];
        let hydrate = |text: &str| {
            let mut text = text.to_owned();
            for (token, value) in dictionary {
                text = text.replace(token, value);
            }
            Some(Zeroizing::new(text))
        };
        let asks = std::cell::Cell::new(0);
        let asked = std::cell::RefCell::new(Vec::new());
        let values = replaced_values(&original, &protected, |tokens| {
            asks.set(asks.get() + 1);
            asked
                .borrow_mut()
                .extend(tokens.iter().map(|token| token.to_string()));
            restore_all(tokens, hydrate)
        })
        .expect("each value restored");
        assert_eq!(asks.get(), 1, "one ask for 2,048 placeholders");
        assert_eq!(*asked.borrow(), [first.clone(), second.clone()]);
        assert!(carries("with-larkspur-overture", &values));
        assert!(carries("use-nova", &values));
        // An answer that doesn't split back into one value per placeholder is no answer.
        assert!(
            restore_all(&[first.as_str(), second.as_str()], |_| Some(
                Zeroizing::new("Larkspur".to_owned())
            ))
            .is_none()
        );
        assert!(restore_all(&[first.as_str()], |_| None).is_none());
    }

    /// A credential-shaped branch name is refused by the detector in this process, before the
    /// privacy worker is asked: the answer cannot wait on a worker that is slow or absent.
    #[test]
    fn a_credential_shaped_branch_name_is_refused_without_the_worker() {
        let redactor = Redactor::new(persona());
        assert!(
            !redactor
                .publishes_unchanged("fix-ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr")
                .unwrap()
        );
    }

    /// A native record whose request cannot fit one protection unit is withheld for good, alone:
    /// the records beside it are still protected, only the oversized one is marked permanent so
    /// its page can move on, and a record whose request fits exactly passes. A batch that
    /// withheld every record beside a large one, marked it retryable, or judged size with a
    /// fixed reserve instead of the real request would hide ordinary history or retry a record
    /// that can never pass.
    #[test]
    fn an_oversized_native_record_is_withheld_alone_and_for_good() {
        let redactor = Redactor::new(persona());
        let large = serde_json::json!({
            "type": "user",
            "message": {"content": "x".repeat(crate::domain::privacy::projector::MAX_UNIT_BYTES)}
        });
        let small = serde_json::json!({"type": "user", "message": {"content": "hello"}});
        // A record whose whole single-record request fits exactly is not too large.
        let room = crate::domain::privacy::projector::MAX_UNIT_BYTES
            - serde_json::to_string(&crate::domain::privacy::projector::NativeBatch {
                runtime: String::new(),
                session: String::new(),
                records: vec![serde_json::json!({"type": "user", "message": {"content": ""}})],
                pointers: vec![Vec::new()],
            })
            .unwrap()
            .len();
        let fits = serde_json::json!({"type": "user", "message": {"content": "x".repeat(room)}});
        let records = [(&large, &[][..]), (&small, &[][..]), (&fits, &[][..])];
        let protected = redactor.scrub_native_batch_profiled(&records, &mut |_, _| {});
        assert_eq!(protected.len(), 3);
        assert!(protected[0].value.get("protection_error").is_some());
        assert!(protected[0].permanent);
        assert!(!protected[1].permanent);
        assert_eq!(protected[1].value, small);
        assert!(!protected[2].permanent);
        assert_eq!(protected[2].value, fits);
    }

    /// The device-control redactor, the one remote control uses, passes text, JSON values, native
    /// records and streamed text as they are, a credential and a public IP included, while the
    /// privacy check would hold them back: remote control shows the owner their own session. A
    /// redactor of any other kind still holds back what the check holds back. A device-control
    /// redactor that asked the privacy worker would put the withheld marker or a placeholder into
    /// the owner's view; one that still numbered IPs would show `[ip1]` in live text and the
    /// stream but the address in history. The stream splits after the address, not inside it,
    /// because a chunk holding half an address is not numbered either way.
    #[cfg(feature = "rc")]
    #[test]
    fn the_device_control_redactor_passes_everything_as_it_is() {
        use crate::domain::privacy::projector::Reason;
        let credential = "AKIA4X7QZ2M5RT6VW3JH";
        let text = format!("ssh 47.91.17.103 && export AWS_ACCESS_KEY_ID={credential}\n");
        WITHHOLD_LIVE.with(|w| w.set(Some((credential, Reason::LockBusy))));
        WITHHOLD_NATIVE.with(|w| w.set(Some(credential)));
        let value = serde_json::json!({"command": text});
        let record = serde_json::json!({"type": "user", "message": {"content": text}});
        let records = [(&record, &[][..])];

        let live = Redactor::new(persona()).for_device_control();
        assert_eq!(live.protect_text(&text).unwrap().text, text);
        assert_eq!(live.scrub(&text).text, text);
        assert_eq!(live.protect_json(&value).unwrap().value, value);
        let native = live.scrub_native_batch_profiled(&records, &mut |_, _| {});
        assert_eq!(native[0].value, record);
        let mut stream = live.stream();
        let (first, second) = text.split_at(text.find("&&").unwrap());
        let streamed = [stream.push(first), stream.push(second), stream.flush()]
            .map(|report| report.text)
            .concat();
        assert_eq!(streamed, text);

        let checked = Redactor::new(persona());
        assert!(checked.protect_text(&text).is_err());
        assert!(checked.protect_json(&value).is_err());
        let native = checked.scrub_native_batch_profiled(&records, &mut |_, _| {});
        assert!(native[0].value.get("protection_error").is_some());
        WITHHOLD_LIVE.with(|w| w.set(None));
        WITHHOLD_NATIVE.with(|w| w.set(None));
    }

    #[test]
    fn empty_persona_keeps_ordinary_text() {
        let r = Redactor::new(Persona::default()).scrub("no persona here");
        assert_eq!(r.text, "no persona here");
        assert_eq!(r.paths, 0);
    }

    /// The end `PrivateKeys` gives lies outside every private key, also when a second key
    /// begins inside the first one's body and closes after it. An end just past the first
    /// footer would sit in the second body, which the next request would hold without its header.
    #[cfg(feature = "cli")]
    #[test]
    fn a_released_part_never_ends_inside_interleaved_private_keys() {
        let text = "intro\n-----BEGIN RSA PRIVATE KEY-----\nAAAA\n\
                    -----BEGIN OPENSSH PRIVATE KEY-----\nBBBB\n-----END RSA PRIVATE KEY-----\n\
                    CCCC\n-----END OPENSSH PRIVATE KEY-----\ntail\n";
        let keys = PrivateKeys::new(text);
        let end = keys.end_after(text.find("AAAA").unwrap()).unwrap();
        assert_eq!(&text[end..], "\ntail\n");
        assert_eq!(keys.end_after(end), None);
        assert_eq!(keys.end_after(text.find("intro").unwrap()), None);
    }

    /// A long chain of keys, each opening inside the one before it and closing after it, ends
    /// only past its last footer. A search that rescanned the text after each footer it moved
    /// past would give the same end, but with work growing with the square of the chain; the
    /// chain is long enough for that work to dominate the test run.
    #[cfg(feature = "cli")]
    #[test]
    fn a_long_chain_of_interleaved_keys_ends_past_its_last_footer() {
        let (first, second) = ("RSA", "OPENSSH");
        let mut text = format!("-----BEGIN {first} PRIVATE KEY-----\nbody\n");
        let mut open = first;
        for _ in 0..20_000 {
            let other = if open == first { second } else { first };
            text.push_str(&format!("-----BEGIN {other} PRIVATE KEY-----\nbody\n"));
            text.push_str(&format!("-----END {open} PRIVATE KEY-----\nbody\n"));
            open = other;
        }
        text.push_str(&format!("-----END {open} PRIVATE KEY-----"));
        let last = text.len();
        text.push_str("\nafter\n");
        let keys = PrivateKeys::new(&text);
        assert_eq!(keys.end_after(text.find("body").unwrap()), Some(last));
        assert_eq!(keys.end_after(last), None);
    }

    /// A key with no footer is open at the end of its text, so a stream that ends inside one is
    /// withheld; a key whose footer is the text's last line is closed there. An index that
    /// recorded "no footer" as a close at the end of the text answers both the same way, and the
    /// unclosed key at the end of a stream goes out as ordinary text.
    #[cfg(feature = "cli")]
    #[test]
    fn a_key_without_a_footer_stays_open_at_the_end_of_its_text() {
        for header in [
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----begin rsa private key-----",
            "-----BEGIN PGP PRIVATE KEY BLOCK-----",
        ] {
            let open = format!("intro\n{header}\nSYNTHETIC\n");
            assert!(opens_key(&open), "{open:?}");
            assert_eq!(PrivateKeys::new(&open).end_after(2), None);
            let footer = header
                .replacen("BEGIN", "END", 1)
                .replacen("begin", "end", 1);
            let closed = format!("{open}{footer}");
            assert!(!opens_key(&closed), "{closed:?}");
        }
    }
}
