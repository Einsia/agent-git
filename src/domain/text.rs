//! Text splitting and similarity. **The only implementation of every split on the search path.**
//!
//! # Why character bigrams and not word segmentation
//!
//! The corpus mixes Chinese and English, and Chinese has no word boundaries. Splitting on
//! whitespace lands `缓存配置` and `配置缓存策略` in two entirely disjoint term sets — and they
//! say the same thing. It is also why search itself picks substring matching over FTS5/tsvector
//! (see the passage at [`super::query::Query::terms`]).
//!
//! Bigrams cut `构建缓存` into `构建`/`建缓`/`缓存`: no dictionary, one logic for both scripts,
//! and a change of word order still shares most of the bigrams. The cost is the occasional
//! cross-word bigram (`建缓`) carrying a little noise, but used for **similarity** that noise is
//! borne by both sides at once, and it weighs far less than a segmentation error.
//!
//! The alternative is a real segmenter like jieba: better results, but it introduces a
//! dictionary dependency, and a segmentation error shows up as "nothing found / not grouped
//! together" — the hardest class of failure to track down.
//!
//! # Why this lives in agent-git and not the backend
//!
//! Four places on the search path split text: query-side term splitting ([`super::query`]), the
//! inverted index on write and on query, similarity for cross-session grouping, and window
//! scoring in the answer layer's coarse filter. **All four must follow the same rules** — two
//! splits mean "the sessions grouped together" and "the sessions adjacent in the index" are not
//! the same batch, and that inconsistency never errors; the results just look inexplicable.
//!
//! The dependency runs one way (backend → agent-git), and the query syntax lives in agent-git,
//! so `query.rs` cannot reference a splitter on the backend side. For query-side splitting and
//! index-side splitting to share one implementation, the split must sit on **the depended-on
//! side**.
//!
//! The backend's `features/search/text.rs` is a re-export of this module, not a second
//! implementation.

use std::collections::HashSet;

/// Normalize: lowercase, and collapse punctuation and whitespace into a single space.
///
/// Chinese stopwords are not dropped: words like `怎么` and `如何` are the very substance of a
/// question, and dropping them leaves two unrelated questions with so few characters left that
/// they are misjudged as similar.
///
/// (Query-side splitting **does** drop stopwords, but that is a different job — see
/// [`query_terms`]. This one serves similarity and the index, and both need the full language
/// signal.)
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_space = false;
        } else if !last_space {
            // Punctuation, whitespace and fullwidth symbols are all separators.
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Split into a **sequence** of bigrams (duplicates kept).
///
/// An ASCII word is taken whole (`cache` is one token, not `ca`/`ac`/`ch`... — English has word
/// boundaries, and cutting it up only makes noise); a run of non-ASCII is cut into character
/// bigrams.
///
/// A single-character run (Chinese `锁`, English `a`) is kept as a unigram; otherwise a
/// one-character query splits into an empty set.
///
/// # Why the sequence is the primitive and the set is its view
///
/// Similarity wants a set (duplicates mean nothing); BM25 term frequency wants a sequence. The
/// two **must share one split** — written separately, "the terms in the index" and "the terms
/// compared when grouping" are not the same batch, and that inconsistency never errors; the
/// results just look inexplicable. So only this function splits, and [`bigrams`] is its
/// deduplicated view.
pub fn bigrams_seq(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in normalize(s).split_whitespace() {
        // A token may mix identifiers and CJK without whitespace (`stage1配置`).
        // Split it into script runs first; otherwise the non-ASCII path turns the
        // whole token into cross-script bigrams (`1配`) and loses the ASCII
        // identifier that the query side keeps intact.
        for run in script_runs(word) {
            let chars: Vec<char> = run.chars().collect();
            if run.is_ascii() || chars.len() == 1 {
                out.push(run);
                continue;
            }
            for w in chars.windows(2) {
                out.push(w.iter().collect());
            }
        }
    }
    out
}

/// Split into a set of bigrams. The deduplicated view of [`bigrams_seq`].
pub fn bigrams(s: &str) -> HashSet<String> {
    bigrams_seq(s).into_iter().collect()
}

/// Jaccard similarity: intersection / union. Two empty sets score 0, not 1 — nothing to compare
/// is not the same as identical.
pub fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        return 0.0;
    }
    inter as f64 / union as f64
}

