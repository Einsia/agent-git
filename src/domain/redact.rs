//! Content redaction: erasing the coordinates before publishing.
//!
//! # The split with the `secrets` module
//!
//! `secrets` is the **gate**: a secret found stops the push. This is the **rewrite**: it makes a
//! byte stream that can be published, with secrets replaced by `[redacted:<rule>]` and
//! environment markers (username, home, hostname, public IP) replaced by stable stand-ins. The
//! gate decides "may this be pushed"; this module answers "what does the published copy look
//! like".
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
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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

#[cfg(feature = "rc")]
pub(crate) struct NativeJson {
    pub value: serde_json::Value,
    pub secret_projection: bool,
    pub registered_ids: Vec<String>,
}

/// User-facing text for a native record that could not pass the local privacy boundary.
/// Internal protection errors belong in the daemon log and must not be sent to viewers.
pub(crate) const PROTECTION_ERROR_TEXT: &str =
    "[content unavailable: local privacy protection could not complete]";

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
    username_pattern: Option<Regex>,
    hostname_pattern: Option<Regex>,
    #[cfg(feature = "secret-vault")]
    require_repository: bool,
    buffered_stream_bytes: Arc<AtomicUsize>,
    #[cfg(feature = "secret-vault")]
    registered: crate::domain::secret_filter::MatcherHandle,
    #[cfg(feature = "secret-vault")]
    dictionary: Option<Arc<crate::domain::secret_filter::RepositoryDictionary>>,
    #[cfg(feature = "rc")]
    native: Option<Arc<std::sync::Mutex<NativeProtection>>>,
}

#[cfg(feature = "rc")]
struct NativeProtection {
    runtime: String,
    session: String,
    evidence: crate::domain::secrets::identity::Evidence,
    seeded: bool,
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
    for (s, e, r) in spans.iter().rev() {
        out.replace_range(*s..*e, r);
        *count += 1;
    }
}

