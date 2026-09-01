//! Rule set and rule engine.
//!
//! # Why gitleaks' rules and our own engine
//!
//! A dozen hand-written rules do not hold up against the real world: the secret shapes that have
//! turned up in transcripts go far past AWS and GitHub. The gitleaks rule set (222 rules) is the
//! most used one for this job, MIT licensed, and can be vendored directly. **Execution** does not
//! need its binary: Go's `regexp` is RE2, the same family as Rust's `regex` (no lookahead, no
//! backreferences), and 221 regexes carry over verbatim and all compile.
//!
//! What matters more is "one source for both sides": the server-side gate is pure Rust, with no
//! Node and no Go runtime. The rules live in `agit::domain::secrets` (not behind the `cli`
//! feature), so a backend depending on this crate with `default-features = false` gets the same
//! rules — two rule sets are certain to drift, and the one that drifts rots first.
//!
//! # Performance: the keyword prefilter is a requirement, not an optimization
//!
//! Observed on a 2.6MB transcript: running all 221 regexes takes **16.6 seconds**, and the
//! keyword prefilter brings that down to **27ms**; compiling the whole set once already costs
//! 1.37 seconds. Two hard constraints follow:
//!
//! 1. **Lazy compilation** — only a rule the prefilter hits is ever compiled into a `Regex`.
//! 2. **One prefilter pass** — every rule's keywords go into one Aho–Corasick automaton and the
//!    whole text is walked once. Per-rule `contains()` is O(rules × text length), and
//!    330 passes × 2.6MB lands on the order of a second, which is the same as no prefilter.
//!
//! # TOML fields that are not implemented
//!
//! This engine scans **session content** (jsonl / VIEW / shared files / commit message), not an
//! arbitrary source tree, so the fields below are deliberately ignored — written down here so it
//! does not pretend to support all of them:
//!
//! * `path` / `paths` (rule-level and allowlist-level): switch rules on and off by file path. Our
//!   scan surface is a fixed set of session files, and no path pattern describes them.
//!   **Ignoring rule-level `path` means those 5 rules are always in effect here** — the direction
//!   is better a false positive than a miss. Ignoring allowlist-level `paths` means those few
//!   allows no longer apply (conservative in the same direction); the one exception is an entry
//!   with `condition = "AND"`: dropping one AND test **widens** the allow, so such an entry is
//!   voided whole (see [`Allow::unsupported`]).
//! * `regexTarget = "line"` is implemented; metadata such as `commits` / `description` takes no
//!   part in the verdict.
//! * The global `[allowlist]` takes only `regexes` and `stopwords` (both apply to the secret);
//!   `paths` is ignored as above.

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};

use aho_corasick::AhoCorasick;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// The vendored gitleaks rule set; the file header carries its provenance and MIT license.
const GITLEAKS_TOML: &str = include_str!("gitleaks.toml");
/// agit's own rules: shapes gitleaks cannot have, or does not reach and that have bitten us.
const AGIT_TOML: &str = include_str!("agit-rules.toml");

/// Upper bound on the size of a compiled regex program.
///
/// 3 of the official rules exceed `regex`'s default 10MB limit (all of them very long literal
/// alternations); at 64MB all 221 compile. Nothing here is syntactically incompatible — the
/// default budget is too small.
const SIZE_LIMIT: usize = 64 << 20;

/// Cache limit for the lazy DFA. **This one is the performance watershed, not a tuning knob.**
///
/// A rule like `generic-api-key` is one long `(?i)` alternation of literals with a large NFA
/// state count; when the cache cannot hold it the lazy DFA clears itself over and over and
/// finally falls back to PikeVM character-by-character simulation. Observed scanning 2MB: the
/// default (2MB cache) **916ms**, raised to 64MB **4.2ms** — 220x. `hashicorp-tf-password` is
/// 519ms → 3.1ms. Leaving this value alone means these two rules eat back everything the keyword
/// prefilter saves, and a gate that answers in seconds is no gate at all (the hint spells out how
/// to bypass it).
const DFA_SIZE_LIMIT: usize = 64 << 20;

// ── TOML schema ─────────────────────────────────────────────────────────

/// `entropy = 4` is an integer in TOML and `entropy = 3.2` is a float. One field with two types,
/// so a plain `Option<f64>` will not do.
#[derive(Deserialize)]
#[serde(untagged)]
enum Num {
    Float(f64),
    Int(i64),
}