/// The Chinese fragments in a query string that **carry syntax only, no search value**.
///
/// # Why the query side drops stopwords and similarity does not
///
/// The two fail in opposite directions. On the similarity side, dropping `怎么`/`如何` leaves
/// two unrelated questions with so few characters that they are misjudged as similar (so
/// [`normalize`] keeps them). On the query side, keeping them lets the AND strangle the results
/// — `stage1的lr是多少` splits out `多少`, the AND then demands the corpus contain `多少` too,
/// and nobody writing a config talks that way.
///
/// Only the **scaffolding of a question** belongs here: interrogatives, modal particles,
/// structural particles. **No content words** — `收敛`, `落盘` and `同步` are exactly what the
/// user is looking for.
///
/// The single characters `的` / `了` / `是` are in as well: in an assignment sentence they are
/// operators (`lr 是 2e-3`), as search terms they discriminate nothing, and the AND requires
/// every term to match.
// The entries stay in Chinese: this is a lexical table the search index tokenizes against.
const STOPWORDS: &[&str] = &[
    // interrogatives
    "多少",
    "为什么",
    "什么",
    "怎么",
    "怎样",
    "如何",
    "哪个",
    "哪些",
    "是不是",
    "有没有",
    // modal / structural particles
    "了吗",
    "吗",
    "呢",
    "吧",
    "的",
    "了",
    "是",
    "在",
    "和",
    "与",
    "把",
    "给",
    "有",
    "会",
];

/// Split one query token into search terms.
///
/// # Why this function exists
///
/// The terms in [`super::query::Query::terms`] are **substrings**, ANDed together. That
/// semantics holds for "a term is a substring of the text" (`评审` matches `代码评审`), but it
/// never handled whether **the query string itself** should be split. So a whole Chinese string
/// becomes one term. In practice:
///
/// ```text
/// stage1的lr是多少    → 0 results       stage1 lr → 7 results
/// checkpoint落盘了吗  → 0 results
/// swanlab同步         → 0 results
/// ```
///
/// # How it splits
///
/// - **ASCII runs are taken whole**: `stage1` / `lr` / `w4a4` are identifiers, and cutting them
///   up only makes noise
/// - **CJK runs are cut to at most two characters**: this is required, not an optimization —
///   `收敛不好` **never appears** in the real corpus while `收敛` does. Not cutting means not
///   finding it
/// - **Stopwords are dropped**: see [`STOPWORDS`]
///
/// # The safety net: it can only widen the match set
///
/// Every term it produces is a **substring** of the original token, and an AND over substrings
/// is easier to satisfy than the original string itself: any text containing `stage1的lr`
/// necessarily contains both `stage1` and `lr`. So this function **cannot** turn a query that
/// found something into one that finds nothing. A test pins that property.
///
/// The other way round it does return more (`stage1` and `lr` occurring at different points of
/// the same session counts as a match). That is deliberate: an empty result set is worth nothing
/// to the user, and the ranking layer is what lifts the genuinely relevant to the top.
pub fn query_terms(token: &str) -> Vec<String> {
    // **A token carrying a colon is never split.**
    //
    // A colon means the user wrote something structured rather than natural language: a URL
    // (`https://example.com/x`), Chinese with a colon (`结论:这样不行`), or a **mistyped
    // qualifier** (`runtim:codex`).
    //
    // The last one is a hard constraint: a mistyped qualifier must be "reported in `unknown` and
    // searched as an ordinary term at the same time", and the string reported and the term
    // searched must be one and the same — split, `unknown` holds `runtim:codex` while `terms`
    // holds `runtim` / `codex`, and the hint the user sees does not match what was searched.
    //
    // It also preserves searching for **an assignment form**: `lr:` as a query, searched
    // literally, is a legitimate use.
    //
    // The fullwidth colon is data, not prose: a user typing `结论：这样不行` must take the same
    // do-not-split path as the ASCII form, so this character class matching another script stays.
    if token.contains(':') || token.contains('：') {
        return Vec::new();
    }

    // A quoted phrase is not split: that is the only entry point for phrase search, where the
    // user explicitly asked for "occurs contiguously". The decision sits with the caller (it
    // knows whether quotes appeared); this handles bare tokens only.
    let mut out: Vec<String> = Vec::new();
    for run in script_runs(token) {
        if run.is_ascii() {
            let w = run.to_lowercase();
            if !w.is_empty() && !out.contains(&w) {
                out.push(w);
            }
            continue;
        }
        for piece in cjk_pieces(&run) {
            if !out.contains(&piece) {
                out.push(piece);
            }
        }
    }
    out
}

