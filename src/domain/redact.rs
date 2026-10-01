//! Content redaction: erasing the coordinates before publishing.
//!
//! Secret replacement is delegated to the optional local privacy service, which persists
//! reversible mappings before returning replacements. Persona masking remains independent.
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

#[cfg(feature = "rc")]
pub(crate) const PROTECTION_ERROR_TEXT: &str = "[content unavailable: invalid native record]";

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
    #[cfg(feature = "secret-vault")]
    fallback_fingerprint: String,
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
            #[cfg(feature = "secret-vault")]
            fallback_fingerprint: uuid::Uuid::new_v4().to_string(),
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

    pub fn try_scrub(&self, text: &str) -> crate::Result<Report> {
        Ok(self.scrub(text))
    }
    pub fn try_scrub_json(&self, value: &serde_json::Value) -> crate::Result<JsonReport> {
        Ok(self.scrub_json(value))
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
    pub(crate) fn publication_fingerprint(&self) -> crate::Result<String> {
        let policy = self
            .dictionary_query("fingerprint")
            .unwrap_or_else(|| serde_json::json!(&self.fallback_fingerprint));
        Ok(crate::domain::privacy_envelope::digest_bytes(
            &serde_json::to_vec(&(
                &self.persona.username,
                &self.persona.home,
                &self.persona.hostname,
                policy,
            ))?,
        ))
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
        let (runtime, session) = self
            .native
            .try_lock()
            .map(|native| native.clone())
            .unwrap_or_default();
        let batch = crate::domain::privacy::projector::NativeBatch {
            runtime,
            session,
            records: records.iter().map(|(value, _)| (*value).clone()).collect(),
            pointers: records
                .iter()
                .map(|(_, pointers)| pointers.iter().map(|p| (*p).to_owned()).collect())
                .collect(),
        };
        let projected = serde_json::to_string(&batch).ok().map(|input| {
            crate::domain::privacy::service::transform(
                self.repo.as_deref(),
                &input,
                crate::domain::privacy::projector::Mode::ProtectNative,
            )
        });
        let values = projected
            .and_then(|outcome| {
                serde_json::from_str::<Vec<serde_json::Value>>(&outcome.content).ok()
            })
            .filter(|values| values.len() == records.len())
            .unwrap_or(batch.records);
        timing(
            "dictionary_protect_ms",
            started.elapsed().as_secs_f64() * 1000.0,
        );
        values
            .into_iter()
            .zip(records)
            .map(|(mut value, (original, _))| {
                let secret_projection = value != **original;
                self.scrub_json_inner(&mut value, &mut JsonTotals::default());
                NativeJson {
                    value,
                    secret_projection,
                    registered_ids: Vec::new(),
                }
            })
            .collect()
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
        }
    }

    /// Redact one text. Deterministic: same input, same persona ⇒ same output.
    pub fn scrub(&self, text: &str) -> Report {
        #[cfg(feature = "cli")]
        let protected = crate::domain::privacy::service::transform(
            self.repo.as_deref(),
            text,
            crate::domain::privacy::projector::Mode::ProtectJsonl,
        );
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
        report
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
        let protected = crate::domain::privacy::service::transform(
            self.repo.as_deref(),
            &serde_json::to_string(value).unwrap_or_default(),
            crate::domain::privacy::projector::Mode::ProtectJsonl,
        );
        #[cfg(feature = "cli")]
        let mut value = serde_json::from_str(&protected.content).unwrap_or_else(|_| value.clone());
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
        JsonReport {
            value,
            secrets: totals.secrets,
            paths: totals.paths,
            ips: totals.ips,
            registered_ids: totals.registered_ids.into_iter().collect(),
        }
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

/// A bounded native tail gives the worker context across transport chunks. Failed processing
/// releases the original bytes so live delivery cannot depend on privacy availability.
pub struct StreamRedactor {
    redactor: Redactor,
    pending: String,
}

impl StreamRedactor {
    pub fn push(&mut self, chunk: &str) -> Report {
        self.pending.push_str(chunk);
        #[cfg(feature = "cli")]
        {
            let outcome = crate::domain::privacy::service::transform(
                self.redactor.repo.as_deref(),
                &self.pending,
                crate::domain::privacy::projector::Mode::ProtectStream,
            );
            let consumed = outcome.consumed.unwrap_or(self.pending.len());
            if consumed > self.pending.len() || !self.pending.is_char_boundary(consumed) {
                return self.flush();
            }
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
        if self.pending.is_empty() {
            return empty_report();
        }
        let ready = std::mem::take(&mut self.pending);
        self.redactor.scrub(&ready)
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

    #[test]
    fn empty_persona_keeps_ordinary_text() {
        let r = Redactor::new(Persona::default()).scrub("no persona here");
        assert_eq!(r.text, "no persona here");
        assert_eq!(r.paths, 0);
    }
}