/// Replacement wrapped in word boundaries (the regex crate has no lookaround, so the boundary is
/// "capture what precedes + check what follows by hand").
fn replace_token(text: &str, token: &str, re: &Regex, with: &str, count: &mut usize) -> String {
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

fn token_pattern(token: Option<&str>) -> Option<Regex> {
    // Dots, underscores, hyphens, and ASCII alphanumerics keep a token inside a longer name.
    token
        .filter(|token| token.len() >= 3)
        .map(|token| Regex::new(&format!(r"(^|[^A-Za-z0-9._-]){}", regex::escape(token))).unwrap())
}

impl Redactor {
    pub fn new(persona: Persona) -> Self {
        Redactor {
            username_pattern: token_pattern(persona.username.as_deref()),
            hostname_pattern: token_pattern(persona.hostname.as_deref()),
            persona,
            #[cfg(feature = "secret-vault")]
            require_repository: false,
            buffered_stream_bytes: Arc::new(AtomicUsize::new(0)),
            #[cfg(feature = "secret-vault")]
            registered: Default::default(),
            #[cfg(feature = "secret-vault")]
            dictionary: None,
            #[cfg(feature = "rc")]
            native: None,
        }
    }

    #[cfg(feature = "secret-vault")]
    pub fn with_registered(
        persona: Persona,
        registered: crate::domain::secret_filter::MatcherHandle,
    ) -> Self {
        Redactor {
            registered,
            ..Self::new(persona)
        }
    }

    /// The caller supplies the selected Agent repository; a source-code working directory does
    /// not select the repository that owns this session's reversible mappings.
    #[cfg(feature = "secret-vault")]
    pub fn with_repository(mut self, repo_root: &std::path::Path) -> crate::Result<Self> {
        self.dictionary = Some(Arc::new(
            crate::domain::secret_filter::RepositoryDictionary::open(repo_root)?,
        ));
        Ok(self)
    }

    /// Supervised sessions need a selected Agent repository to publish reversible mappings.
    #[cfg(feature = "rc")]
    pub(crate) fn require_repository(mut self) -> Self {
        self.require_repository = true;
        self
    }

    #[cfg(feature = "rc")]
    pub(crate) fn with_native_context(
        mut self,
        runtime: &str,
        session: &str,
        cwd: &std::path::Path,
        root: &std::path::Path,
    ) -> Self {
        self.native = Some(Arc::new(std::sync::Mutex::new(NativeProtection {
            runtime: runtime.into(),
            session: session.into(),
            evidence: crate::domain::secrets::identity::Evidence::new(
                &crate::domain::repo::Repo::at(root),
                cwd,
            ),
            seeded: false,
        })));
        self
    }

    #[cfg(feature = "rc")]
    pub(crate) fn bind_native_session(&self, session: &str) {
        if let Some(native) = &self.native
            && let Ok(mut native) = native.lock()
            && native.session != session
        {
            native.evidence.reset();
            native.seeded = false;
            native.session = session.to_owned();
        }
    }

    #[cfg(all(feature = "rc", test))]
    pub(crate) fn scrub_native_json(
        &self,
        value: &serde_json::Value,
        pointers: &[&str],
    ) -> NativeJson {
        self.scrub_native_batch(&[(value, pointers)]).remove(0)
    }

    /// Healthy pages share a dictionary transaction; identity masks remain occurrence-scoped.
    #[cfg(feature = "rc")]
    pub(crate) fn scrub_native_batch(
        &self,
        records: &[(&serde_json::Value, &[&str])],
    ) -> Vec<NativeJson> {
        let withheld = || NativeJson {
            value: serde_json::json!({"protection_error": PROTECTION_ERROR_TEXT}),
            secret_projection: true,
            registered_ids: Vec::new(),
        };
        if records.is_empty() {
            return Vec::new();
        }
        let result = (|| -> crate::Result<Vec<NativeJson>> {
            let (Some(native), Some(dictionary)) = (&self.native, &self.dictionary) else {
                return Ok(records
                    .iter()
                    .map(|(value, pointers)| {
                        let report = self.scrub_json_with_verified_fields(value, pointers);
                        NativeJson {
                            secret_projection: report.secrets > 0
                                || report.value.get("protection_error").is_some(),
                            value: report.value,
                            registered_ids: report.registered_ids,
                        }
                    })
                    .collect());
            };
            let mut native = native
                .lock()
                .map_err(|_| anyhow::anyhow!("native protection context is unavailable"))?;
            let runtime = native.runtime.clone();
            let session = native.session.clone();
            if !native.seeded && !session.is_empty() {
                native.evidence.seed_native(&runtime, &session)?;
                native.seeded = true;
            }
            let masks: Vec<_> = records
                .iter()
                .map(|(value, pointers)| {
                    let mut mask = native.evidence.record(&runtime, &session, value);
                    for pointer in *pointers {
                        if let Some(text) =
                            value.pointer(pointer).and_then(serde_json::Value::as_str)
                        {
                            mask.0.push(((*pointer).into(), 0..text.len()));
                        }
                    }
                    mask.0
                        .sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start.cmp(&b.1.start)));
                    mask.0.dedup();
                    mask
                })
                .collect();
            let registered = self.registered.snapshot();
            let protect = |range: std::ops::Range<usize>| -> crate::Result<Vec<NativeJson>> {
                let mut input = String::new();
                for (value, _) in &records[range.clone()] {
                    input.push_str(&serde_json::to_string(value)?);
                    input.push('\n');
                }
                let mut index = range.start;
                let protected = dictionary.protect_with_masks(&input, &registered, |_| {
                    let mask = masks[index].clone();
                    index += 1;
                    mask
                })?;
                anyhow::ensure!(
                    protected.intact == 0,
                    "native page exceeds its reversible protection limit"
                );
                let values: Vec<serde_json::Value> = protected
                    .text
                    .lines()
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()?;
                anyhow::ensure!(
                    index == range.end && values.len() == range.len(),
                    "native protection changed record boundaries"
                );
                values
                    .into_iter()
                    .zip(&records[range])
                    .map(|(mut value, (original, _))| {
                        // Persona changes preserve source hashes; secret projection cannot expose them.
                        let secret_projection = value != **original;
                        self.scrub_persona_json(&mut value, &mut JsonTotals::default())?;
                        Ok(NativeJson {
                            value,
                            secret_projection,
                            registered_ids: Vec::new(),
                        })
                    })
                    .collect()
            };
            // A single oversized or malformed record must not hide healthy neighbors. Retry
            // each record with its original mask, while keeping every failed record fail-closed.
            match protect(0..records.len()) {
                Ok(values) => Ok(values),
                Err(batch_error) => {
                    let mut values = Vec::with_capacity(records.len());
                    let mut first_failure = None;
                    for index in 0..records.len() {
                        match protect(index..index + 1) {
                            Ok(mut item) => values.append(&mut item),
                            Err(error) => {
                                first_failure.get_or_insert(error);
                                values.push(withheld());
                            }
                        }
                    }
                    if let Some(error) = first_failure {
                        eprintln!(
                            "agitd: native secret projection batch failed; retried {} records: {batch_error:#}",
                            records.len()
                        );
                        eprintln!(
                            "agitd: native secret projection withheld one or more records: {error:#}"
                        );
                    }
                    Ok(values)
                }
            }
        })();
        result.unwrap_or_else(|error| {
            eprintln!(
                "agitd: native secret projection withheld {} records: {error:#}",
                records.len()
            );
            records.iter().map(|_| withheld()).collect()
        })
    }

    #[cfg(feature = "rc")]
    fn scrub_persona_json(
        &self,
        value: &mut serde_json::Value,
        totals: &mut JsonTotals,
    ) -> crate::Result<()> {
        match value {
            serde_json::Value::String(text) => {
                let report = self.scrub_persona(text);
                totals.add(&report);
                *text = report.text;
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    self.scrub_persona_json(value, totals)?;
                }
            }
            serde_json::Value::Object(map) => {
                for (key, mut value) in std::mem::take(map) {
                    let report = self.scrub_persona(&key);
                    totals.add(&report);
                    self.scrub_persona_json(&mut value, totals)?;
                    anyhow::ensure!(
                        map.insert(report.text, value).is_none(),
                        "privacy projection produced duplicate JSON keys"
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The infallible form: panicking on a vault failure is safer than silently publishing
    /// unscanned content. An outbound path calls [`Self::try_this_machine`] instead, so the user
    /// gets an actionable error.
    pub fn this_machine() -> Self {
        Self::try_this_machine()
            .expect("the device-local secret-filter vault could not be authenticated")
    }

    /// An outbound path handles an unlock failure explicitly; it must not degrade to an empty
    /// rule set and publish anyway.
    pub fn try_this_machine() -> crate::Result<Self> {
        #[cfg(feature = "secret-vault")]
        {
            Ok(Self::with_registered(
                Persona::this_machine(),
                crate::domain::secret_filter::MatcherHandle::load_default()?,
            ))
        }
        #[cfg(not(feature = "secret-vault"))]
        {
            Ok(Self::new(Persona::this_machine()))
        }
    }

    pub fn stream(&self) -> StreamRedactor {
        StreamRedactor {
            redactor: self.clone(),
            pending: String::new(),
            failed: false,
        }
    }

    /// Redact one text. Deterministic: same input, same persona ⇒ same output.
    pub fn scrub(&self, text: &str) -> Report {
        self.try_scrub(text).unwrap_or_else(|_| Report {
            text: PROTECTION_ERROR_TEXT.into(),
            ..empty_report()
        })
    }

    /// Mapping persistence must succeed before any protected bytes leave this call.
    pub fn try_scrub(&self, text: &str) -> crate::Result<Report> {
        // ── 1. Secrets first: later rewrites must not change rule hits, or the reverse ──
        // Inspect both policies on original bytes. Rewriting either first can
        // destroy the evidence needed to recognize an overlapping sensitive region.
        #[cfg(feature = "secret-vault")]
        let (out, secrets, registered_ids) = if let Some(dictionary) = &self.dictionary {
            let report = dictionary.protect_text(text, &self.registered.snapshot())?;
            anyhow::ensure!(
                report.intact == 0,
                "text exceeds the reversible protection limit"
            );
            (report.text, report.replacements, Vec::new())
        } else {
            let scrubbed =
                crate::domain::secrets::scrub_registered(text, &self.registered.snapshot());
            anyhow::ensure!(
                !self.require_repository || scrubbed.1 == 0,
                "content withheld: reversible protection requires the session's Agent repository"
            );
            scrubbed
        };

        #[cfg(not(feature = "secret-vault"))]
        let (out, secrets) = crate::domain::secrets::scrub(text);
        #[cfg(not(feature = "secret-vault"))]
        let registered_ids = Vec::new();

        let mut report = self.scrub_persona(&out);
        report.secrets = secrets;
        report.registered_ids = registered_ids;
        Ok(report)
    }

    /// Call only after secret projection: environment substitutions are intentionally irreversible.
    pub(crate) fn scrub_persona(&self, protected: &str) -> Report {
        let mut out = protected.to_owned();
        let mut paths = 0;

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
        if let (Some(user), Some(pattern)) = (persona_user, &self.username_pattern) {
            out = replace_token(&out, user, pattern, "user", &mut paths);
        }
        if let (Some(host), Some(pattern)) = (&self.persona.hostname, &self.hostname_pattern) {
            out = replace_token(&out, host, pattern, "host", &mut paths);
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
            secrets: 0,
            paths,
            ips,
            registered_ids: Vec::new(),
        }
    }

    /// Scrub every semantic JSON string, including object keys. This is the
    /// only safe boundary for serialized runtime events: matching the wire text
    /// would miss `\"`, `\\` and `\n` inside a registered literal.
    pub fn scrub_json(&self, value: &serde_json::Value) -> JsonReport {
        self.try_scrub_json(value).unwrap_or_else(|_| JsonReport {
            value: serde_json::json!({"protection_error": PROTECTION_ERROR_TEXT}),
            secrets: 0,
            paths: 0,
            ips: 0,
            registered_ids: vec![],
        })
    }

    /// Pointers are supplied only after the caller validates schema-owned identities.
    /// Explicit registrations still override identity evidence at those exact occurrences.
    #[cfg(feature = "secret-vault")]
    pub(crate) fn scrub_json_with_verified_fields(
        &self,
        value: &serde_json::Value,
        pointers: &[&str],
    ) -> JsonReport {
        let registered = match &self.dictionary {
            Some(dictionary) => match dictionary
                .registered_matcher()
                .and_then(|local| self.registered.snapshot().merged(&local))
            {
                Ok(matcher) => matcher,
                Err(_) => return self.scrub_json(value),
            },
            None => self.registered.snapshot(),
        };
        let mut input = value.clone();
        let mut retained = Vec::new();
        for pointer in pointers {
            if let Some(field) = input.pointer_mut(pointer)
                && let Some(identity) = field.as_str()
                && registered.find(identity).is_empty()
            {
                retained.push((*pointer, field.take()));
            }
        }
        let mut report = self.scrub_json(&input);
        for (pointer, identity) in retained {
            if let Some(field) = report.value.pointer_mut(pointer) {
                *field = identity;
            }
        }
        report
    }

    pub fn try_scrub_json(&self, value: &serde_json::Value) -> crate::Result<JsonReport> {
        let mut value = value.clone();
        let mut totals = JsonTotals::default();
        self.scrub_json_inner(&mut value, &mut totals)?;
        // Some built-in gitleaks rules need assignment context spanning a JSON
        // key and value. Preserve the previous whole-wire pass after semantic
        // registered matching; otherwise `{"token":"..."}` could regress
        // even though quoted/newline registered values are now handled safely.
        let wire = serde_json::to_string(&value)?;
        #[cfg(feature = "secret-vault")]
        let (wire, built_in) = if let Some(dictionary) = &self.dictionary {
            let report = dictionary.protect_jsonl(&wire, &self.registered.snapshot())?;
            anyhow::ensure!(
                report.intact == 0,
                "JSON exceeds the reversible protection limit"
            );
            (report.text, report.replacements)
        } else {
            crate::domain::secrets::scrub(&wire)
        };
        #[cfg(not(feature = "secret-vault"))]
        let (wire, built_in) = crate::domain::secrets::scrub(&wire);
        if built_in > 0
            && let Ok(scrubbed) = serde_json::from_str(&wire)
        {
            value = scrubbed;
            totals.secrets = totals.secrets.saturating_add(built_in);
        }
        Ok(JsonReport {
            value,
            secrets: totals.secrets,
            paths: totals.paths,
            ips: totals.ips,
            registered_ids: totals.registered_ids.into_iter().collect(),
        })
    }

    fn scrub_json_inner(
        &self,
        value: &mut serde_json::Value,
        totals: &mut JsonTotals,
    ) -> crate::Result<()> {
        match value {
            serde_json::Value::String(text) => {
                let report = self.try_scrub(text)?;
                totals.add(&report);
                *text = report.text;
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    self.scrub_json_inner(value, totals)?;
                }
            }
            serde_json::Value::Object(map) => {
                let old = std::mem::take(map);
                for (key, mut value) in old {
                    let key_report = self.try_scrub(&key)?;
                    totals.add(&key_report);
                    self.scrub_json_inner(&mut value, totals)?;
                    // Redaction can theoretically collapse two keys. Keep the
                    // first instead of losing the whole object or restoring a
                    // secret-bearing key on the outbound path.
                    map.entry(key_report.text).or_insert(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
        Ok(())
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

/// Buffer an item until finalization: contextual and multiline findings may
/// depend on bytes beyond any fixed tail. Capacity failure poisons the item so
/// neither later chunks nor finalization can release an unchecked suffix.
pub struct StreamRedactor {
    redactor: Redactor,
    pending: String,
    failed: bool,
}

pub(crate) const MAX_STREAM_ITEM_BYTES: usize = 1024 * 1024;
const MAX_STREAM_BUFFER_BYTES: usize = 8 * MAX_STREAM_ITEM_BYTES;

#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("stream protection failed or exceeded its limit; no item bytes were emitted")]
pub struct StreamProtectionError;

impl StreamRedactor {
    pub fn push(&mut self, chunk: &str) -> Result<Report, StreamProtectionError> {
        if self.failed || chunk.len() > MAX_STREAM_ITEM_BYTES.saturating_sub(self.pending.len()) {
            self.fail();
            return Err(StreamProtectionError);
        }
        if self
            .redactor
            .buffered_stream_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(chunk.len())
                    .filter(|total| *total <= MAX_STREAM_BUFFER_BYTES)
            })
            .is_err()
        {
            self.fail();
            return Err(StreamProtectionError);
        }
        if self.pending.try_reserve_exact(chunk.len()).is_err() {
            self.redactor
                .buffered_stream_bytes
                .fetch_sub(chunk.len(), Ordering::Relaxed);
            self.fail();
            return Err(StreamProtectionError);
        }
        self.pending.push_str(chunk);
        Ok(empty_report())
    }

    pub fn flush(&mut self) -> Result<Report, StreamProtectionError> {
        if self.failed {
            return Err(StreamProtectionError);
        }
        if self.pending.is_empty() {
            return Ok(empty_report());
        }
        let ready = std::mem::take(&mut self.pending);
        self.redactor
            .buffered_stream_bytes
            .fetch_sub(ready.len(), Ordering::Relaxed);
        #[cfg(feature = "rc")]
        if let Some(native) = &self.redactor.native {
            let mut native = native.lock().map_err(|_| StreamProtectionError)?;
            if native.evidence.contains_object_identity(&ready) {
                // The authoritative completed record carries the operation/result relationship.
                return Ok(empty_report());
            }
        }
        self.redactor.try_scrub(&ready).map_err(|_| {
            self.failed = true;
            StreamProtectionError
        })
    }

    fn fail(&mut self) {
        self.failed = true;
        self.redactor
            .buffered_stream_bytes
            .fetch_sub(self.pending.len(), Ordering::Relaxed);
        #[cfg(feature = "secret-vault")]
        zeroize::Zeroize::zeroize(&mut self.pending);
        self.pending.clear();
    }
}

impl Drop for StreamRedactor {
    fn drop(&mut self) {
        self.fail();
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

    #[cfg(feature = "secret-vault")]
    #[test]
    fn repository_streams_persist_reversible_values_and_reuse_local_blocks() {
        use crate::domain::secret_filter::{MatcherHandle, RepositoryDictionary};
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        let dictionary = RepositoryDictionary::open(repo.root()).unwrap();
        dictionary
            .block_add("local", "blue horse battery".to_string().into(), false)
            .unwrap();
        let value = "Qz7mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr2dF";
        let redactor = Redactor::with_registered(Persona::default(), MatcherHandle::default())
            .with_repository(repo.root())
            .unwrap();
        let input = format!("blue horse battery {value}");
        let mut stream = redactor.stream();
        assert!(stream.push(&input[..27]).unwrap().text.is_empty());
        assert!(stream.push(&input[27..]).unwrap().text.is_empty());
        let protected = stream.flush().unwrap();
        assert!(!protected.text.contains(value));
        assert!(!protected.text.contains("blue horse battery"));
        assert_eq!(
            dictionary.hydrate_text(&protected.text).unwrap().text,
            input
        );
        let reopened = Redactor::with_registered(Persona::default(), MatcherHandle::default())
            .with_repository(repo.root())
            .unwrap();
        assert_eq!(reopened.try_scrub(&input).unwrap().text, protected.text);
        let json = serde_json::json!({"result": value});
        let protected_json = reopened.try_scrub_json(&json).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &dictionary
                    .hydrate_jsonl(&protected_json.value.to_string())
                    .unwrap()
                    .text
            )
            .unwrap(),
            json
        );
    }

    #[cfg(feature = "rc")]
    #[test]
    fn native_batch_keeps_identity_masks_local_and_reloads_explicit_policy() {
        use crate::domain::secret_filter::RepositoryDictionary;
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        let dictionary = RepositoryDictionary::open(repo.root()).unwrap();
        let secret = "Qz7mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr2dF";
        let identity = serde_json::json!({"identity":secret,"path":"/home/operator/work"});
        let credential = serde_json::json!({"token":secret,"text":"A quoted \"value\" and a newline\nremain readable."});
        let clean = serde_json::json!({"text":"No credentials here."});
        let redactor = Redactor::new(Persona {
            home: Some("/home/operator".into()),
            ..Default::default()
        })
        .with_repository(repo.root())
        .unwrap()
        .with_native_context("codex", "", repo.root(), repo.root());
        let records: &[(&serde_json::Value, &[&str])] = &[
            (&identity, &["/identity"]),
            (&credential, &[]),
            (&clean, &[]),
        ];
        let projected = redactor.scrub_native_batch(records);
        assert_eq!(projected.len(), records.len());
        assert_eq!(projected[0].value["identity"], secret);
        assert_eq!(projected[0].value["path"], "~/work");
        assert!(!projected[0].secret_projection);
        assert!(projected[1].secret_projection);
        assert!(!projected[1].value.to_string().contains(secret));
        let hydrated: serde_json::Value = serde_json::from_str(
            &dictionary
                .hydrate_jsonl(&projected[1].value.to_string())
                .unwrap()
                .text,
        )
        .unwrap();
        assert_eq!(hydrated, credential);
        assert_eq!(projected[2].value, clean);
        assert!(!projected[2].secret_projection);
        dictionary
            .block_add("explicit", secret.to_string().into(), false)
            .unwrap();
        let updated = redactor.scrub_native_batch(records);
        assert!(updated[0].secret_projection);
        assert!(!updated[0].value.to_string().contains(secret));
        assert_eq!(updated[2].value, clean);
    }

    #[cfg(feature = "rc")]
    #[test]
    fn native_batch_isolates_unprotectable_records_without_exposing_neighbors() {
        let dir = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(dir.path()).unwrap();
        let redactor = Redactor::new(Persona::default())
            .with_repository(repo.root())
            .unwrap()
            .with_native_context("codex", "", repo.root(), repo.root());
        let clean = serde_json::json!({"text":"The first reply stays readable."});
        let oversized = serde_json::json!({"text":format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            "A".repeat(65537)
        )});
        let secret = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let adjacent = serde_json::json!({"text":"The last reply stays readable.","token":secret});
        let projected =
            redactor.scrub_native_batch(&[(&clean, &[]), (&oversized, &[]), (&adjacent, &[])]);
        assert_eq!(projected.len(), 3);
        assert_eq!(projected[0].value, clean);
        assert!(projected[1].value.get("protection_error").is_some());
        assert!(projected[1].secret_projection);
        assert!(
            !projected[1]
                .value
                .to_string()
                .contains("BEGIN RSA PRIVATE KEY")
        );
        assert_eq!(projected[2].value["text"], adjacent["text"]);
        assert!(!projected[2].value.to_string().contains(secret));
        assert!(projected[2].secret_projection);
    }

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
    fn secrets_are_replaced_in_place() {
        // The fake token has to be high-entropy: the rule set carries an entropy threshold and
        // filters `"a".repeat(36)` out as a placeholder, so a test written with that exercises a
        // path that never fires.
        let leak = "ghp_7Kd2mQ9xR4vB1nT8sW3zY6cL5jH0gF2aE4pU";
        let r = scrub(persona(), &format!("token: {leak} done"));
        assert!(!r.text.contains(leak));
        assert!(r.text.contains("[redacted:github-pat]"), "{}", r.text);
        assert_eq!(r.secrets, 1);
    }

    #[test]
    fn deterministic_on_same_input() {
        let text = "/home/nana/x and /home/alice/y via 47.91.17.103 and ghp_".to_string()
            + &"z".repeat(36);
        let a = scrub(persona(), &text);
        let b = scrub(persona(), &text);
        assert_eq!(a.text, b.text);
    }

    #[test]
    fn empty_persona_still_scrubs_secrets() {
        let r = Redactor::new(Persona::default()).scrub("no persona here");
        assert_eq!(r.text, "no persona here");
        assert_eq!(r.paths, 0);
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn json_scrubbing_matches_semantic_strings_not_escape_bytes() {
        let secret = "quote\" slash\\ and\nnewline";
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_json", secret)]);
        let redactor = Redactor::with_registered(
            Persona::default(),
            crate::domain::secret_filter::MatcherHandle::new(matcher),
        );
        let input = serde_json::json!({"message": format!("before {secret} after")});
        let report = redactor.scrub_json(&input);
        assert_eq!(report.secrets, 1);
        assert_eq!(report.registered_ids, vec!["sec_json"]);
        assert_eq!(
            report.value["message"],
            "before [redacted:registered-secret] after"
        );
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn repository_placeholder_remains_opaque_across_every_chunk_boundary() {
        let token = "{{AGIT_SECRET_V1:00000000-0000-0000-0000-000000000000:sec_0123456789abcdef0123456789abcdef}}";
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_stream", "AGIT")]);
        let redactor = Redactor::with_registered(
            Persona::default(),
            crate::domain::secret_filter::MatcherHandle::new(matcher),
        );

        for split in 1..token.len() {
            let mut stream = redactor.stream();
            let mut output = stream.push(&token[..split]).unwrap().text;
            output.push_str(&stream.push(&token[split..]).unwrap().text);
            output.push_str(&stream.flush().unwrap().text);
            assert_eq!(
                output, token,
                "placeholder was changed at byte boundary {split}"
            );
        }
    }

    #[test]
    fn vendor_and_pem_fragments_are_not_released_before_finalization() {
        let token = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{token}\nprivate body");
        for text in [token, pem.as_str()] {
            for split in 1..text.len() {
                let mut stream = Redactor::new(Persona::default()).stream();
                let first = stream.push(&text[..split]).unwrap();
                let second = stream.push(&text[split..]).unwrap();
                assert!(first.text.is_empty() && second.text.is_empty());
                let output = stream.flush().unwrap().text;
                assert!(output.starts_with("[redacted:"));
                assert!(!output.contains(token));
                assert!(!output.contains("private body"));
            }
        }
    }

    #[cfg(feature = "secret-vault")]
    #[test]
    fn a_registered_header_cannot_erase_evidence_for_the_private_key_body() {
        let header = "-----BEGIN PRIVATE KEY-----";
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("sec_header", header)]);
        let redactor = Redactor::with_registered(
            Persona::default(),
            crate::domain::secret_filter::MatcherHandle::new(matcher),
        );
        let report = redactor.scrub(&format!("{header}\nprivate body material"));
        assert_eq!(report.text, "[redacted:registered-secret]");
        assert_eq!(report.registered_ids, ["sec_header"]);
    }

    #[test]
    fn exceeding_stream_capacity_cannot_release_a_suffix_on_retry_or_flush() {
        let mut stream = Redactor::new(Persona::default()).stream();
        assert!(
            stream
                .push(&"a".repeat(MAX_STREAM_ITEM_BYTES))
                .unwrap()
                .text
                .is_empty()
        );
        assert!(stream.push("b").is_err());
        assert!(stream.pending.is_empty());
        assert!(stream.push("suffix").is_err());
        assert!(stream.flush().is_err());
    }

    #[test]
    fn concurrent_items_share_a_reclaimable_buffer_budget() {
        let redactor = Redactor::new(Persona::default());
        let mut streams: Vec<_> = (0..MAX_STREAM_BUFFER_BYTES / MAX_STREAM_ITEM_BYTES)
            .map(|_| redactor.stream())
            .collect();
        let chunk = "a".repeat(MAX_STREAM_ITEM_BYTES);
        for stream in &mut streams {
            assert!(stream.push(&chunk).is_ok());
        }
        assert!(redactor.clone().stream().push("overflow").is_err());
        streams.pop();
        assert!(redactor.stream().push(&chunk).is_ok());
        drop(streams);
        assert_eq!(redactor.buffered_stream_bytes.load(Ordering::Relaxed), 0);
    }
}