impl Num {
    fn as_f64(&self) -> f64 {
        match self {
            Num::Float(f) => *f,
            Num::Int(i) => *i as f64,
        }
    }
}

#[derive(Deserialize)]
struct RawConfig {
    allowlist: Option<RawAllow>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
struct RawRule {
    id: String,
    #[serde(default)]
    description: String,
    /// `pkcs12-file` has only a path and no regex — a path-only rule has nothing to judge here,
    /// so it is skipped.
    regex: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    entropy: Option<Num>,
    #[serde(rename = "secretGroup")]
    secret_group: Option<usize>,
    #[serde(default)]
    allowlists: Vec<RawAllow>,
}

#[derive(Deserialize)]
struct RawAllow {
    condition: Option<String>,
    #[serde(rename = "regexTarget")]
    regex_target: Option<String>,
    #[serde(default)]
    regexes: Vec<String>,
    #[serde(default)]
    stopwords: Vec<String>,
    /// Unimplemented; it only decides whether this allowlist is voided as a result (see
    /// [`Allow::unsupported`]).
    #[serde(default)]
    paths: Vec<String>,
}

// ── Compiled form ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Secret,
    Match,
    Line,
}

struct Allow {
    target: Target,
    /// `condition = "AND"`: every declared test must hit before it allows.
    require_all: bool,
    /// This allowlist carries a test this engine does not implement (`paths`) under AND
    /// semantics. Dropping one AND test makes the allow **wider**, so the whole entry is voided —
    /// better an allow that stops working than one that reaches past its authority.
    unsupported: bool,
    regexes: Vec<Regex>,
    stopwords: Option<AhoCorasick>,
}

impl Allow {
    fn compile(raw: &RawAllow) -> Allow {
        let require_all = raw.condition.as_deref() == Some("AND");
        Allow {
            target: match raw.regex_target.as_deref() {
                Some("match") => Target::Match,
                Some("line") => Target::Line,
                // gitleaks' default target is the secret.
                _ => Target::Secret,
            },
            require_all,
            unsupported: require_all && !raw.paths.is_empty(),
            // An allow entry that does not compile is dropped: one allow fewer = a few more
            // false positives, and that direction is safe (the other way around — treating a
            // non-compiling entry as "allow" — silently lets a real secret through).
            regexes: raw.regexes.iter().filter_map(|r| build(r)).collect(),
            stopwords: (!raw.stopwords.is_empty())
                .then(|| {
                    AhoCorasick::builder()
                        .ascii_case_insensitive(true)
                        .build(&raw.stopwords)
                        .ok()
                })
                .flatten(),
        }
    }

    fn allows(&self, secret: &str, whole: &str, line: &str) -> bool {
        if self.unsupported {
            return false;
        }
        let hay = match self.target {
            Target::Secret => secret,
            Target::Match => whole,
            Target::Line => line,
        };
        let re_hit = self.regexes.iter().any(|r| r.is_match(hay));
        // A stopword always looks at the secret, as in gitleaks: it asks whether this value is
        // itself a well-known fake, which has nothing to do with `regexTarget`.
        let sw_hit = self
            .stopwords
            .as_ref()
            .is_some_and(|ac| ac.is_match(secret));
        if self.require_all {
            let declared_re = !self.regexes.is_empty();
            let declared_sw = self.stopwords.is_some();
            (declared_re || declared_sw) && (!declared_re || re_hit) && (!declared_sw || sw_hit)
        } else {
            re_hit || sw_hit
        }
    }
}

struct Compiled {
    re: Regex,
    allows: Vec<Allow>,
}

pub struct Rule {
    pub id: String,
    pub description: String,
    src: String,
    /// All lowercase. Empty means the rule takes no part in the prefilter and runs every time.
    pub keywords: Vec<String>,
    entropy: Option<f64>,
    secret_group: Option<usize>,
    raw_allows: Vec<RawAllow>,
    /// Lazy: compiling all 221 takes 1.37 seconds, while one scan usually needs only a handful
    /// of rules to actually run.
    lazy: OnceLock<Option<Compiled>>,
}

fn build(src: &str) -> Option<Regex> {
    RegexBuilder::new(src)
        .size_limit(SIZE_LIMIT)
        .dfa_size_limit(DFA_SIZE_LIMIT)
        .build()
        .ok()
}

impl Rule {
    fn compiled(&self) -> Option<&Compiled> {
        self.lazy
            .get_or_init(|| {
                let re = build(&self.src)?;
                Some(Compiled {
                    re,
                    allows: self.raw_allows.iter().map(Allow::compile).collect(),
                })
            })
            .as_ref()
    }

