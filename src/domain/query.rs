//! Search query syntax: `term term qualifier:value -excluded "phrase with spaces"`.
//!
//! # Why the syntax is defined in this crate
//!
//! The backend parses queries, the CLI validates them, and the MCP tool description has to
//! spell out which qualifiers work — all three must recognize the same syntax. Two
//! implementations drift apart silently: the CLI accepts `in:prompt` while the backend only
//! knows `in:user`, and what the user gets is a qualifier searched as an ordinary term — the
//! hardest class of failure to track down (there are results, but they are wrong).
//!
//! Same reason the IR has only one implementation, so this sits in `domain` rather than behind
//! the `cli` feature.
//!
//! # The categories are not copied from GitHub
//!
//! GitHub searches homogeneous text (lines of code, issue bodies). This searches a
//! **structured event stream**: a hit landing on a user prompt, on a tool call, or on a compact
//! summary are three different things —
//!
//! * on [`EventKind::UserPrompt`] = **someone asked about this**
//! * on `ToolUse` = **someone ran this command**
//! * on `CompactSummary` = a secondhand paraphrase; the original words are already folded away
//!
//! So `in:` is the core qualifier of this syntax, and GitHub has no counterpart.
//!
//! [`EventKind::UserPrompt`]: crate::adapter::EventKind::UserPrompt

use super::text;
use serde::{Deserialize, Serialize};

/// The entity type being searched.
///
/// The four come from this product's own model (agent / session / PR / namespace), not from
/// renaming GitHub's code/repo/issue/user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchType {
    /// Content inside session transcripts. **The default** — "has anyone done this" is the
    /// main question.
    #[default]
    Sessions,
    /// The agent itself (name, owner, opening gist).
    Agents,
    /// A PR's title / body / reconciliation summary.
    Prs,
    /// Users and organizations. Both live in one namespace table, and splitting them into two
    /// tabs leaves the reader typing a name without knowing which tab to click — so they
    /// merge, and every hit labels its own kind.
    People,
}

impl SearchType {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchType::Sessions => "sessions",
            SearchType::Agents => "agents",
            SearchType::Prs => "prs",
            SearchType::People => "people",
        }
    }

    /// Every type, in tab order.
    pub const ALL: [SearchType; 4] = [
        SearchType::Sessions,
        SearchType::Agents,
        SearchType::Prs,
        SearchType::People,
    ];

    pub fn parse(s: &str) -> Option<SearchType> {
        match s.trim().to_lowercase().as_str() {
            // Plural and singular are both accepted: `type=session` in a URL is far too easy
            // a mistake to make, and answering it with an "unknown type" 400 helps nobody.
            "sessions" | "session" => Some(SearchType::Sessions),
            "agents" | "agent" => Some(SearchType::Agents),
            "prs" | "pr" | "pull_requests" => Some(SearchType::Prs),
            "people" | "person" | "users" | "user" | "orgs" | "org" => Some(SearchType::People),
            _ => None,
        }
    }
}

/// The values of `in:` — which class of session event a hit may land on.
///
/// This is where this search differs from GitHub: the unit of search is not "a line in a file"
/// but **one semantic event**. So every scope has to answer a question in plain words (see the
/// variants).
///
/// `TurnEnd` and `Other` deliberately get no scope: the former is the runtime's turn marker
/// (the IR models it only so it is not mistaken for content), the latter is what the IR does
/// not express. Making them selectable promises "you can find it" when they have no searchable
/// body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// What the user said. "**Has anyone asked** about this?"
    Prompt,
    /// The agent's reply. "**Has anyone been answered** this way?"
    Reply,
    /// A tool call. "**Has anyone run** this command?"
    Tool,
    /// Tool output. "**Has a command printed** this value?"
    ///
    /// Deliberately separate from [`Self::Tool`]: that scope is intent (what was run), this
    /// one is fact (what came out). In practice a large share of the distinct
    /// configuration-assignment forms appear only in tool output, and merging the two into one
    /// scope drowns `in:tool` in log noise.
    Output,
    /// A file edit. "**Has anyone touched** this file?"
    Edit,
    /// Content inside a compact boundary. **Secondhand** — see [`Query::scopes`].
    Summary,
}

