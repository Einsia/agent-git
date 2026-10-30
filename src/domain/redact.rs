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

    #[cfg(feature = "rc")]
    pub(crate) fn for_device_control(mut self) -> Self {
        self.preserve_device_identity = true;
        self
    }

    #[cfg(feature = "rc")]
    pub(crate) fn with_native_context(
        self,
        runtime: &str,
        session: &str,
        _cwd: &std::path::Path,
        root: &std::path::Path,
    ) -> Self {
        self.for_repository(Some(root))
            .with_unbound_native_context(runtime, session)
    }
    #[cfg(feature = "rc")]
    pub(crate) fn with_unbound_native_context(self, runtime: &str, session: &str) -> Self {
        if let Ok(mut native) = self.native.try_lock() {
            *native = (runtime.into(), session.into());
        }
        self
    }
    #[cfg(feature = "rc")]
    pub(crate) fn bind_native_session(&self, session: &str) {
        if let Ok(mut native) = self.native.try_lock() {
            native.1 = session.into();
        }
    }
    #[cfg(feature = "secret-vault")]
    fn dictionary_query(&self, action: &str) -> Option<serde_json::Value> {
        #[cfg(feature = "cli")]
        return crate::domain::privacy::service::manage(
            self.repo.as_deref(),
            crate::domain::privacy::management::Command {
                action: action.into(),
                global: self.repo.is_none(),
                id: None,
                name: None,
                secret: None,
            },
        )
        .ok();
        #[cfg(not(feature = "cli"))]
        {
            let _ = action;
            None
        }
    }
    #[cfg(feature = "secret-vault")]
    pub(crate) fn publication_values_matching(
        &self,
        used: impl Fn(&str) -> bool,
    ) -> crate::Result<std::collections::BTreeSet<String>> {
        Ok(self
            .dictionary_query("values")
            .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|value| used(value))
            .collect())
    }

    #[cfg(feature = "rc")]
    pub(crate) fn scrub_native_batch_profiled(
        &self,
        records: &[(&serde_json::Value, &[&str])],
        timing: &mut dyn FnMut(&'static str, f64),
    ) -> Vec<NativeJson> {
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

    /// What this redactor replaces on its own, after the privacy worker: text the worker checked
    /// under one policy comes out of two redactors the same when this is the same for both.
    #[cfg(feature = "rc")]
    pub(crate) fn persona_key(&self) -> String {
        format!("{:?}/{}", self.persona, self.preserve_device_identity)
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
        for attempt in 0..2 {
            let protected = crate::domain::privacy::service::transform(
                self.repo.as_deref(),
                name,
                crate::domain::privacy::projector::Mode::ProtectText,
            );
            if crate::domain::privacy::service::passed(&protected) {
                return Ok(protected.content == name);
            }
            if attempt == 0 {
                crate::domain::privacy::service::wait_for_retry();
            }
        }
        anyhow::bail!(
            "the privacy check for `{name}` could not run; retrying the push usually works (`agit doctor` if it keeps failing)"
        )
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

        if !self.preserve_device_identity {
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
        #[cfg(all(test, feature = "rc"))]
        if let Some(withheld) = withheld_live(&value.to_string()) {
            return Err(withheld);
        }
        #[cfg(feature = "cli")]
        let protected = crate::domain::privacy::service::transform_live(
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
            serde_json::from_str(&protected.content).map_err(|_| Withheld { reason: None })?;
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