    pub fn regex(&self) -> Option<&Regex> {
        self.compiled().map(|c| &c.re)
    }

    /// What a hit is weighed against: entropy, the rule's own allowlist, the global allowlist.
    ///
    /// `line` is the source line the hit sits on; an allow with `regexTarget = "line"` reads it.
    pub fn accepts(&self, secret: &str, whole: &str, line: &str) -> bool {
        if let Some(min) = self.entropy
            && shannon(secret) <= min
        {
            return false;
        }
        if let Some(c) = self.compiled()
            && c.allows.iter().any(|a| a.allows(secret, whole, line))
        {
            return false;
        }
        !global_allow().iter().any(|a| a.allows(secret, whole, line))
    }

    /// Which span of a match is "the actual secret".
    ///
    /// As in gitleaks: the group named by `secretGroup` wins, otherwise the first capture group
    /// that took part in the match, and only with neither does it fall back to the whole match.
    /// Rules generally frame the secret in parentheses and leave the variable name outside, so
    /// this step decides directly whether the entropy comes out right — entropy over the whole
    /// match is dragged down by the low-entropy variable name in front of it, and the result is
    /// **a miss**.
    pub fn secret_span<'t>(&self, caps: &regex::Captures<'t>) -> (usize, usize, &'t str) {
        let whole = caps.get(0).expect("group 0 always exists");
        let pick = self
            .secret_group
            .and_then(|g| caps.get(g))
            .or_else(|| (1..caps.len()).find_map(|i| caps.get(i)))
            .unwrap_or(whole);
        (pick.start(), pick.end(), pick.as_str())
    }

    /// Whether the rule has capture groups — without them `find_iter` is enough, and faster than
    /// `captures_iter`.
    pub fn has_groups(&self) -> bool {
        self.regex().is_some_and(|r| r.captures_len() > 1)
    }
}

/// Shannon entropy: `H = -Σ p_i·log2(p_i)`, where p_i is the frequency of each character.
///
/// gitleaks uses it to keep low-entropy fakes like `token = "changeme_placeholder"` out of the
/// report.
pub fn shannon(s: &str) -> f64 {
    let mut counts: HashMap<char, f64> = HashMap::new();
    let mut n = 0f64;
    for c in s.chars() {
        *counts.entry(c).or_insert(0.0) += 1.0;
        n += 1.0;
    }
    if n == 0.0 {
        return 0.0;
    }
    -counts
        .values()
        .map(|&c| {
            let p = c / n;
            p * p.log2()
        })
        .sum::<f64>()
}

// ── Loading ─────────────────────────────────────────────────────────────

fn parse(toml_src: &str, into: &mut Vec<Rule>, global: &mut Vec<RawAllow>) {
    // The rule files are vendored constants, so a parse failure can only mean we broke a file
    // ourselves. But it **must not panic**: a bad file that takes down push is worse than a few
    // missing rules.
    let Ok(cfg) = toml::from_str::<RawConfig>(toml_src) else {
        debug_assert!(false, "a vendored rule file must parse");
        return;
    };
    if let Some(a) = cfg.allowlist {
        global.push(a);
    }
    for r in cfg.rules {
        let Some(src) = r.regex else { continue };
        into.push(Rule {
            id: r.id,
            description: r.description,
            src,
            keywords: r.keywords.iter().map(|k| k.to_lowercase()).collect(),
            entropy: r.entropy.map(|e| e.as_f64()),
            secret_group: r.secret_group,
            raw_allows: r.allowlists,
            lazy: OnceLock::new(),
        });
    }
}

struct RuleSet {
    rules: Vec<Rule>,
    global: Vec<RawAllow>,
    /// keyword → the indices of the rules that use it.
    prefilter: Option<AhoCorasick>,
    owners: Vec<Vec<usize>>,
    /// Rules with no keyword: the prefilter cannot reach them, so they run every time.
    always: Vec<usize>,
}