/// Cut a token into alternating ASCII / non-ASCII runs.
///
/// `stage1的lr是多少` → `["stage1", "的", "lr", "是多少"]`
///
/// Digits and letters are not separated (`w4a4` is one identifier); punctuation is dropped as a
/// separator — in a query it carries no search intent (`lr?` and `lr` mean the same thing).
fn script_runs(token: &str) -> Vec<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_ascii: Option<bool> = None;
    for c in token.chars() {
        // Punctuation and whitespace end the current run. ASCII `_` / `-` / `.` stay — they
        // appear inside identifiers (`min_lr`, `w4a4-config`, `v0.9`).
        let keep = c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/');
        if !keep {
            if !cur.is_empty() {
                runs.push(std::mem::take(&mut cur));
                cur_ascii = None;
            }
            continue;
        }
        let is_ascii = c.is_ascii();
        if cur_ascii != Some(is_ascii) && !cur.is_empty() {
            runs.push(std::mem::take(&mut cur));
        }
        cur_ascii = Some(is_ascii);
        cur.push(c);
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// Cut a run of CJK text into search terms of at most two characters, dropping stopwords.
///
/// Split on stopwords first, then cut what is left into pieces of at most two characters:
///
/// ```text
/// 是多少        → []              all stopwords
/// 落盘了吗      → ["落盘"]
/// 收敛不好      → ["收敛", "不好"]
/// 同步失败      → ["同步", "失败"]
/// ```
///
/// # Why not overlapping bigrams
///
/// [`bigrams_seq`] gives overlapping windows (`收敛不好` → `收敛`/`敛不`/`不好`), which is right
/// for **recall** (provably misses nothing). But the terms produced here go into an AND, and
/// `敛不` is a cross-word seam; requiring the corpus to contain it adds a far stricter condition
/// out of nowhere. So this cuts by a **stride** of two characters, without overlap.
///
/// The index side still recalls with `bigrams_seq`'s overlapping windows — the two follow
/// different rules **deliberately**, because one serves the AND (which must be loose) and the
/// other serves recall (which must miss nothing). What they share is the [`normalize`] layer.
fn cjk_pieces(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // Does a stopword start here? Longest first — `是不是` must beat `是`.
        let mut skipped = false;
        for sw in STOPWORDS {
            let sw_chars: Vec<char> = sw.chars().collect();
            if chars[i..].starts_with(&sw_chars[..]) {
                i += sw_chars.len();
                skipped = true;
                break;
            }
        }
        if skipped {
            continue;
        }
        // Take two characters, but never cross into a stopword — `收敛的` yields `收敛`, not
        // `收敛的`.
        let mut piece = String::new();
        while piece.chars().count() < 2 && i < chars.len() {
            if STOPWORDS
                .iter()
                .any(|sw| chars[i..].starts_with(&sw.chars().collect::<Vec<_>>()[..]))
            {
                break;
            }
            piece.push(chars[i]);
            i += 1;
        }
        if !piece.is_empty() {
            out.push(piece);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Chinese literals below stay in the original language: they are fixtures that exercise
    // CJK segmentation, and the corpus strings the queries are matched against.

    /// Reordering the words of a Chinese sentence still matches — the **entire reason** for
    /// choosing bigrams.
    ///
    /// 0.5 is the **ceiling** reordering can reach, not a coincidence: reordering keeps every
    /// content bigram (`缓存`/`配置`/`怎么`/`么改`), but each side coins the same number of new
    /// seam bigrams (`存配`/`置怎` vs `改配`/`置缓`), so the intersection is exactly half the
    /// union.
    ///
    /// This pins that ceiling, because the grouping threshold is set from it (see the backend's
    /// `routes::GROUP_THRESHOLD`): let the threshold rise above 0.5 and a reordered synonym
    /// never groups again.
    #[test]
    fn chinese_word_order_still_matches() {
        let a = bigrams("缓存配置怎么改");
        let b = bigrams("怎么改配置缓存");
        let s = similarity(&a, &b);
        assert!(
            s >= 0.5,
            "a reordered sentence must stay at or above 0.5; got {s}"
        );
    }

    /// An unrelated pair scores **far below** a same-topic pair — that gap is what makes
    /// grouping possible.
    #[test]
    fn the_noise_floor_is_far_below_the_signal() {
        let unrelated = similarity(
            &bigrams("monorepo 的构建缓存怎么配"),
            &bigrams("数据库连接池的大小怎么定"),
        );
        let prefix = similarity(
            &bigrams("构建缓存怎么配才不会每次全量重编"),
            &bigrams("构建缓存怎么配"),
        );
        assert!(
            unrelated < 0.1,
            "an unrelated pair must score near zero; got {unrelated}"
        );
        assert!(
            prefix >= 0.4,
            "a same-topic pair must score high; got {prefix}"
        );
        assert!(
            prefix > unrelated * 4.0,
            "signal must clear noise by a wide margin: {prefix} vs {unrelated}"
        );
    }

    #[test]
    fn ascii_words_are_kept_whole() {
        let g = bigrams("cache key");
        assert!(g.contains("cache") && g.contains("key"));
        assert!(
            !g.contains("ca"),
            "English has word boundaries; a word must not be cut up"
        );
    }

    #[test]
    fn mixed_ascii_and_cjk_runs_keep_identifiers_and_cjk_bigrams_separate() {
        let g = bigrams("stage1配置");
        assert!(
            g.contains("stage1"),
            "an ASCII identifier must not be cut up: {g:?}"
        );
        assert!(
            g.contains("配置"),
            "a CJK run must still split into bigrams: {g:?}"
        );
        assert!(
            !g.contains("1配"),
            "a cross-script bigram must not be produced: {g:?}"
        );
    }

    #[test]
    fn a_single_character_survives() {
        assert_eq!(bigrams("锁").len(), 1);
        assert!(!bigrams("锁").is_empty());
    }

    #[test]
    fn punctuation_and_case_are_normalized() {
        assert_eq!(bigrams("Cache, key!"), bigrams("cache key"));
        assert_eq!(bigrams("构建缓存：怎么配"), bigrams("构建缓存 怎么配"));
    }

    #[test]
    fn empty_is_not_identical_to_empty() {
        assert_eq!(similarity(&bigrams(""), &bigrams("")), 0.0);
    }

    #[test]
    fn identical_text_is_one() {
        let a = bigrams("monorepo 的构建缓存怎么配才不会每次全量重编");
        assert_eq!(similarity(&a, &a.clone()), 1.0);
    }

    // ── query_terms ────────────────────────────────────────────────────────

    /// **The four queries that return nothing in practice.**
    ///
    /// These four come from `search-eval.md`, each one run against the real corpus — not
    /// constructed examples.
    #[test]
    fn the_four_queries_that_returned_nothing_now_have_terms() {
        for q in [
            "stage1的lr是多少",
            "checkpoint落盘了吗",
            "swanlab同步",
            "为什么stage1收敛不好",
        ] {
            let terms = query_terms(q);
            assert!(
                !terms.is_empty(),
                "{q} must split into at least one search term"
            );
        }
    }

    /// Spaced and unspaced spellings must split into the same terms — the premise of "matching
    /// the same batch of sessions".
    #[test]
    fn spacing_does_not_change_the_terms() {
        let mut a = query_terms("stage1的lr是多少");
        // The hand-spaced spelling, one run at a time, then merged.
        let mut b: Vec<String> = ["stage1", "lr"]
            .iter()
            .flat_map(|t| query_terms(t))
            .collect();
        a.sort();
        b.sort();
        assert_eq!(a, b, "spacing must not change the terms");
    }

    /// ASCII identifiers are taken whole, never cut up.
    #[test]
    fn ascii_identifiers_stay_whole() {
        assert_eq!(query_terms("stage1"), vec!["stage1"]);
        assert_eq!(query_terms("w4a4"), vec!["w4a4"]);
        assert_eq!(query_terms("min_lr"), vec!["min_lr"]);
    }

    /// **Cutting CJK to at most two characters is required, not an optimization.**
    ///
    /// `收敛不好` never appears in the real corpus while `收敛` does. Not cutting means not
    /// finding it.
    #[test]
    fn cjk_runs_are_cut_to_two_chars() {
        let terms = query_terms("收敛不好");
        assert!(terms.contains(&"收敛".to_string()), "got {terms:?}");
        assert!(terms.iter().all(|t| t.chars().count() <= 2));
    }

    /// Stopwords never reach the AND — otherwise `多少` demands the corpus talk that way too.
    #[test]
    fn stopwords_do_not_reach_the_and() {
        let terms = query_terms("stage1的lr是多少");
        assert!(terms.contains(&"stage1".to_string()));
        assert!(terms.contains(&"lr".to_string()));
        for junk in ["多少", "的", "是"] {
            assert!(
                !terms.contains(&junk.to_string()),
                "{junk} must not become a search term"
            );
        }
    }

    /// An interrogative is dropped whole; the content words around it are kept.
    #[test]
    fn a_causal_query_keeps_only_its_content_words() {
        let terms = query_terms("为什么stage1收敛不好");
        assert!(terms.contains(&"stage1".to_string()));
        assert!(terms.contains(&"收敛".to_string()));
        assert!(!terms.iter().any(|t| t.contains("为什")));
    }

    /// **The safety net: splitting can only widen the match set, never lose what the unsplit
    /// query found.**
    ///
    /// The property: every term produced is a substring of the original token. Any text
    /// containing the original token contains every piece, so the AND still holds. This matters
    /// more than any specific example — it is the whole argument that splitting causes no
    /// regression.
    #[test]
    fn every_term_is_a_substring_of_the_original() {
        for q in [
            "stage1的lr是多少",
            "checkpoint落盘了吗",
            "为什么stage1收敛不好",
            "swanlab的mode是什么",
            "batch size是多少",
            "量化的teacher是ema的吗",
        ] {
            let lowered = q.to_lowercase();
            for t in query_terms(q) {
                assert!(
                    lowered.contains(&t),
                    "{t:?} must be a substring of {q:?}; splitting widened the meaning"
                );
            }
        }
    }

    /// Existing behavior must not break: `评审` still matches `代码评审`.
    ///
    /// This is the core example of the substring-matching argument, and splitting must not touch
    /// it.
    #[test]
    fn a_bare_word_is_unchanged() {
        assert_eq!(query_terms("评审"), vec!["评审"]);
    }

    /// A pure-ASCII query is untouched.
    #[test]
    fn ascii_only_queries_are_untouched() {
        assert_eq!(query_terms("lockfile"), vec!["lockfile"]);
    }

    /// A repeat must not produce a duplicate term (appearing twice in the AND means nothing).
    #[test]
    fn repeated_pieces_are_deduplicated() {
        let terms = query_terms("缓存缓存");
        assert_eq!(terms, vec!["缓存"]);
    }

    /// **A token carrying a colon is never split.**
    ///
    /// Three cases rest on this: URLs, Chinese with a colon, and a mistyped qualifier. The last
    /// is a hard constraint — a mistyped qualifier must be "reported in unknown and searched as
    /// an ordinary term", and once split, the string reported no longer matches the term
    /// actually searched.
    #[test]
    fn anything_with_a_colon_is_left_alone() {
        for token in [
            "https://example.com/x",
            "结论:这样不行",
            "配置：缓存",
            "runtim:codex",
        ] {
            assert!(
                query_terms(token).is_empty(),
                "{token:?} must not be split; the caller keeps it verbatim"
            );
        }
    }
}