impl EventScope {
    pub fn as_str(self) -> &'static str {
        match self {
            EventScope::Prompt => "prompt",
            EventScope::Reply => "reply",
            EventScope::Tool => "tool",
            EventScope::Output => "output",
            EventScope::Edit => "edit",
            EventScope::Summary => "summary",
        }
    }

    pub const ALL: [EventScope; 6] = [
        EventScope::Prompt,
        EventScope::Reply,
        EventScope::Tool,
        EventScope::Output,
        EventScope::Edit,
        EventScope::Summary,
    ];

    pub fn parse(s: &str) -> Option<EventScope> {
        match s.trim().to_lowercase().as_str() {
            "prompt" | "prompts" | "user" => Some(EventScope::Prompt),
            "reply" | "replies" | "assistant" => Some(EventScope::Reply),
            "tool" | "tools" => Some(EventScope::Tool),
            // `result` is an alias, not the primary name: `in:output` says "is it in the
            // output", while `in:result` reads as "did this call succeed" — that is what
            // `outcome` covers.
            "output" | "outputs" | "result" | "results" => Some(EventScope::Output),
            "edit" | "edits" | "file" => Some(EventScope::Edit),
            "summary" | "compact" => Some(EventScope::Summary),
            _ => None,
        }
    }

    /// Whether content in this scope is secondhand (compact has rewritten it).
    pub fn is_secondhand(self) -> bool {
        matches!(self, EventScope::Summary)
    }
}

/// Sort order.
///
/// `Best` (scored) is the default, because "has anyone done this" wants the most relevant
/// result, not the newest one. `Recent` / `Turns` are this domain's own dimensions: the length
/// of a session is itself a signal (a 50-turn session is likelier to have actually solved the
/// problem than a 2-turn one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    #[default]
    Best,
    Recent,
    Turns,
}

impl Sort {
    pub fn as_str(self) -> &'static str {
        match self {
            Sort::Best => "best",
            Sort::Recent => "recent",
            Sort::Turns => "turns",
        }
    }

    pub fn parse(s: &str) -> Option<Sort> {
        match s.trim().to_lowercase().as_str() {
            "best" | "match" | "relevance" => Some(Sort::Best),
            "recent" | "updated" | "new" => Some(Sort::Recent),
            "turns" | "length" => Some(Sort::Turns),
            _ => None,
        }
    }
}

/// Numeric comparison: `turns:>20`, `turns:<5`, `turns:10`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumFilter {
    AtLeast(usize),
    AtMost(usize),
    Exactly(usize),
}

impl NumFilter {
    pub fn parse(s: &str) -> Option<NumFilter> {
        let s = s.trim();
        // `>=` / `<=` must be tried before `>` / `<`, or the `=20` in `>=20` is read as
        // the number.
        if let Some(n) = s.strip_prefix(">=") {
            return n.trim().parse().ok().map(NumFilter::AtLeast);
        }
        if let Some(n) = s.strip_prefix("<=") {
            return n.trim().parse().ok().map(NumFilter::AtMost);
        }
        if let Some(n) = s.strip_prefix('>') {
            // `>20` is "more than 20" = at least 21. Off-by-one is easy here; spell it out.
            return n
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|v| v.checked_add(1))
                .map(NumFilter::AtLeast);
        }
        if let Some(n) = s.strip_prefix('<') {
            return n
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|v| v.checked_sub(1))
                .map(NumFilter::AtMost);
        }
        s.parse().ok().map(NumFilter::Exactly)
    }

    pub fn matches(self, v: usize) -> bool {
        match self {
            NumFilter::AtLeast(n) => v >= n,
            NumFilter::AtMost(n) => v <= n,
            NumFilter::Exactly(n) => v == n,
        }
    }
}