static SET: LazyLock<RuleSet> = LazyLock::new(|| {
    let mut rules = vec![];
    let mut global = vec![];
    parse(GITLEAKS_TOML, &mut rules, &mut global);
    parse(AGIT_TOML, &mut rules, &mut global);

    // Keyword dedup: the 221 rules share far fewer keywords than they have rules, and the
    // automaton is built once.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut words: Vec<String> = vec![];
    let mut owners: Vec<Vec<usize>> = vec![];
    let mut always: Vec<usize> = vec![];
    for (ri, r) in rules.iter().enumerate() {
        if r.keywords.is_empty() {
            always.push(ri);
            continue;
        }
        for k in &r.keywords {
            let wi = *index.entry(k.clone()).or_insert_with(|| {
                words.push(k.clone());
                owners.push(vec![]);
                words.len() - 1
            });
            owners[wi].push(ri);
        }
    }
    let prefilter = AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(&words)
        .ok();
    RuleSet {
        rules,
        global,
        prefilter,
        owners,
        always,
    }
});

static GLOBAL_ALLOW: OnceLock<Vec<Allow>> = OnceLock::new();

fn global_allow() -> &'static [Allow] {
    GLOBAL_ALLOW.get_or_init(|| SET.global.iter().map(Allow::compile).collect())
}

/// How many rules are loaded (counting only those with a regex).
pub fn count() -> usize {
    SET.rules.len()
}

pub fn all() -> &'static [Rule] {
    &SET.rules
}

/// The prefilter: one Aho–Corasick pass over the whole text, returning the rules whose regex
/// still has to run.
///
/// The text is not `to_lowercase()`d: the automaton itself is ASCII case-insensitive and the
/// keywords are all ASCII literals. That saves an MB-scale allocation plus a full rewrite.
pub fn candidates(text: &str) -> Vec<&'static Rule> {
    let mut on = vec![false; SET.rules.len()];
    for &i in &SET.always {
        on[i] = true;
    }
    if let Some(ac) = &SET.prefilter {
        // Overlapping: `key` can sit entirely inside `keystore`, and non-overlapping iteration
        // misses it.
        for m in ac.find_overlapping_iter(text) {
            for &ri in &SET.owners[m.pattern().as_usize()] {
                on[ri] = true;
            }
        }
    }
    SET.rules
        .iter()
        .enumerate()
        .filter(|(i, _)| on[*i])
        .map(|(_, r)| r)
        .collect()
}

/// For audit: compile every rule and return the ids that fail.
///
/// Called only from tests — it pays the 1.37 seconds lazy compilation deliberately avoids. A rule
/// that fails to compile is a silent hole, so a test has to pin "zero failures".
pub fn audit() -> Vec<&'static str> {
    SET.rules
        .iter()
        .filter(|r| r.regex().is_none())
        .map(|r| r.id.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored rule files must actually parse into rules.
    ///
    /// This pins the silent-failure case: if TOML parsing fails over one mismatched field type,
    /// `parse` quietly returns an empty table and scanning becomes permanently zero-hit — worse
    /// than having no gate, because it still prints a "clean scan" line.
    #[test]
    fn the_vendored_ruleset_actually_loads() {
        assert!(
            count() >= 220,
            "only {} rules loaded; the rule files most likely did not parse",
            count()
        );
        for id in ["aws-access-token", "generic-api-key", "private-key"] {
            assert!(
                all().iter().any(|r| r.id == id),
                "gitleaks rule {id} must load"
            );
        }
        for id in [
            "agit-token",
            "agit-private-key-header",
            "agit-credentials-in-url",
        ] {
            assert!(all().iter().any(|r| r.id == id), "agit rule {id} must load");
        }
    }

    /// A rule that cannot compile is a hole that never alarms.
    #[test]
    fn every_rule_compiles() {
        let bad = audit();
        assert!(bad.is_empty(), "these rules fail to compile: {bad:?}");
    }

    #[test]
    fn entropy_separates_real_secrets_from_placeholders() {
        assert!(shannon("aaaaaaaaaaaaaaaa") < 1.0);
        assert!(shannon("kR7wQ2mZ9xVb4Ntc") > 3.0);
        assert_eq!(shannon(""), 0.0);
    }

    /// The prefilter must actually narrow the set, or the performance promise is a lie.
    #[test]
    fn the_prefilter_narrows_the_ruleset() {
        // CJK fixture: ordinary prose in another script must not wake the keyword rules, so the
        // sample stays Chinese.
        let picked = candidates("今天把退款流程理了一遍，没有任何凭据。");
        assert!(
            picked.len() < 10,
            "a passage of ordinary Chinese prose must not wake {} rules",
            picked.len()
        );
        assert!(
            candidates("ghp_0123456789abcdefghij")
                .iter()
                .any(|r| r.id == "github-pat"),
            "a rule whose keyword hits must enter the candidate set"
        );
    }
}