/// One parsed query.
///
/// # Semantics: terms are ANDed
///
/// There is no `OR`. The reason is not "hard to implement" but that **the default must be
/// predictable**: `a OR b` and `a b` look almost the same in one input box, while the two
/// result sets differ by an order of magnitude. A user who wants a wider net searches twice;
/// a user whose net an `OR` quietly widened never notices what happened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    /// Terms that must all hit (case-insensitive substrings).
    ///
    /// **Substring, not tokenized**: the corpus mixes Chinese and English, and Chinese has no
    /// word boundaries. Splitting on whitespace makes `缓存配置` find nothing inside
    /// `配置缓存策略`, because a whole Chinese clause is often one "word". Substring matching
    /// happens to be right for Chinese; the cost is `cat` hitting `concatenate` in English —
    /// a far smaller cost.
    pub terms: Vec<String>,
    /// Terms that drop a result as soon as they hit (`-foo`).
    pub exclude: Vec<String>,
    /// Only these event types count as a hit. Empty = all of them (but compact summaries are
    /// weighted down, see [`Self::scopes`]).
    pub scopes: Vec<EventScope>,
    /// `owner:<name>` — restrict to an owner (person or organization).
    pub owner: Option<String>,
    /// `agent:<owner>/<name>` or `agent:<name>` — restrict to one agent.
    pub agent: Option<String>,
    /// `runtime:<claude-code|codex|...>`.
    pub runtime: Option<String>,
    /// `category:<general|demo|research|eval>`.
    pub category: Option<String>,
    /// `tool:<name>` — count a hit only on calls of this tool.
    pub tool: Option<String>,
    /// `path:<fragment>` — the path of a file-edit event contains it.
    pub path: Option<String>,
    /// `state:<open|merged|closed>` — PR state.
    pub state: Option<String>,
    /// `is:public` / `is:private`.
    pub visibility: Option<String>,
    /// `is:fork` (`-is:fork` lands on `Some(false)`).
    pub fork: Option<bool>,
    /// `turns:>20` and the like.
    pub turns: Option<NumFilter>,
    /// Unrecognized qualifiers, carried out unchanged so the caller can tell the user.
    ///
    /// **Not searched as ordinary terms**: that returns a pile of irrelevant results while the
    /// user believes the qualifier took effect — "there are results but they are wrong" is the
    /// hardest failure to track down.
    pub unknown: Vec<String>,
}

impl Query {
    /// Parse one query string.
    ///
    /// Never fails: an unrecognized qualifier goes into [`Self::unknown`], and the caller
    /// decides whether to hint or to reject.
    pub fn parse(input: &str) -> Query {
        let mut q = Query::default();
        for tok in tokenize(input) {
            let quoted = tok.quoted;
            let (negated, body) = match tok.body.strip_prefix('-') {
                // A lone `-` is not a negation, it is an ordinary term.
                Some(rest) if !rest.is_empty() => (true, rest.to_string()),
                _ => (false, tok.body),
            };

            let Some((key, value)) = split_qualifier(&body) else {
                // Not a (recognized) qualifier = an ordinary term.
                //
                // Two kinds of "unrecognized" have to stay apart: `http://x` is a term to
                // begin with, while `runtim:codex` is a user who **meant a qualifier and
                // mistyped it**. Taking the second silently as an ordinary term returns
                // inexplicable results, and the user does not suspect their own syntax —
                // they suspect search is broken.
                //
                // So anything shaped like a qualifier is reported in `unknown` and still
                // searched as an ordinary term: the report exists to be seen, not to fail
                // this query.
                if looks_like_qualifier(&body) {
                    q.unknown.push(body.clone());
                }
                if negated {
                    // **An excluded term is never split.**
                    //
                    // `exclude` means "drop this result as soon as any one of them hits", so
                    // splitting it **narrows** the result set: once `-配置缓存` becomes
                    // `配置` / `缓存`, a session that only mentions `缓存` is excluded too.
                    // The safety net for term splitting is that it can only grow the hit set
                    // — for an excluded term, not splitting is what satisfies that.
                    q.exclude.push(body);
                } else if quoted {
                    // A quoted phrase goes in unchanged: the user explicitly asked for
                    // "appearing next to each other", and splitting loses that meaning.
                    q.terms.push(body);
                } else {
                    // Only a bare term is split. A whole Chinese query arriving intact makes
                    // the substring AND hit nothing — `stage1的lr是多少` returns no result
                    // (see `domain::text::query_terms`).
                    let pieces = text::query_terms(&body);
                    if pieces.is_empty() {
                        // The whole string is stopwords (`是多少`). Keep it unchanged rather
                        // than drop it: dropping turns the query into "no terms", and that is
                        // a different state from "searched and found nothing" — the caller
                        // tells them apart with `has_terms()`.
                        q.terms.push(body);
                    } else {
                        for p in pieces {
                            if !q.terms.contains(&p) {
                                q.terms.push(p);
                            }
                        }
                    }
                }
                continue;
            };

            // Negation has an explicit representation only for the `is:` values
            // handled below.  Treat every other negated qualifier as unsupported
            // instead of silently applying its positive meaning (`-owner:alice`
            // must never return only Alice's sessions).
            if negated && key != "is" {
                q.unknown.push(format!("-{body}"));
                continue;
            }

            match key.as_str() {
                "in" => match EventScope::parse(&value) {
                    Some(s) if !q.scopes.contains(&s) => q.scopes.push(s),
                    Some(_) => {}
                    None => q.unknown.push(body.clone()),
                },
                "owner" | "user" | "org" => q.owner = Some(value.to_lowercase()),
                "agent" | "repo" => q.agent = Some(value.to_lowercase()),
                "runtime" => q.runtime = Some(value.to_lowercase()),
                "category" => q.category = Some(value.to_lowercase()),
                "tool" => q.tool = Some(value.to_lowercase()),
                "path" => q.path = Some(value),
                "state" => q.state = Some(value.to_lowercase()),
                "turns" => match NumFilter::parse(&value) {
                    Some(n) => q.turns = Some(n),
                    None => q.unknown.push(body.clone()),
                },
                "is" => match value.to_lowercase().as_str() {
                    "public" | "private" if negated => {
                        // `-is:public` is `is:private`. Two spellings of the same thing land
                        // in one field; no separate "negated visibility" state is added.
                        q.visibility = Some(
                            if value.eq_ignore_ascii_case("public") {
                                "private"
                            } else {
                                "public"
                            }
                            .into(),
                        );
                    }
                    "public" => q.visibility = Some("public".into()),
                    "private" => q.visibility = Some("private".into()),
                    "fork" | "forked" => q.fork = Some(!negated),
                    "open" | "merged" | "closed" if !negated => {
                        q.state = Some(value.to_lowercase())
                    }
                    _ if negated => q.unknown.push(format!("-{body}")),
                    _ => q.unknown.push(body.clone()),
                },
                _ => q.unknown.push(body.clone()),
            }
        }
        q
    }

    /// Whether there are searchable body terms. A qualifier alone is a valid query
    /// (`owner:alice` lists everything of hers).
    pub fn has_terms(&self) -> bool {
        !self.terms.is_empty()
    }

    /// A wholly empty query — no terms and no qualifiers; it returns nothing.
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.exclude.is_empty()
            && self.scopes.is_empty()
            && self.owner.is_none()
            && self.agent.is_none()
            && self.runtime.is_none()
            && self.category.is_none()
            && self.tool.is_none()
            && self.path.is_none()
            && self.state.is_none()
            && self.visibility.is_none()
            && self.fork.is_none()
            && self.turns.is_none()
    }

    /// Whether this event scope may take part in a hit.
    ///
    /// With no `in:`, all of them are allowed — compact summaries included. **A summary is not
    /// excluded by default**, because sometimes the original has already been compacted away
    /// and the summary is the only clue left; but it must be labeled secondhand (the hit
    /// carries `secondhand`), or the reader takes a paraphrase for someone's own words.
    pub fn allows(&self, scope: EventScope) -> bool {
        self.scopes.is_empty() || self.scopes.contains(&scope)
    }

    /// Whether a stretch of text satisfies every term condition (AND + exclusions).
    pub fn matches_text(&self, text: &str) -> bool {
        let hay = text.to_lowercase();
        self.exclude
            .iter()
            .all(|e| !hay.contains(&e.to_lowercase()))
            && self.terms.iter().all(|t| hay.contains(&t.to_lowercase()))
    }

    /// The byte offset of the first hit term — the excerpt takes its window around it.
    pub fn first_hit(&self, text: &str) -> Option<usize> {
        let hay = text.to_lowercase();
        self.terms
            .iter()
            .filter_map(|t| hay.find(&t.to_lowercase()))
            .min()
    }
}

/// One token cut out of the input, carrying whether it was wrapped in quotes.
///
/// The quote state has to travel with it: quoting is the only way into phrase search, and
/// downstream decides from it **whether this token may be split again** (see [`Query::parse`]).
/// Drop that bit and `"缓存 配置"` is taken as an ordinary term and split a second time, and the
/// phrase semantics are gone.
struct Token {
    body: String,
    quoted: bool,
}

/// Split into tokens: whitespace separates, but whitespace inside quotes is kept.
///
/// `"缓存 配置"` is one term and not two — this is the only way into phrase search, and without
/// it "both terms appear in the same session" and "these two terms appear next to each other"
/// cannot be told apart.
///
/// This layer **splits on whitespace only**. Further splitting of Chinese happens in
/// [`Query::parse`], which is where it is known whether a token is a qualifier, an excluded
/// term or an ordinary term — what may be split differs across the three.
fn tokenize(input: &str) -> Vec<Token> {
    let mut out: Vec<Token> = vec![];
    let mut cur = String::new();
    // Whether a quote appeared anywhere in this token's lifetime. "appeared" rather than
    // "currently inside quotes", because the quote in `-"a b"` starts after the minus sign.
    let mut seen_quote = false;
    let mut quoted = false;
    for c in input.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                seen_quote = true;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(Token {
                        body: std::mem::take(&mut cur),
                        quoted: seen_quote,
                    });
                }
                seen_quote = false;
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(Token {
            body: cur,
            quoted: seen_quote,
        });
    }
    out
}

/// `key:value` split.
///
/// Only **known** qualifier prefixes are recognized, or `http://x` becomes an `http:`
/// qualifier. That is also why the key must be pure ASCII letters: neither a fullwidth colon
/// in Chinese nor a URL may trigger it.
/// Does this token **look** like a qualifier (in the case where the key is unrecognized)?
///
/// The test is the shape plus a table of URL exceptions. What has to be kept out are ordinary
/// terms that carry a colon of their own, like `http://x`: their value starts with `//`, or
/// their key is a widely known scheme.
///
/// The key-length cap of 12 is empirical: real qualifier keys are short (the longest,
/// `category`, is eight characters), while the first half of a colon-bearing natural-language
/// fragment (`结论:这样不行`) is usually longer and usually not pure ASCII letters.
fn looks_like_qualifier(tok: &str) -> bool {
    const SCHEMES: &[&str] = &[
        "http", "https", "ftp", "file", "git", "ssh", "mailto", "data",
    ];
    let Some((key, value)) = tok.split_once(':') else {
        return false;
    };
    if key.is_empty() || value.is_empty() || value.starts_with("//") {
        return false;
    }
    if key.len() > 12 || !key.chars().all(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    !SCHEMES.contains(&key.to_lowercase().as_str())
}

fn split_qualifier(tok: &str) -> Option<(String, String)> {
    let (key, value) = tok.split_once(':')?;
    if key.is_empty() || value.is_empty() {
        return None;
    }
    if !key.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    const KEYS: &[&str] = &[
        "in", "owner", "user", "org", "agent", "repo", "runtime", "category", "tool", "path",
        "state", "turns", "is",
    ];
    let lower = key.to_lowercase();
    if !KEYS.contains(&lower.as_str()) {
        // An unrecognized key: the whole token is an ordinary term. `http://x` takes this path.
        return None;
    }
    Some((lower, value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Chinese literals below stay in the original language (AGENTS.md exception iii): they
    // are the fixtures that exercise substring matching with no tokenizer, phrase quoting, the
    // do-not-split rule for excluded terms, the all-stopword path and the fullwidth colon. An
    // ASCII substitute makes every one of them vacuous.

    #[test]
    fn plain_words_are_anded() {
        let q = Query::parse("缓存 配置");
        assert_eq!(q.terms, vec!["缓存", "配置"]);
        assert!(
            q.matches_text("先配置再看缓存"),
            "both terms present is a hit; order does not matter"
        );
        assert!(
            !q.matches_text("只有缓存"),
            "a missing term is not a hit — terms are ANDed"
        );
    }

    #[test]
    fn quotes_make_a_phrase() {
        let q = Query::parse("\"缓存 配置\"");
        assert_eq!(q.terms, vec!["缓存 配置"], "quoted whitespace is kept");
        assert!(q.matches_text("这里有 缓存 配置 一段"));
        assert!(
            !q.matches_text("配置 缓存"),
            "a phrase is contiguous; reversed does not count"
        );
    }

    #[test]
    fn minus_excludes() {
        let q = Query::parse("缓存 -redis");
        assert_eq!(q.terms, vec!["缓存"]);
        assert_eq!(q.exclude, vec!["redis"]);
        assert!(q.matches_text("本地缓存"));
        assert!(!q.matches_text("redis 缓存"));
    }

    #[test]
    fn a_lone_dash_is_a_word_not_a_negation() {
        let q = Query::parse("-");
        assert_eq!(q.terms, vec!["-"]);
        assert!(q.exclude.is_empty());
    }

    #[test]
    fn substring_matching_is_what_chinese_needs() {
        // Chinese has no word boundaries: splitting on whitespace makes this query find
        // nothing.
        let q = Query::parse("配置缓存");
        assert!(
            q.matches_text("我在配置缓存策略"),
            "substring matching is what Chinese requires"
        );
    }

    /// **A Chinese query string is itself split.**
    ///
    /// The substring-matching argument secures "a term is a substring of the text"; it never
    /// addresses whether the query string gets split. Without term splitting all four of those
    /// queries return nothing (`docs/search-eval.md`, change 1).
    #[test]
    fn a_chinese_question_is_split_into_searchable_terms() {
        let q = Query::parse("stage1的lr是多少");
        assert!(
            q.terms.contains(&"stage1".to_string()),
            "the ASCII identifier survives splitting; got {:?}",
            q.terms
        );
        assert!(q.terms.contains(&"lr".to_string()));
        // This text is the form the corpus really takes. Without term splitting nothing hits.
        assert!(
            q.matches_text("stage1 训练配置：blr 0.001，实际峰值 lr 2e-3"),
            "term splitting must make this a hit; got {:?}",
            q.terms
        );
    }

    /// A spaced query and an unspaced one hit the same content.
    #[test]
    fn spacing_no_longer_changes_the_result() {
        let spaced = Query::parse("stage1 lr");
        let dense = Query::parse("stage1的lr是多少");
        let text = "stage1 的 lr 设成了 2e-3";
        assert_eq!(
            spaced.matches_text(text),
            dense.matches_text(text),
            "spacing must not change what hits"
        );
    }

    /// **A quoted phrase must not be split.**
    ///
    /// Splitting downgrades "appearing next to each other" into "both appearing", and that
    /// adjacency is the only thing phrase search provides.
    #[test]
    fn a_quoted_phrase_survives_tokenizing() {
        let q = Query::parse("\"配置缓存\"");
        assert_eq!(q.terms, vec!["配置缓存"], "a phrase is kept unchanged");
        assert!(q.matches_text("我在配置缓存策略"));
        assert!(
            !q.matches_text("缓存的配置"),
            "reversed order must not count as a hit"
        );
    }

    /// **An excluded term must not be split.**
    ///
    /// `exclude` means "drop it as soon as any one hits", so splitting narrows the result set
    /// — and the safety net for term splitting is that it can only grow the hit set.
    #[test]
    fn an_excluded_phrase_is_not_split() {
        let q = Query::parse("lr -配置缓存");
        assert_eq!(q.exclude, vec!["配置缓存"]);
        // A session that only mentions `缓存` must not be excluded.
        assert!(q.matches_text("lr 和缓存没关系"));
        assert!(!q.matches_text("lr 在配置缓存里"));
    }

    /// When the whole string is stopwords it is kept unchanged, not turned into "no terms".
    ///
    /// `has_terms()` has to tell "searched and found nothing" apart from "gave no term at all";
    /// they are different states to the caller (the latter lists everything).
    #[test]
    fn an_all_stopword_query_still_has_a_term() {
        let q = Query::parse("是多少");
        assert!(q.has_terms(), "must not degenerate into an empty query");
    }

    #[test]
    fn case_is_ignored_for_ascii() {
        let q = Query::parse("Cache");
        assert!(q.matches_text("the CACHE layer"));
    }

    #[test]
    fn event_scope_is_the_dimension_github_does_not_have() {
        let q = Query::parse("超时 in:prompt");
        assert_eq!(q.scopes, vec![EventScope::Prompt]);
        assert!(q.allows(EventScope::Prompt));
        assert!(
            !q.allows(EventScope::Reply),
            "an explicit in: searches only that scope"
        );
        assert_eq!(q.terms, vec!["超时"], "a qualifier is not a term");
    }

    #[test]
    fn no_scope_allows_everything_including_secondhand_summaries() {
        let q = Query::parse("超时");
        // A summary is not excluded by default — sometimes the original has been compacted
        // away and it is the only clue left. But the result must be labeled secondhand, which
        // is EventScope::is_secondhand's job.
        for s in EventScope::ALL {
            assert!(q.allows(s), "{s:?} is allowed by default");
        }
        assert!(EventScope::Summary.is_secondhand());
        assert!(!EventScope::Prompt.is_secondhand());
    }

    #[test]
    fn multiple_scopes_accumulate_without_duplicates() {
        let q = Query::parse("x in:tool in:edit in:tool");
        assert_eq!(q.scopes, vec![EventScope::Tool, EventScope::Edit]);
    }

    /// **`in:output` and `in:tool` are two different questions.**
    ///
    /// The former is "has a command printed this value", the latter "has anyone run this
    /// command". Merging them into one scope drowns `in:tool` in log noise, and the whole
    /// value of that qualifier is finding evidence that something really ran.
    #[test]
    fn tool_output_is_its_own_scope() {
        let q = Query::parse("lr in:output");
        assert_eq!(q.scopes, vec![EventScope::Output]);
        assert!(
            !q.allows(EventScope::Tool),
            "output and call are not the same scope"
        );
        assert_eq!(EventScope::Output.as_str(), "output");
    }

    /// Every scope value the syntax recognizes must actually find something.
    ///
    /// Put the other way: a value in `ALL` that the backend cannot map produces "the syntax
    /// accepts this word but it never returns anything" — the hardest class of failure to
    /// track down. This pins the completeness of the value table on the CLI side.
    #[test]
    fn every_scope_round_trips_through_its_name() {
        for s in EventScope::ALL {
            assert_eq!(
                EventScope::parse(s.as_str()),
                Some(s),
                "{s:?} round-trips through its name"
            );
        }
    }

    /// Tool output is **not** secondhand.
    ///
    /// Secondhand means "words the compactor paraphrased". Tool output is what a machine
    /// printed verbatim — harder evidence than anything a person said.
    #[test]
    fn tool_output_is_not_secondhand() {
        assert!(!EventScope::Output.is_secondhand());
    }

    #[test]
    fn qualifiers_land_in_their_fields() {
        let q = Query::parse("owner:alice agent:alice/photo runtime:codex category:research");
        assert_eq!(q.owner.as_deref(), Some("alice"));
        assert_eq!(q.agent.as_deref(), Some("alice/photo"));
        assert_eq!(q.runtime.as_deref(), Some("codex"));
        assert_eq!(q.category.as_deref(), Some("research"));
        assert!(q.terms.is_empty(), "a qualifier-only query has no terms");
        assert!(
            !q.is_empty(),
            "a qualifier-only query is not empty — listing all of alice's is a legitimate intent"
        );
    }

    #[test]
    fn is_public_and_negated_public_are_the_same_field() {
        assert_eq!(
            Query::parse("is:public").visibility.as_deref(),
            Some("public")
        );
        assert_eq!(
            Query::parse("is:private").visibility.as_deref(),
            Some("private")
        );
        // `-is:public` and `is:private` say the same thing and land in one field.
        assert_eq!(
            Query::parse("-is:public").visibility.as_deref(),
            Some("private")
        );
        assert_eq!(
            Query::parse("-is:private").visibility.as_deref(),
            Some("public")
        );
    }

    #[test]
    fn is_fork_can_be_negated() {
        assert_eq!(Query::parse("is:fork").fork, Some(true));
        assert_eq!(Query::parse("-is:fork").fork, Some(false));
        assert_eq!(Query::parse("x").fork, None, "unwritten means unfiltered");
    }

    #[test]
    fn unsupported_negated_qualifiers_are_reported_not_inverted() {
        for input in [
            "-owner:alice",
            "-agent:alice/photo",
            "-runtime:codex",
            "-category:research",
            "-tool:grep",
            "-path:src/lib.rs",
            "-state:open",
            "-turns:>20",
            "-in:tool",
            "-is:open",
        ] {
            let q = Query::parse(input);
            assert_eq!(
                q.unknown,
                vec![input],
                "{input} must not silently become a positive filter"
            );
            assert!(q.owner.is_none());
            assert!(q.agent.is_none());
            assert!(q.runtime.is_none());
            assert!(q.category.is_none());
            assert!(q.tool.is_none());
            assert!(q.path.is_none());
            assert!(q.state.is_none());
            assert!(q.turns.is_none());
            assert!(q.scopes.is_empty());
        }
    }

    #[test]
    fn numeric_comparison_is_off_by_one_safe() {
        // `>20` is "more than 20", that is at least 21 — off-by-one is easiest here.
        assert_eq!(NumFilter::parse(">20"), Some(NumFilter::AtLeast(21)));
        assert_eq!(NumFilter::parse(">=20"), Some(NumFilter::AtLeast(20)));
        assert_eq!(NumFilter::parse("<5"), Some(NumFilter::AtMost(4)));
        assert_eq!(NumFilter::parse("<=5"), Some(NumFilter::AtMost(5)));
        assert_eq!(NumFilter::parse("7"), Some(NumFilter::Exactly(7)));
        // No natural number satisfies `<0`, and it must not panic.
        assert_eq!(NumFilter::parse("<0"), None);
        // No natural number satisfies `>usize::MAX` either; it must not panic in debug or
        // wrap around to `AtLeast(0)` in release.
        assert_eq!(
            NumFilter::parse(&format!(">{}", usize::MAX)),
            None,
            "a strict greater-than past the usize cap must be rejected"
        );

        assert!(NumFilter::AtLeast(21).matches(21));
        assert!(!NumFilter::AtLeast(21).matches(20));
        assert!(NumFilter::AtMost(4).matches(4));
        assert!(!NumFilter::AtMost(4).matches(5));
    }

    #[test]
    fn turns_filter_parses() {
        assert_eq!(
            Query::parse("x turns:>20").turns,
            Some(NumFilter::AtLeast(21))
        );
        // An unrecognized value must not silently become an ordinary term.
        let q = Query::parse("x turns:abc");
        assert!(q.turns.is_none());
        assert_eq!(q.unknown, vec!["turns:abc"]);
    }

    #[test]
    fn unknown_qualifiers_are_reported_not_silently_searched() {
        // "there are results but they are wrong" is the hardest failure to track down: a
        // qualifier searched as an ordinary term returns a pile of irrelevant results while
        // the user believes the filter took effect.
        let q = Query::parse("缓存 in:nonsense");
        assert_eq!(q.unknown, vec!["in:nonsense"]);
        assert_eq!(
            q.terms,
            vec!["缓存"],
            "an unknown qualifier does not enter the term list"
        );
    }

    /// The two kinds of "unrecognized" are handled differently; this pins the difference.
    ///
    /// - Known key, unknown value (`in:nonsense`): the filter **cannot run**, so it stays out
    ///   of the term list — searching `nonsense` as a term returns only noise.
    /// - The key itself is mistyped (`runtim:codex`): the intent is still usable — the user
    ///   wants codex things. So it is reported in `unknown` and **still searched as an
    ///   ordinary term**, so they get results at all instead of an empty page and a line
    ///   saying the qualifier is unknown.
    #[test]
    fn misspelled_qualifier_key_is_reported_but_still_searched() {
        let q = Query::parse("缓存 runtim:codex");
        assert_eq!(
            q.unknown,
            vec!["runtim:codex"],
            "a mistyped key is reported"
        );
        assert_eq!(
            q.terms,
            vec!["缓存", "runtim:codex"],
            "a mistyped key is also searched as a term — the report must not fail the query"
        );
        assert!(q.runtime.is_none(), "runtim is not guessed into runtime");
    }

    /// Natural language carrying a colon is not a mistyped qualifier.
    #[test]
    fn colon_in_prose_is_not_a_misspelled_qualifier() {
        // The key is too long and not pure ASCII letters.
        let q = Query::parse("结论:这样不行");
        assert!(q.unknown.is_empty(), "got {:?}", q.unknown);
        assert_eq!(q.terms, vec!["结论:这样不行"]);
    }

    #[test]
    fn urls_are_words_not_qualifiers() {
        let q = Query::parse("https://example.com/x");
        assert_eq!(q.terms, vec!["https://example.com/x"]);
        assert!(
            q.unknown.is_empty(),
            "an unknown key prefix is not an unknown qualifier"
        );
    }

    #[test]
    fn full_width_colon_is_not_a_qualifier() {
        // The fullwidth colon from a Chinese IME is common; it must not trigger qualifier
        // parsing.
        let q = Query::parse("配置：缓存");
        assert_eq!(q.terms, vec!["配置：缓存"]);
    }

    #[test]
    fn empty_query_is_empty() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
        assert!(!Query::parse("x").is_empty());
    }

    #[test]
    fn first_hit_finds_the_earliest_term() {
        let q = Query::parse("配置 缓存");
        let text = "先说缓存后说配置";
        let pos = q.first_hit(text).unwrap();
        assert_eq!(
            pos,
            text.find("缓存").unwrap(),
            "the earliest occurring term wins"
        );
    }

    #[test]
    fn types_round_trip_through_their_wire_names() {
        for t in SearchType::ALL {
            assert_eq!(SearchType::parse(t.as_str()), Some(t));
        }
        // The singular is a common typo; accept it rather than answer 400.
        assert_eq!(SearchType::parse("session"), Some(SearchType::Sessions));
        assert_eq!(SearchType::parse("org"), Some(SearchType::People));
        assert_eq!(SearchType::parse("no-such-type"), None);
        assert_eq!(SearchType::default(), SearchType::Sessions);
    }

    #[test]
    fn scopes_and_sorts_round_trip() {
        for s in EventScope::ALL {
            assert_eq!(EventScope::parse(s.as_str()), Some(s));
        }
        for s in [Sort::Best, Sort::Recent, Sort::Turns] {
            assert_eq!(Sort::parse(s.as_str()), Some(s));
        }
        assert_eq!(Sort::default(), Sort::Best);
    }
}
