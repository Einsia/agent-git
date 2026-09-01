//! Path allowlist enforcement — the machine-side half of the trust boundary.
//!
//! A shared workspace is closer to "here is a shell account" than "here is a
//! document". The bound project directories *are* the allowlist; `agitd`
//! refuses any fs / exec target outside them. This check runs here and not (only)
//! at the hub because the hub is a relay: if it is ever wrong or compromised,
//! this is what caps the damage.
//!
//! Canonicalization matters: `..`, symlinks and case games are how allowlists
//! get bypassed. Targets are canonicalized when checked; roots are canonicalized
//! once when they enter [`CanonicalRoots`] and then compared by component. We
//! refuse paths that don't exist yet *unless* the caller opts in (binding a
//! project that is about to be created is legitimate; executing in one is not).

use std::path::{Component, Path, PathBuf};

/// Canonicalize; on failure fall back to lexical normalization so callers can still
/// reason about the intended location.
///
/// **This is a convenience, not a test.** The fallback to [`lexical`] folds `..` literally,
/// and under `vendor -> /` the literal fold of `vendor/../etc/passwd` lands inside the
/// workspace (see [`resolve_ancestor`]); it also gives the same answer for "this segment does
/// not exist yet" and for "what this segment is cannot be answered". Only [`is_within`] /
/// [`require_within`] answer "is it confined", and they go through that segment-by-segment
/// resolver.
pub fn canonical(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    lexical(p)
}

fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A path allowlist whose roots have already crossed the filesystem trust boundary.
///
/// Production roots enter through `project.bind`, register adoption, or the one-time legacy
/// mirror rebuild. Keeping the proof in the type prevents every approval-path check from
/// resolving the same roots again. Tests and legacy callers use [`Self::from_untrusted`], which
/// resolves symlinks once and drops missing paths instead of granting authority to a lexical
/// guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CanonicalRoots(Vec<PathBuf>);

impl CanonicalRoots {
    pub fn from_untrusted(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self(
            roots
                .into_iter()
                .filter_map(|root| std::fs::canonicalize(root).ok())
                .collect(),
        )
    }

    /// Build from paths that were returned by [`require_bindable_dir`].
    ///
    /// Kept crate-private so a wire path cannot label itself canonical. This deliberately does
    /// no filesystem work: the caller already paid for and validated that work at bind/adopt.
    pub(crate) fn from_verified(roots: Vec<PathBuf>) -> Self {
        Self(roots)
    }
}

impl std::ops::Deref for CanonicalRoots {
    type Target = [PathBuf];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> IntoIterator for &'a CanonicalRoots {
    type Item = &'a PathBuf;
    type IntoIter = std::slice::Iter<'a, PathBuf>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Is `target` inside any already-canonical root?
///
/// # If it cannot tell, the answer is "outside"
///
/// The test goes through [`resolve_existing_ancestor`], not [`canonical`]: that one falls
/// back to [`lexical`], which folds `..` literally, whenever `canonicalize` fails — and
/// **failure is not only "does not exist yet"**. A path that expands past the length the
/// kernel accepts (`ENAMETOOLONG`), a segment on the way that cannot be read (`EACCES`), a
/// cycle (`ELOOP`) — each of them makes the fallback hand back a path that looks inside the
/// root while the kernel opens somewhere else. So only that segment-by-segment resolver's
/// answer counts here: when it cannot tell, this gate says "outside".
pub fn is_within(target: &Path, roots: &CanonicalRoots) -> bool {
    resolve_existing_ancestor(target).is_some_and(|t| within(&t, roots))
}

fn within(resolved: &Path, roots: &CanonicalRoots) -> bool {
    roots
        .iter()
        .any(|r| resolved == r || resolved.starts_with(r))
}

/// Is `target` under the user's home directory? Used for `fs.readDirectory`
/// (the folder picker) which runs *before* any project is bound and is scoped
/// by ownership rather than allowlist.
pub fn is_under_home(target: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    is_within(target, &CanonicalRoots::from_untrusted([home]))
}

/// Directories that may never be bound.
///
/// This is not "protect the machine from its owner" — the owner can already `cd /` on their
/// own machine and start an agent there. It defends against **something else**: a compromised
/// hub talking the owner into one click that turns the whole filesystem into the allowlist.
/// Binding is the only action that widens the boundary, so the machine side pins down a few
/// roots that obviously must not be handed over whole.
const NEVER_BIND: &[&str] = &[
    "/", "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/dev", "/proc", "/sys",
    "/var/lib", "/var/run", "/run",
];

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("path {0} is outside the workspace's bound projects")]
    OutsideAllowlist(String),
    #[error("path {0} is outside your home directory")]
    OutsideHome(String),
    #[error("path {0} is not a directory")]
    NotADirectory(String),
    #[error("{0} is a system directory and cannot be bound as a project")]
    SystemDirectory(String),
    #[error("path {0} does not exist on this machine")]
    Missing(String),
}

/// [`is_within`], plus handing the caller back **the path the test looked at**.
///
/// Two things must be one answer: the path judged inside or outside, and the path the caller
/// then opens. Computed separately, the test looks at the resolved location while the caller
/// gets another function's fallback — and where this read lands is decided by the latter.
pub fn require_within(target: &Path, roots: &CanonicalRoots) -> Result<PathBuf, PolicyError> {
    match resolve_existing_ancestor(target) {
        Some(t) if within(&t, roots) => Ok(t),
        _ => Err(PolicyError::OutsideAllowlist(target.display().to_string())),
    }
}

/// Validate a directory that is about to be bound as a project.
///
/// **Deliberately different** from [`require_dir_under_home`] (the folder picker): a project
/// may live in `/srv`, `/opt` or `/workspace`, and pinning it to `$HOME` makes the feature
/// unusable for half the real repos. Three things are guarded here: it must really exist, it
/// must be a directory, and it must not be a system root.
///
/// "Who may bind" is judged by the hub (the owner only); "what may be bound" is judged here.
/// Each side holds half, because only the machine sees its own filesystem and only the hub
/// sees roles.
pub fn require_bindable_dir(target: &Path) -> Result<PathBuf, PolicyError> {
    // This path **becomes a root of the allowlist**, and every later `is_within` measures
    // against it — so it must be the one the kernel resolves to, never the lexical fold
    // [`canonical`] falls back to when resolution fails. The kernel cannot walk a folded
    // spelling at all, yet its `exists()` may well be true, because that is **another**
    // directory that really exists: the owner names one folder and a different one enters the
    // allowlist. Binding is not like writing a file: a write may create a tail that does not
    // exist yet, while a bind target must **already be there**. So what is wanted here is the
    // kernel's answer for the whole spelling, not "the ancestor that resolves" — the latter
    // appends a missing tail unchanged by design, so `/nonexistent/../Users` folds out a
    // really existing `/Users` to serve as a root.
    let Ok(c) = std::fs::canonicalize(target) else {
        return Err(PolicyError::Missing(target.display().to_string()));
    };
    if !c.is_dir() {
        return Err(PolicyError::NotADirectory(target.display().to_string()));
    }
    if is_never_bind(&c, target) {
        return Err(PolicyError::SystemDirectory(c.display().to_string()));
    }
    Ok(c)
}

/// Is a path one of the unbindable roots on the list?
///
/// # Why it compares **twice**
///
/// The list spells `/etc`, while `require_bindable_dir` compares the path after
/// `canonical()`. On macOS `/etc` is a symlink to `/private/etc`, so once resolved it is no
/// longer on the list — the same holds for `/var/lib` and `/var/run` (`/var` →
/// `/private/var`). Three guardrails fail silently across the whole platform, and the
/// direction of the failure is **allow**.
///
/// The fix is not adding literals like `/private/etc` to the list: that asks every new entry
/// to be accompanied by its real name on every platform, enforced by remembering — which is
/// exactly what a guardrail like this must not rest on. So each entry of the list is resolved
/// once before the comparison, and the comparison against the **raw** path is kept as well —
/// the `/etc` the user typed and its real name must both be stopped.
fn is_never_bind(canonical_target: &Path, raw_target: &Path) -> bool {
    NEVER_BIND.iter().any(|d| {
        let listed = Path::new(d);
        canonical_target == listed
            || raw_target == listed
            // Resolve the listed entry too: `/etc` is `/private/etc` on macOS.
            || listed
                .canonicalize()
                .is_ok_and(|resolved| canonical_target == resolved)
    })
}

/// Any one of these **outside** quotes means we do not know what this line ends up running.
///
/// Do not write a shell parser here: getting one right is harder than everything else in this
/// file together, and getting it wrong points at allow. `=` is deliberately absent — an
/// environment prefix (`LD_PRELOAD=... ls`) can only appear in the **head**, and the head has
/// its own character check; putting `=` in this table only kills `--type=rust` along the way.
const SHELL_META: &[char] = &[
    ';', '&', '|', '$', '`', '(', ')', '<', '>', '{', '}', '[', ']', '*', '?', '~', '!', '#', '\\',
    '\n', '\r', '\t',
];

/// The control surfaces in a workspace, where writing is the same as executing.
///
/// **Inside the workspace ≠ confined.** `.git/config` is inside the workspace, and agitd
/// **itself** runs git in that directory on every settlement (`settle_and_push` starts
/// `agit commit --from-hook`, and that child process asks `git remote get-url origin` and
/// `git status --porcelain`). Every one of those reads `.git/config` and **executes** the
/// command `core.fsmonitor` names; `core.hooksPath`, `.git/hooks/*`, `filter.*.clean`,
/// `diff.external` and `core.pager` are the same channel. `.claude/settings.json` is the same
/// on the harness side.
///
/// So for a **write** these directories are equivalent to "outside the allowlist": allow one
/// and there is no next approval, because the daemon itself runs that program, not the
/// agent.
#[rustfmt::skip]
const CONTROL_SURFACES: &[&str] = &[
    ".git", ".hg", ".svn", ".claude", ".codex", ".cursor", ".vscode", ".idea", ".github",
    ".gitlab", ".envrc", ".direnv", ".husky",
    // A build tool's config **is an execution channel too**, and it is read by exactly the
    // names `agit rc grant` is most likely to hand out: `.cargo/config.toml` can set
    // `target.*.runner` and `[alias]`, so one granted `cargo test` becomes any command;
    // `.npmrc` with `ignore-scripts=false` plus a preinstall, `.yarnrc.yml` plugins and a
    // gradle init script are the same.
    ".cargo", ".rustup", ".npmrc", ".yarnrc", ".yarnrc.yml", ".yarn",
    ".gradle", ".mvn", ".bundle", ".pre-commit-config.yaml",
];

/// Fold one name into "what the filesystem sees". **Every name comparison in this file goes
/// through here** — the forward table ([`is_control_surface_name`]) and the two reverse path
/// comparisons ([`resolves_under`]) must use the same answer, otherwise a name the forward
/// side stops goes unrecognized in reverse, and unrecognized points at allow.
///
/// # The question it answers, and which way it may be wrong
///
/// The question is "**would the kernel resolve these two names to the same file**". No answer
/// to it is exact for every filesystem (case, normalization, which code points are ignored —
/// every layer has its own table, and one path can cross mount points), so this function does
/// not chase exactness; it holds one rule: **the fold may only be coarser than the kernel,
/// never finer**. A notch coarser treats two names the kernel keeps apart as one, at the cost
/// of asking the owner once more; a notch finer treats two spellings the kernel calls one
/// file as two, at the cost of allowing one write a control surface reaches. Every place
/// below that cannot tell leans coarse.
///
/// # Why case is folded
///
/// APFS / HFS+ on macOS are case-insensitive by default, and so is NTFS on Windows. So
/// `.CLAUDE/settings.json` and `.claude/settings.json` are **one file** on disk, while a
/// byte-for-byte comparison lets the first through: `resolve_existing_ancestor` resolves only
/// down to the deepest ancestor that **already exists**, and the tail that does not exist yet
/// keeps the case the agent wrote. Once written, Claude Code reads it as `.claude/` all the
/// same, and that hook executes on the agent's next tool call — with no second approval.
///
/// (`.git/config` is stopped only because `.git` usually exists already and realpath
/// normalizes its case along the way. That is luck, not the test doing its work; the piece
/// `Write` creates has no such luck.)
///
/// # On a case-sensitive mount: fold anyway
///
/// Case sensitivity is a property of the **filesystem**, not of the operating system: a
/// case-sensitive APFS volume can be created on macOS, a mounted exFAT/NTFS or a
/// casefold-enabled directory on Linux is insensitive, and one path can cross mount points.
/// So "is this directory sensitive right now" can only be probed on the spot, and the probe's
/// answer changes with the next mount — resting a security test on an answer like that lets
/// the test drift with the mount table.
///
/// So it does not ask; it folds uniformly. The cost on a case-sensitive volume is that two
/// **genuinely different** files are treated as one, which asks the owner once more; the
/// other way round (not folding while the volume is insensitive) allows one write a control
/// surface really reaches. The two ways of being wrong do not cost the same, so the one that
/// asks once more is chosen.
///
/// # The fold is case folding, not the lowercase mapping
///
/// `str::to_lowercase` is Unicode's **lowercase mapping**, while an insensitive volume
/// compares by **case folding**, and the two are not the same thing: `ſ` (U+017F) folds to
/// `s` and the lowercase mapping leaves it as it is — so `.huſky` and `.husky` are one
/// directory on APFS, while per-character lowercasing makes this check see two. The same
/// class holds `ﬀ ﬁ ﬂ ﬃ ﬄ ﬅ ﬆ` (folding to `ff fi fl ffi ffl st st`), `ß`/`ẞ` (folding to
/// `ss`), and `ı` from the NTFS uppercase table (U+0131 uppercases to ASCII `I`, so `.gıt`
/// and `.git` are one name there). The names in [`CONTROL_SURFACES`] carry `s` (`.husky`),
/// `st` (`.rustup`) and `fi` (`.pre-commit-config.yaml`) — that is, with lowercasing alone
/// every one of them has a second spelling that walks under this check.
///
/// # An insensitive volume is normalization-insensitive too
///
/// HFS+ stores NFD, APFS compares normalization-insensitively, and git writes a symlink's
/// text as NFC — so the two sides of this check routinely hold two normalizations of one
/// name: `Café.json` (NFC) and `Cafe`+U+0301 (NFD) are **one file**. Folding case without
/// normalization allows that whole class of spelling, and hitting it takes nothing special
/// from the agent — the two sides' normalizations come from different places to begin with.
///
/// # The fold: exact within ASCII, coarse outside it
///
/// A non-ASCII code point can spell a name from the control-surface table, or the ASCII
/// spelling of another path segment, only when it folds to **pure ASCII**. Such code points
/// are countable, and [`FOLDS_TO_ASCII`] is the result of counting them. So:
///
/// 0. the code points a comparison treats as absent ([`is_ignorable_in_names`]) are dropped
///    first;
/// 1. the ones on the table expand into their ASCII spelling;
/// 2. ASCII folds to ASCII lowercase — every target filesystem agrees on ASCII;
/// 3. whatever non-ASCII is left is not asked about at all: **the whole alphanumeric run
///    holding it** is replaced by a placeholder.
///
/// Rule 3 solves normalization along with it, and needs no NFD table: precomposed `é` and
/// decomposed `e`+U+0301 differ only in "this run of letters holds a non-ASCII code point",
/// so `café.json` and `cafe`+U+0301+`.json` both fold to `<placeholder>.json`. Canonical
/// decomposition never crosses a non-alphanumeric ASCII character (`.`, `-`, `/`, `_`), so
/// splitting on those splits both sides in the same place.
///
/// The cost falls entirely on the coarse side: two **genuinely different** non-ASCII names
/// (`naïve` and `résumé`) fold to one. It does not widen the test onto ordinary writes — a
/// placeholder spells none of the ASCII names in [`CONTROL_SURFACES`], and `naïve/note.md`
/// stays operator-answerable; only when a control surface points at a non-ASCII name does
/// another non-ASCII name on the same level go to the owner with it.
///
/// # What it cannot fold
///
/// * **NTFS 8.3 short names**: `CLAUDE~1` is also `.claude`, and that ordinal is handed out
///   in creation order, cannot be computed, and is known only to the kernel — while the shape
///   that matters most to this check is exactly the piece that **does not exist yet**, which
///   the kernel cannot answer either. This is the only entry whose error points at allow, and
///   short-name generation is off by default on non-system volumes.
/// * **A future Unicode version adding a code point that folds to pure ASCII**: the table is
///   then missing one. What is on it now is the result of enumerating full case folding, the
///   NTFS uppercase table and canonical decomposition, each to the end.
/// * The exact relation between two non-ASCII names: see above — treated as one, in the
///   direction that asks once more.
fn fold_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    // Where the current alphanumeric run starts, and whether an unfoldable code point has
    // appeared in it.
    let mut word_at = 0usize;
    let mut opaque = false;
    for c in name.chars() {
        if is_ignorable_in_names(c) {
            continue;
        }
        if let Some(ascii) = FOLDS_TO_ASCII
            .iter()
            .find_map(|(from, to)| (*from == c).then_some(*to))
        {
            // Every expanded character still takes the ASCII path below: the `;` U+037E
            // folds to has to be a word boundary just like a literally written `;`,
            // otherwise the two spellings split in different places.
            for a in ascii.chars() {
                push_folded_ascii(a, &mut out, &mut word_at, &mut opaque);
            }
        } else if !c.is_ascii() {
            // Unfoldable: kept as it is, and the whole run is replaced when it ends.
            opaque = true;
            out.push(c);
        } else {
            push_folded_ascii(c.to_ascii_lowercase(), &mut out, &mut word_at, &mut opaque);
        }
    }
    collapse_opaque_word(&mut out, word_at, &mut opaque);
    // Win32 strips trailing `.` and spaces off every segment before it opens a path:
    // `.claude.` and `.claude` are one directory. Stripping only recognizes a few more names,
    // in the direction that asks once more.
    out.truncate(out.trim_end_matches(['.', ' ']).len());
    out
}

/// The non-ASCII code points that some target filesystem folds to **pure ASCII**.
///
/// This table is the whole premise of [`fold_name`]'s "exact within ASCII": miss one and some
/// name spells a control surface while this side fails to recognize it, and unrecognized
/// points at allow. The result of enumerating three sources, each to the end —
///
/// * the entries of **full case folding** (what APFS and casefold-enabled ext4 use) whose
///   result is pure ASCII: `ß`/`ẞ`→`ss`, `ſ`→`s`, U+212A KELVIN SIGN→`k`, `ﬀ ﬁ ﬂ ﬃ ﬄ ﬅ ﬆ`;
/// * the entries of the **NTFS uppercase table** that uppercase into ASCII: `ı`(U+0131)→`I`,
///   `ſ`→`S`; `İ`(U+0130) is folded to `i` along the way — neither NTFS nor APFS says so, and
///   one extra fold only asks once more;
/// * the entries of **canonical decomposition** (which a normalization-insensitive volume
///   follows) that decompose into a single ASCII code point: U+037E GREEK QUESTION MARK→`;`,
///   U+212A→`K`.
///
/// What they expand into is the **already folded** spelling (lowercase throughout), because
/// they are not folded again after leaving here.
const FOLDS_TO_ASCII: &[(char, &str)] = &[
    ('\u{00DF}', "ss"),
    ('\u{1E9E}', "ss"),
    ('\u{017F}', "s"),
    ('\u{0130}', "i"),
    ('\u{0131}', "i"),
    ('\u{037E}', ";"),
    ('\u{212A}', "k"),
    ('\u{FB00}', "ff"),
    ('\u{FB01}', "fi"),
    ('\u{FB02}', "fl"),
    ('\u{FB03}', "ffi"),
    ('\u{FB04}', "ffl"),
    ('\u{FB05}', "st"),
    ('\u{FB06}', "st"),
];

/// Code points a name comparison treats as absent.
///
/// The HFS+ folding table maps a batch of format characters to zero length, so
/// `.clau<ZWJ>de` and `.claude` are one directory on such a volume. What is taken here is a
/// **wider** set (zero-width, bidi controls, variation selectors), and wider recognizes a few
/// more names; which filesystem drops which is not something this has to tell apart.
fn is_ignorable_in_names(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{2064}'
        | '\u{206A}'..='\u{206F}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{E0000}'..='\u{E01EF}')
}

/// One already-folded ASCII character. Alphanumerics extend the current run, everything else
/// is a word boundary — those characters (`.`, `-`, `/`, `_`) are spelled identically on both
/// sides, and canonical decomposition never crosses them.
fn push_folded_ascii(c: char, out: &mut String, word_at: &mut usize, opaque: &mut bool) {
    if c.is_ascii_alphanumeric() {
        out.push(c);
        return;
    }
    collapse_opaque_word(out, *word_at, opaque);
    out.push(c);
    *word_at = out.len();
}

/// Replace the whole current alphanumeric run with a placeholder — as soon as one unfoldable
/// code point has appeared in it.
///
/// What is replaced is the **whole run**, not that one code point: precomposed `é` and
/// decomposed `e`+U+0301 differ in exactly whether that ASCII base character is still there,
/// and only swallowing it too folds both sides to the same thing.
fn collapse_opaque_word(out: &mut String, word_at: usize, opaque: &mut bool) {
    if std::mem::take(opaque) {
        out.truncate(word_at);
        out.push(char::REPLACEMENT_CHARACTER);
    }
}

fn is_control_surface(p: &Path) -> bool {
    p.components()
        .any(|c| is_control_surface_name(&c.as_os_str().to_string_lossy()))
        || spells_node_modules_bin(p)
}

/// Does this path hold `node_modules/.bin` — **two adjacent segments**? Whatever is put in
/// that directory is run as a tool by the next `npx` / `npm run`.
///
/// The component-by-component test above cannot see it: neither `node_modules` nor `.bin` is
/// in [`CONTROL_SURFACES`] on its own, and the danger exists only while the pair is adjacent.
///
/// # Three things to finish before comparing; missing any one points at allow
///
/// * **Fold `\` into `/`**: this test runs on Windows too, and there the harness reports
///   `node_modules\.bin\tsc`. The only cost of folding is that a Unix file genuinely named
///   `node_modules\.bin` also goes to the owner — the direction is tighter.
/// * **Fold per segment**, not the whole path as one name. The `.` and spaces [`fold_name`]
///   strips sit at the end of **the run it was handed**, while Win32 strips the end of
///   **every segment**: `node_modules./.bin/tsc` opens there as `node_modules/.bin/tsc`.
/// * **A segment the kernel does not have before it walks the path must not be here
///   either**: an empty segment (`//`), `.`, and the pair `seg/..` pops. None of them should
///   separate these two names, yet a literal substring search is split apart by every one of
///   them. `.` hides best: the kernel drops the whole segment, so `node_modules/./.bin/tsc`
///   spelled back is `node_modules//.bin/tsc` — and this write is the first stroke that
///   creates that chain of directories.
///
/// # "Is there a segment here" asks the raw text, not the folded name
///
/// [`fold_name`] answers **another** question: would the kernel resolve these two names to
/// the same file. For that it strips trailing `.` and spaces and drops ignorable code points,
/// so `...`, `" "`, `". ."` and `\u{200b}` all fold to nothing left — while every one of them
/// is an **ordinary directory name** the kernel walks into without a pause. Read a folded
/// emptiness as "there is no segment here" and the `..` behind it reaches past to pop
/// `node_modules` itself: `node_modules/.../../.bin/tsc` does not hold the pair once popped,
/// and holds a `..` between them if not popped, so neither reading recognizes it — while the
/// kernel opens it at `node_modules/.bin/tsc`. So which segments are dropped looks only at
/// the **raw text**, and the fold is used only to compare names.
///
/// # The half the raw text cannot answer: segments Win32 strips
///
/// Win32 strips trailing `.` and spaces off every segment before it opens a path, and a
/// segment left with nothing disappears — `node_modules/.../.bin/tsc` opens there as
/// `node_modules/.bin/tsc`, while Unix walks into `...` as an ordinary directory name. Which
/// platform this runs on is not something this test can ask (the spelling is a string the
/// harness reported), so both readings are compared, and recognizing more asks the owner once
/// more.
///
/// # The version before `..` is popped is compared as well
///
/// `node_modules/x/../.bin/tsc` is recognized only once popped; `node_modules/.bin/../x` is
/// recognized only while it is not — and that write still travels the path under `.bin`
/// (whether `x` is a directory, and whether the `.bin` segment really gets created, are not
/// decided by the spelling). Both versions are compared once, and recognizing more asks the
/// owner once more.
fn spells_node_modules_bin(p: &Path) -> bool {
    let s = p.to_string_lossy().replace('\\', "/");
    let raw: Vec<&str> = s.split('/').collect();
    [Vanish::Everywhere, Vanish::AlsoOnWin32]
        .into_iter()
        .any(|vanish| holds_the_bin_pair(&raw, vanish))
}

/// Does a segment disappear **before** the kernel walks to it? Two platforms, two answers, so
/// [`spells_node_modules_bin`] compares both readings.
#[derive(Clone, Copy)]
enum Vanish {
    /// An empty segment (`//`) and `.`: nothing walks to them anywhere.
    Everywhere,
    /// Plus the ones left with nothing after Win32 strips trailing `.` and spaces.
    AlsoOnWin32,
}

/// Among the segments this spelling keeps under the `vanish` reading, are `node_modules` and
/// `.bin` adjacent — compared once before `..` is popped and once after.
fn holds_the_bin_pair(raw: &[&str], vanish: Vanish) -> bool {
    let kept: Vec<String> = raw
        .iter()
        .filter(|seg| !vanishes_before_the_walk(seg, vanish))
        // `..` is the segment the next pass pops, not a name — folding it leaves nothing.
        .map(|seg| {
            if *seg == ".." {
                (*seg).to_string()
            } else {
                fold_name(seg)
            }
        })
        .collect();
    let spelled: Vec<&str> = kept.iter().map(String::as_str).collect();
    let mut popped: Vec<&str> = Vec::with_capacity(spelled.len());
    for seg in &spelled {
        if *seg == ".." {
            popped.pop();
        } else {
            popped.push(seg);
        }
    }
    has_the_bin_pair(&spelled) || has_the_bin_pair(&popped)
}

/// The segments the kernel does not have before it walks this path. **The test is the raw
/// text**: what the fold produces answers another question (see
/// [`spells_node_modules_bin`]), and counting the remaining segments by it erases ordinary
/// directory names whole.
fn vanishes_before_the_walk(seg: &str, vanish: Vanish) -> bool {
    if seg.is_empty() || seg == "." {
        return true;
    }
    // `..` does not disappear; it pops the segment before it — that pass is above.
    matches!(vanish, Vanish::AlsoOnWin32)
        && seg != ".."
        && seg.trim_end_matches(['.', ' ']).is_empty()
}

/// Are `node_modules` and `.bin` adjacent in this run of **folded** segment names?
fn has_the_bin_pair(segments: &[&str]) -> bool {
    segments
        .windows(2)
        .any(|pair| folded_is_node_modules(pair[0]) && pair[1].starts_with(".bin"))
}

/// Is a folded segment name `node_modules`?
///
/// The forward pair ([`has_the_bin_pair`]) and the reverse enumeration
/// ([`reachable_under_a_control_surface_name`]) must ask the **same** question: write an
/// equality test once on each side and some spelling is stopped forward and unrecognized in
/// reverse, and unrecognized points at allow.
///
/// A notch coarser than literal equality (**ending** in `node_modules` counts), and coarse
/// asks once more.
fn folded_is_node_modules(folded: &str) -> bool {
    folded.ends_with("node_modules")
}

/// Is **one** name a control surface? The same table as [`is_control_surface`], split out
/// because the reverse enumeration ([`reachable_under_a_control_surface_name`]) holds a single
/// entry name from `read_dir` rather than a whole path — both sides must ask the same
/// question, otherwise a name the forward side stops goes unrecognized in reverse, and
/// unrecognized points at allow.
///
/// Case goes through [`fold_name`], where the reason is written down.
fn is_control_surface_name(name: &str) -> bool {
    let folded = fold_name(name);
    CONTROL_SURFACES.iter().any(|s| folded == *s)
        // `.agit` / `.agit.toml` — our own config also takes effect the moment it is
        // written.
        // **Never slice by byte.** On an ordinary CJK directory name such as `你好` (an
        // AGENTS.md exception (iii) fixture: a CJK name whose UTF-8 boundaries are the
        // point), `name[..5]` cuts in the middle of a character and panics outright — and
        // this test runs inside approval classification, where one panic kills this
        // session's approval task.
        || folded.starts_with(".agit")
}

/// Does `target` land on `at` — `at` itself, or something under it?
///
/// # Why not `Path::starts_with`
///
/// That one compares path segments **byte for byte**, while the two paths this check holds
/// take their case from different places: `at` carries the spelling from disk (or from a
/// link's text), `target` carries the spelling the agent wrote this time, and
/// [`resolve_existing_ancestor`] normalizes only the part that **already exists** — the tail
/// that does not exist yet is kept unchanged. A dangling control-surface link
/// (`.claude/settings.json -> ../Claude.json`, target not yet created) is exactly the shape
/// this check exists for: compared byte for byte it and `claude.json` are two unrelated
/// paths, while the kernel resolves them into one file the instant this write lands.
///
/// So both sides go through [`fold_name`], using the same answer as the forward table.
fn resolves_under(target: &Path, at: &Path) -> bool {
    let mut segments = target.components();
    at.components().all(|a| {
        segments.next().is_some_and(|t| {
            t == a
                || fold_name(&t.as_os_str().to_string_lossy())
                    == fold_name(&a.as_os_str().to_string_lossy())
        })
    })
}

/// May the operator allow this tool call; if not, **why**.
///
/// `None` = the operator can answer.
///
/// # The default must be "no"
///
/// A denylist of dangerous words means **whatever it fails to recognize counts as safe**.
/// That direction is wrong structurally, not because the list is short: `python -c
/// "urllib..."`, `bash -c 'curl ...'`, `foo; curl ...`, `/usr/bin/curl`, `git fetch`,
/// `pip install`, one layer of base64, `$(...)` — every way around it takes a few characters.
/// The list can never be completed, and what it misses points at allow.
///
/// So the test is inverted: the operator can answer if and only if agitd can **positively
/// prove** this call is confined to the workspace — either its effect is described entirely
/// by structured paths and every one of them is inside the allowlist, or it is a command line
/// whose command name is on the built-in short list or on the owner's granted list
/// ([`crate::rc::grants`]), whose arguments are each validated, and which carries no shell
/// metacharacter outside quotes. Everything else goes back to the owner.
pub fn approval_owner_reason(
    tool: &str,
    input: &serde_json::Value,
    roots: &CanonicalRoots,
    cwd: &Path,
    granted_heads: &std::collections::BTreeSet<String>,
) -> Option<crate::protocol::OwnerReason> {
    use crate::protocol::OwnerReason::{Escalates, Unprovable};

    // ── Positive danger signals ──
    //
    // These are not "cannot be proven"; they are "proven to reach out". They are separate
    // because the sentence a person reads is different, and a warning that does not hold
    // drowns out the few that are really dangerous.
    if matches!(tool, "WebFetch" | "WebSearch") {
        return Some(Escalates);
    }
    // These fields come from the codex app-server schema: the harness has already said this
    // call reaches the network, widens a write root, or amends policy. It is more accurate
    // than guessing from a command line, and it is sent by the local codex, not by a
    // browser.
    for k in [
        "networkApprovalContext",
        "proposedNetworkPolicyAmendments",
        "proposedExecpolicyAmendment",
        "grantRoot",
        "permissions",
    ] {
        if input.get(k).is_some_and(|v| !v.is_null()) {
            return Some(Escalates);
        }
    }

    // With no allowlist there is no "inside" to speak of; the same holds when the session
    // itself is not in the workspace.
    if roots.is_empty() || !is_within(cwd, roots) {
        return Some(Unprovable);
    }
    // A codex exec approval carries its own cwd. It must be inside the allowlist, and it
    // must also be the base the relative paths in the command below resolve against.
    // Validating it and then dropping it makes the classifier and the process about to run
    // the command see two different paths (a `../x` that is safe under the session cwd is
    // already outside under the exec cwd). A non-string is not "not given": a broken wire
    // shape fails closed.
    let effective_cwd = match input.get("cwd") {
        None | Some(serde_json::Value::Null) => cwd.to_path_buf(),
        Some(serde_json::Value::String(c)) => {
            let Some(resolved) = resolve_against(c, cwd) else {
                return Some(Unprovable);
            };
            // `resolved` is the resolver's own answer; do not let `is_within` resolve it
            // a second time.
            if !within(&resolved, roots) {
                return Some(Escalates);
            }
            resolved
        }
        Some(_) => return Some(Unprovable),
    };
    let cwd = effective_cwd.as_path();

    match tool {
        // ── Tools that can do nothing at all ──
        //
        // A plan never persists, a todo list never persists, and every step inside a plan
        // comes back later as an approval of its own. Sending them to the owner
        // **punishes the correct choice**: the operator switches the session into plan mode
        // out of caution, the agent produces a plan, and the whole session then sits there
        // waiting for the owner.
        "ExitPlanMode" | "TodoWrite" => None,

        // ── The read-only verbs of agit's own MCP server ──
        //
        // Their inputSchema carries neither a path nor a command, so "prove it is confined to
        // the workspace" **never** holds for them — not because they are dangerous, but
        // because the test does not apply. What these verbs do is our own implementation, and
        // needs no inference. `mcp__agit__commit` is deliberately absent: it writes the repo.
        // Other MCP servers still go back to the owner, and the way out is the owner's
        // granted list, not another literal here.
        //
        // `search` and `rc_list` are **not** here, read-only as they look: they run under
        // **the machine owner's identity** (`agit search` goes through `require_login()` and
        // asks the hub with the owner's token), and their results come from the whole corpus
        // the owner can see — every team, every private project, including workspaces this
        // operator was never admitted to. `rc_list` is the same: it lists every machine the
        // owner has. A cross-workspace read is not harmless for being read-only.
        // `show` / `view` are **not** here either, for the same reason as `search`: by id
        // they read any session in the owner's local store, not just this workspace.
        //
        // `rc_status` and `status` are both out too — they look like local self-inspection
        // and land squarely on the rule above (a cross-workspace read is not harmless for
        // being read-only): `agit rc status` lists **every live session** on this machine
        // (id, runtime, state, seq), and `agit status` lists **every** agent repo in the
        // owner's local store (`clone::list_local()`, each with its own ahead/behind). Both
        // include workspaces this operator was never admitted to.
        //
        // So not one `mcp__agit__*` is hard-allowed. The way out is the owner's granted list,
        // not another literal here — which is exactly why `agit rc grant` exists.

        // ── Tools whose effect is described entirely by structured paths ──
        "Read" => {
            let paths = crate::rc::harness::paths_of(input);
            if paths.is_empty() {
                return Some(Unprovable);
            }
            paths
                .iter()
                .find_map(|p| confined_read_path(p, roots, cwd).err())
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let paths = crate::rc::harness::paths_of(input);
            if paths.is_empty() {
                return Some(Unprovable);
            }
            paths
                .iter()
                .find_map(|p| confined_write_path(p, roots, cwd).err())
        }
        // A `Glob`'s target lives in `pattern`, while `paths_of` reads only file_path /
        // path / notebook_path / edits[].file_path — it **never reads pattern**. So "no path
        // given means it acts on cwd" is false for Glob: no path given means the path is in
        // the field nobody looks at. `.all()` over an empty array is vacuously true, and
        // that is what would let `{"pattern":"/Users/**/.ssh/id_*"}` through with nothing
        // checked.
        "Glob" => {
            // **The search root can be given by `path` alone**, and the pattern is only
            // the shape below it.
            //
            // Looking at the pattern alone, `{"pattern":"**/*","path":"/etc"}` takes the
            // "no fixed prefix = acts on cwd" branch and is allowed, while the real root
            // `/etc` is never checked at all. So `path` is first validated into a confined
            // root (cwd is only the default), and that root is the base the pattern is
            // judged against.
            let base = match input.get("path").and_then(|v| v.as_str()) {
                Some(p) => match confined_read_path(p, roots, cwd) {
                    Ok(b) => b,
                    Err(e) => return Some(e),
                },
                None => cwd.to_path_buf(),
            };
            match input.get("pattern").and_then(|v| v.as_str()) {
                Some(pat) => confined_glob(pat, roots, &base).err(),
                None => Some(Unprovable),
            }
        }
        // These two really do act on cwd when no path is given, and cwd is already
        // validated.
        "Grep" | "LS" => crate::rc::harness::paths_of(input)
            .iter()
            .find_map(|p| confined_read_path(p, roots, cwd).err()),
        "Bash" | "shell" => match input.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => confined_command(cmd, roots, cwd, granted_heads).err(),
            // A codex `command` is `["string","null"]` by schema. An exec that cannot be
            // judged is the last thing that may count as safe.
            None => Some(Unprovable),
        },
        // A name we do not know — a third-party `mcp__*` tool, `Task`, codex's
        // `apply_patch` — goes back to the owner, uniformly.
        _ => Some(Unprovable),
    }
}

/// Compatibility shell: `ApprovalRequest.requires_owner` is still a bool.
pub fn approval_requires_owner(
    tool: &str,
    input: &serde_json::Value,
    roots: &CanonicalRoots,
    cwd: &Path,
    granted_heads: &std::collections::BTreeSet<String>,
) -> bool {
    approval_owner_reason(tool, input, roots, cwd, granted_heads).is_some()
}

/// Is a path confined to the allowlist (for a read)? Two things differ from [`is_within`],
/// and both are traps:
///
/// * a relative path resolves against **the session's cwd**. A `file_path` in an approval is
///   often `src/main.rs`, while agitd is a daemon whose own process cwd has nothing to do
///   with this session;
/// * `canonicalize` fails on a file that does not exist yet (`Write` creating one), and after
///   the fallback to lexical normalization the `escape` symlink pointing at `/etc` inside
///   `<root>/escape/new.rs` **cannot be caught**. So walk up to the first ancestor that
///   really exists, resolve that, and splice the rest back on.
fn confined_read_path(
    p: &str,
    roots: &CanonicalRoots,
    cwd: &Path,
) -> Result<PathBuf, crate::protocol::OwnerReason> {
    // Unresolvable (a dangling symlink on the way) = confinement is not proven.
    let Some(abs) = resolve_against(p, cwd) else {
        return Err(crate::protocol::OwnerReason::Escalates);
    };
    // The test looks at the path the caller takes away: `abs` is already the resolver's
    // answer.
    if within(&abs, roots) {
        Ok(abs)
    } else {
        Err(crate::protocol::OwnerReason::Escalates)
    }
}

/// The write version: inside the allowlist **and** not landing on a control surface. See
/// [`CONTROL_SURFACES`].
///
/// "Is it confined" can only be asked of the resolved path — a write lands wherever the
/// symlink points. But "is it a control surface" asks something else: **who will read it**.
/// A file can be reached by the agent's runtime under several names, and if **one** of them
/// is a control surface, this write is a code execution with no second approval. So the test
/// asks about the whole run of names [`names_along_resolution`] enumerates, not about two
/// endpoints.
fn confined_write_path(
    p: &str,
    roots: &CanonicalRoots,
    cwd: &Path,
) -> Result<PathBuf, crate::protocol::OwnerReason> {
    let abs = confined_read_path(p, roots, cwd)?;
    // A chain we cannot walk to the end = we never got that run of names, so nothing proves
    // there is no control surface in it.
    let Some(names) = names_along_resolution(&absolutize(p, cwd)) else {
        return Err(crate::protocol::OwnerReason::Escalates);
    };
    if names.iter().any(|n| is_control_surface(n)) {
        return Err(crate::protocol::OwnerReason::Escalates);
    }
    // The forward run holds the names on **this one path**. The same file carries other
    // names that are not on it, and those can only be asked in reverse. See
    // [`reachable_under_a_control_surface_name`].
    if reachable_under_a_control_surface_name(&abs, cwd, roots) {
        return Err(crate::protocol::OwnerReason::Escalates);
    }
    Ok(abs)
}

/// Complete the path in an approval into absolute form: joined onto **the session's cwd**,
/// with not one symlink resolved and not one `..` folded. This is **what it reads as** at the
/// moment of the write.
///
/// Deliberately no lexical normalization. `lexical` folds `..` literally, and the segment it
/// folds away is exactly the name the control-surface test needs (`.claude/../x` has no
/// `.claude` left once folded); using it to judge confinement is worse than useless — under a
/// link like `vendor -> /`, `vendor/../etc/passwd` folds to something inside the allowlist
/// while the kernel opens `/etc/passwd`. Each question has its own resolver: this one answers
/// only "what it reads as", and [`resolve_existing_ancestor`] answers "where it opens".
fn absolutize(p: &str, cwd: &Path) -> PathBuf {
    let raw = Path::new(p);
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    }
}

/// How many symlink hops one resolution follows at most. Linux's `MAXSYMLINKS` is 40:
/// looser than the kernel is pointless (the kernel gives ELOOP there and the write never
/// lands), and tighter than the kernel only pushes legitimate deep links to the owner.
const MAX_LINK_HOPS: usize = 40;

/// **Which names** the agent's runtime can reach this file by — every hop along the
/// resolution, not just the two endpoints.
///
/// # Why two endpoints are not enough
///
/// With both `alias -> .claude` and `.claude -> tooling` in the repo, writing
/// `alias/settings.json`: the resolved form is only `tooling/settings.json`, the literal form
/// is only `alias/settings.json`, and neither endpoint has a component named after a control
/// surface. Yet Claude Code still reads that same file next time as `.claude/settings.json`
/// and executes the hook inside it — run by the daemon itself, with no second approval. The
/// middle hop is the name that matters, and each end covers half of it. A longer chain
/// (`a -> b -> .cursor -> plain`) covers it more thoroughly still.
///
/// # Checked hop by hop, not "a link anywhere means the owner"
///
/// "Fail closed on any symlink along the way" is shorter to implement, but it pushes every
/// write in **ordinary layouts** — a monorepo's shared config, dotfile management, a pnpm
/// store, `docs -> ../shared` — to the owner. Once the approval queue is full of what the
/// operator could have answered, the owner starts going blind, and the guardrail's cost lands
/// on its own accuracy. And this run of names is **finite and enumerable**: recording "where
/// the link points + the tail not yet walked" at each hop is that file's complete name after
/// that hop. Since it can be enumerated exactly, there is no reason to trade a blanket rule
/// for a little implementation convenience.
///
/// # A chain that cannot be enumerated = `None`, and the caller treats it as a control surface
///
/// Checking hop by hop only holds if the walk **really reached the end**. Three ways it does
/// not:
///
/// * **a cycle** (`a -> b -> a`): it stops after [`MAX_LINK_HOPS`]. That is both the
///   guarantee of no recursion and no hang, and the answer itself — a cycle has no "that
///   file" to speak of, and the kernel gives ELOOP too.
/// * **not being able to tell whether a segment is a link** (an unreadable parent directory, a
///   plain file used as a directory on the way): `NotFound` is normal (the ordinary case of
///   `Write` creating a file), while every other `Err` means we know nothing about this
///   segment.
/// * **`read_link` failing** (swapped out after the lstat): the same.
///
/// All three return `None`. The remaining one — a **dangling link** — does not come through
/// here: its target reads out and its name is recorded as usual, while the "cannot resolve"
/// half already sent this write to the owner back in [`confined_read_path`].
fn names_along_resolution(spelled: &Path) -> Option<Vec<PathBuf>> {
    // The first one is what it reads as at the moment of the write.
    let mut names = vec![spelled.to_path_buf()];
    // `walked` is the prefix already resolved, `rest` is the piece not yet walked. A relative
    // link target resolves against **the directory the link lives in** — which is exactly
    // `walked` at that moment.
    let mut walked = PathBuf::new();
    let mut rest = spelled.to_path_buf();
    let mut budget = MAX_LINK_HOPS;
    loop {
        let mut comps = rest.components();
        let Some(c) = comps.next() else {
            return Some(names);
        };
        let tail = comps.as_path().to_path_buf();
        let next = match c {
            Component::Prefix(x) => {
                walked = PathBuf::from(x.as_os_str());
                tail
            }
            Component::RootDir => {
                walked.push(std::path::MAIN_SEPARATOR_STR);
                tail
            }
            Component::CurDir => tail,
            // Up one level **from the resolved location**, as the kernel does.
            Component::ParentDir => {
                walked.pop();
                tail
            }
            Component::Normal(name) => {
                let here = walked.join(name);
                match std::fs::symlink_metadata(&here) {
                    Ok(md) if md.is_symlink() => {
                        budget = budget.checked_sub(1)?;
                        let target = std::fs::read_link(&here).ok()?;
                        // An absolute target starts over; a relative one joins onto the
                        // directory the link lives in.
                        if target.is_absolute() {
                            walked = PathBuf::new();
                        }
                        let spliced = target.join(&tail);
                        // Another name for the same file after this hop — recorded with
                        // its tail, so `nm/.bin/x` under `nm -> node_modules` is
                        // recognized too.
                        names.push(walked.join(&spliced));
                        spliced
                    }
                    Ok(_) => {
                        walked = here;
                        tail
                    }
                    // Not there yet (`Write` creating it): the tail is spliced on
                    // unchanged, since it has no symlink to speak of.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        walked = here;
                        tail
                    }
                    Err(_) => return None,
                }
            }
        };
        rest = next;
    }
}

/// The reverse enumeration's three budgets. **Crossing any one of them counts as "cannot be
/// proven"**, so this write goes back to the owner.
///
/// # Why they must be counted separately
///
/// Reading a directory entry costs almost nothing — `read_dir` hands the name back along the
/// way; resolving a symlink to its landing (`location_of` → `canonicalize`) asks the kernel
/// segment by segment, one to two orders of magnitude more expensive. Holding links under one
/// "directory entry" budget charges at the cheapest item's rate: a directory of a few
/// thousand links can slow this approval down while most of the budget is still there, and a
/// slow approval and an unread approval are the same thing.
///
/// # The unit of account must be the unit actually paid
///
/// "One link" and "one directory entry" count **how many things we looked at**, but what is
/// paid to the kernel is never "things", it is **path segments**: every whole path handed to
/// the kernel (`canonicalize`, `read_dir`, `metadata`, `read_link`) is looked up segment by
/// segment from the start, and the cost follows that path's depth at that moment. And the
/// depth is not decided by the spelling — one `mid -> d/d/…/d` in the workspace lets a
/// spelling of a few segments like `.claude/jump` land on an arbitrarily deep existing
/// directory in **one** `canonicalize`, and the resolver then returns that very deep path to
/// keep walking. Charging by "things", one entry is billed once while what is actually paid
/// is that path's whole depth, and the depth is given by whoever wrote the link: the budget
/// looks mostly intact while this approval has already stopped there — and a stopped approval
/// and an unread approval are the same thing.
///
/// So the [`segments`](Self::segments) budget counts exactly segments: every time the reverse
/// enumeration hands a whole path to the kernel, it is charged its depth at that moment
/// **before** it is handed over, sharing one running total.
///
/// "Depth at that moment" is exact only if the hop does not grow into something else inside
/// the kernel. So [`resolve_ancestor`] follows links one hop at a time instead of letting a
/// single `canonicalize` swallow a whole vine: the length of a link's text is likewise
/// charged before the kernel walks it (see "one `canonicalize` may not swallow a whole vine"
/// there). How deep the landing is says nothing about how far the walk went — a vine can walk
/// deep and land shallow — so an account kept by the landing is short by exactly the piece
/// whoever wrote the link decides.
///
/// # Why not "check the depth after each normalization and refuse over a limit"
///
/// That road also plugs the hole above, and it is shorter. It was not chosen because it
/// stops the **result** while the money is already spent before the check: by the time
/// `canonicalize` returns, the kernel has walked that whole vine. So every single link can
/// still buy one kernel-capped deep walk of its own, and the total is still "count × the cap
/// per link" — while the bound to establish here is precisely the one on the total. The
/// segment budget works the other way round: the expensive hop breaks the account on the
/// spot, and not one hop after it moves.
///
/// # How the numbers are set, and why the segment one must be looser than the other two
///
/// All three sit between "a real repo cannot reach it" and "reaching it has not yet slowed
/// this approval past being read": what reaches them is a pathological or purpose-built tree.
/// The wall-clock cost follows the machine and the filesystem — do not write it down here;
/// re-run the benchmark to check it.
///
/// The segment budget carries one more constraint, and it is the one that decides **which way
/// this check errs**. The number of segments one resolution walks grows with the square of
/// the path depth (each segment the resolver descends hands the kernel the whole `cur` at
/// that depth), so `segments / links` is "how deep a path each link can afford". That
/// quotient must sit where real layouts' path depths cannot reach: an ordinary repo — even a
/// pnpm-style monorepo with hundreds of links in one `.bin` — always hits the link budget
/// first, and the segment budget speaks up first only when the depth is abnormal. Set the
/// other way round, it becomes this check's worst error: a normal deep repo walks the segment
/// budget dry, so **every** write in that workspace becomes the owner's problem, and a gate
/// that asks the owner for everything is the same as no gate.
///
/// Exhausting a budget goes back to the owner, not to the operator: see "not walked to the
/// end = not proven" in [`scan_for_alias`].
struct AliasBudget {
    /// How many more directory entries may be read.
    dirents: usize,
    /// How many more symlinks may be resolved to their landing. **The anchor's own resolution
    /// counts too** (see [`anchor_reaches`]): no `location_of` in the reverse enumeration is
    /// free, otherwise the expensive half falls exactly where nobody keeps the account.
    links: usize,
    /// How many more path segments the kernel may walk. This one caps not "how many things
    /// were looked at" but **how far it walked**.
    segments: usize,
}

impl AliasBudget {
    fn new() -> Self {
        Self {
            dirents: 200_000,
            links: 2_000,
            segments: 2_000_000,
        }
    }

    /// The forward pass's own budget ([`resolve_existing_ancestor`]): one path, one
    /// resolution.
    ///
    /// It does not share the account above — the reverse enumeration is "a whole patch of
    /// what is written in this workspace", the forward pass is "the one path the agent
    /// spelled this time", the number of times each happens in a pass is not the same, and
    /// sharing one account only lets whichever side runs first spend the other side's
    /// allowance. That is also why its segment count is far smaller than the reverse one's:
    /// this pass walks a single path, and how deep a path the kernel accepts is decided by
    /// `PATH_MAX`; only the spelling itself (a JSON string with no length bound) can push the
    /// number up, and the kernel refuses such a spelling anyway.
    ///
    /// Neither the directory-entry nor the link budget is asked on this side —
    /// [`resolve_ancestor`] charges segments only.
    fn for_one_path() -> Self {
        Self {
            segments: 250_000,
            ..Self::new()
        }
    }

    /// Read one directory entry. `false` = the budget is exhausted, and the caller must
    /// treat it as the owner's.
    fn take_dirent(&mut self) -> bool {
        Self::take(&mut self.dirents, 1)
    }

    /// Resolve one symlink to its landing. `false` = the budget is exhausted, and the caller
    /// must treat it as the owner's.
    fn take_link(&mut self) -> bool {
        Self::take(&mut self.links, 1)
    }

    /// Hand the kernel one whole path `deep` segments deep to walk. `false` = the budget is
    /// exhausted, and the caller must treat it as the owner's.
    fn take_segments(&mut self, deep: usize) -> bool {
        Self::take(&mut self.segments, deep)
    }

    fn take(counter: &mut usize, n: usize) -> bool {
        match counter.checked_sub(n) {
            Some(left) => {
                *counter = left;
                true
            }
            None => {
                // What cannot be charged is charged to zero: this enumeration has crossed
                // the line, and every hop after it must be unanswerable too.
                *counter = 0;
                false
            }
        }
    }
}

/// **What other names can reach this file** — the ones outside the forward run.
///
/// # Why the forward pass is not enough
///
/// [`names_along_resolution`] answers "what **this path** reads as at the moment of the
/// write": it starts from the spelling and walks forward hop by hop, so it sees only the
/// names on this one path. But the other names the same file carries are not on it. Where
/// `.claude -> tooling` comes with the clone (a monorepo sharing one config, dotfile
/// management), `tooling/settings.json` and `.claude/settings.json` are **one file**, and the
/// first spelling — the shorter, more natural one, the one the agent is likelier to write —
/// walks out a list with no `.claude` in it at all. Walking forward only, this write is
/// judged operator-answerable, while Claude Code still reads it next time as
/// `.claude/settings.json` and executes the hook inside it: run by the daemon itself, with no
/// second approval. Fixing long chains without asking in reverse plugs the long chain and
/// leaves the shortest one open.
///
/// So what this check asks about is not the spelling but the **file**: does any control
/// surface's name in the workspace land on it.
///
/// # Why the reverse side can enumerate this little
///
/// "Who points at this file" has no index in a filesystem, and answering it directly means
/// scanning the whole tree — a whole-tree scan per write, whose cost on a person is a slower
/// approval, and a slow approval and an unread approval are the same thing. So the reverse
/// side does not scan the tree; it enumerates only the **finite and known** handful:
///
/// * **anchors** are looked for on two ancestor chains only — the target's own and the
///   session cwd's, each cut off at the allowlist's root ([`ControlSurfaceScope`]).
/// * each level is `read_dir`-ed once and the entry names go through
///   [`is_control_surface_name`] — an order of magnitude fewer syscalls than lstat-ing that
///   whole table entry by entry, and the `.agit*` prefix rule and case folding both come out
///   right on their own.
/// * an anchor that matches is then walked into **exhaustively** by [`scan_for_alias`], for
///   the sake of **file-level** aliases: `.claude/settings.json -> ../shared/claude.json` is
///   the standard stow / dotfile layout, and it makes `shared/claude.json` just as much a
///   code execution with no second approval.
///
/// # Cannot be proven = yes
///
/// `true` here means "send it to the owner". A directory that cannot be read, an entry whose
/// type cannot be asked, a link whose target cannot be answered, an exhausted budget — all of
/// them return `true`. This check's default must be "no": missing one alias costs the daemon
/// executing what the agent just wrote.
fn reachable_under_a_control_surface_name(
    target: &Path,
    cwd: &Path,
    roots: &CanonicalRoots,
) -> bool {
    reachable_under_a_control_surface_name_within(target, cwd, roots, &mut AliasBudget::new())
}

/// [`reachable_under_a_control_surface_name`], with the budget given by the caller. Split out
/// so a regression can see **what this enumeration spent**, not only what it answered.
fn reachable_under_a_control_surface_name_within(
    target: &Path,
    cwd: &Path,
    roots: &CanonicalRoots,
    budget: &mut AliasBudget,
) -> bool {
    // The directories in this sweep are ancestors of `target` and of cwd, and `target`
    // itself may be a very deep path **a link expanded into** (see "the unit of account" in
    // [`AliasBudget`]), so how long this chain is is not decided by the spelling.
    // [`ControlSurfaceScope`] hands them out one level at a time, and every level is charged
    // its depth **before** it is handed over.
    let mut scope = ControlSurfaceScope::new(target, cwd, roots);
    while let Some(step) = scope.next_dir(budget) {
        let Ok(dir) = step else { return true };
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // Not there yet (`Write` creating a whole stretch of directories along the
            // way): a directory that does not exist can hold no alias.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return true,
        };
        for entry in entries {
            let Ok(entry) = entry else { return true };
            if !budget.take_dirent() {
                return true;
            }
            let raw = entry.file_name();
            let name = raw.to_string_lossy();
            if is_control_surface_name(&name) && anchor_reaches(&entry.path(), target, &mut *budget)
            {
                return true;
            }
            // `node_modules/.bin` is **two** segments, and one level of `read_dir` cannot
            // see it; meanwhile `node_modules/.bin -> tools` makes `tools/tsc` the `tsc` the
            // next command runs outright. One extra probe buys that hole shut.
            //
            // **Ask even when `.bin` does not exist yet.** With `node_modules -> vendor/nm`
            // and nothing installed under `vendor/nm` yet, `.bin` is exactly the segment this
            // write would create — taking "already exists" as a premise means looking away
            // from precisely the shape this check exists for. [`location_of`] has an answer
            // for a tail that does not exist yet, so ask it.
            //
            // The name goes through [`fold_name`] before the comparison, asking the same
            // question as the forward pair ([`folded_is_node_modules`]): comparing ASCII case
            // alone leaves a spelling like `node_module\u{17f}`, which is the same directory
            // on disk, unrecognized in reverse, and unrecognized points at allow.
            if folded_is_node_modules(&fold_name(&name)) {
                match entry.file_type() {
                    // A plain file can hold no `.bin` under it, neither now nor after the
                    // write.
                    Ok(ft) if ft.is_file() => {}
                    Ok(_) => {
                        if anchor_reaches(&entry.path().join(".bin"), target, &mut *budget) {
                            return true;
                        }
                    }
                    // What this entry is cannot be asked.
                    Err(_) => return true,
                }
            }
        }
    }
    false
}

/// Which directories to look in for a control-surface name that reaches `target`: two
/// ancestor chains — the target's own and **the session cwd's** — each cut off at the
/// allowlist's root. Outside the roots is neither a range we can enumerate nor the tree this
/// approval is about.
///
/// # This is a **bound**, not a claim that nothing can be elsewhere
///
/// There can be. When `packages/app/node_modules/.bin/tsc -> ../../../../payload.js` hangs in
/// a sibling subtree neither chain passes through, that directory is never opened here, so
/// writing `payload.js` gets an operator-answerable answer — **not looked at**, not "looked
/// at, nothing there". npm/pnpm look upward for `node_modules/.bin` from the package
/// directory the executed script lives in, direnv/git look upward for `.envrc` / `.git` from
/// the command's cwd, and those places need not sit on either chain.
///
/// The bound is drawn here because the other two ways of drawing it are worse:
///
/// * **scan the whole tree**: "who points at this file" has no index in a filesystem, and
///   answering it directly is a whole-tree scan per write. A repo with `node_modules`
///   exhausts [`AliasBudget`] in one go, and an exhausted budget answers owner — so **every**
///   write in that workspace becomes the owner's problem. A gate that asks the owner for
///   everything is the same as no gate: once the approval queue is full, the one that really
///   needed looking at is not looked at either.
/// * **keep the target's chain only** (drop the cwd chain and the answer depends on the file
///   alone): what is dropped is exactly half of why this check exists. The daemon's own
///   settlement child process starts on **the session cwd**, and git reads `.git/` upward
///   from there — a control-surface alias on that chain is of the "run by the daemon itself,
///   with no second approval" kind, the one that must never be missed here.
///
/// # So the answer follows the session cwd
///
/// One file can get different answers in two sessions with different cwds. That is not an
/// unfinished trade-off; the question this check asks carries the session by construction:
/// not "is this file dangerous" but "can **this write** become an execution in **this
/// session** without a second approval". The cwd is fixed when the session starts
/// (`spec.cwd`) and the agent's `cd` cannot move it, so the session sitting next to the trap
/// getting the stricter answer is correct.
///
/// For the gap left open (a control surface in a subtree neither chain passes through) to
/// become an execution, somebody must first run a command in that directory; and running a
/// command is itself either on the built-in read-only list (none of which can run
/// `node_modules/.bin`) or granted by the owner in person — and in the world where the owner
/// has already granted a build tool, the ordinary files in that same subtree that are **not**
/// control surfaces, `package.json` and `Makefile`, long since handed over the same
/// capability, so this alias grants nothing more.
///
/// # This chain is **charged as it is walked**, never flattened into a `Vec` first
///
/// How long the chain is is known only **after resolution**: a link like `mid -> d/a/…/a`
/// lets a spelling of a few segments like `mid/x` expand into an arbitrarily deep path, so
/// the number of levels in these two ancestor chains is given by whoever wrote the link.
/// Copying the whole chain into a container and only then holding each level in it under
/// [`AliasBudget`] opens the account **after** the very cost it is there to stop: the copy is
/// itself one whole path per level, and what queues behind this check is the approval thread.
///
/// So levels are handed out one at a time, and each is charged segments for its depth at that
/// moment **before** it is handed over — what cannot be charged stops on the spot and not one
/// further level is touched. The number of path segments the whole enumeration touches is
/// then capped by the budget rather than by the chain's length.
///
/// Deduplication is the same, and cannot rest on "compare against everything already handed
/// out": that is one linear scan per level, and a deep chain turns it into a pile of cost
/// compared against itself, likewise outside the budget. The two chains overlap only on their
/// **common prefix**, and the target's chain hands out exactly all of `target`'s true
/// ancestors — so the levels on the cwd chain that are "shallower than `target` and a
/// segment-wise prefix of it" are the whole overlap, answerable by one prefix comparison,
/// with no need to remember who was handed out.
struct ControlSurfaceScope<'a> {
    /// `target` is itself the file to be written, not a directory, so start at its parent.
    from_target: std::iter::Skip<std::path::Ancestors<'a>>,
    from_cwd: std::path::Ancestors<'a>,
    target: &'a Path,
    target_deep: usize,
    roots: &'a CanonicalRoots,
    /// Stop once the budget is exhausted: this enumeration has crossed the line, and not one
    /// further level should be touched.
    done: bool,
}

impl<'a> ControlSurfaceScope<'a> {
    fn new(target: &'a Path, cwd: &'a Path, roots: &'a CanonicalRoots) -> Self {
        Self {
            from_target: target.ancestors().skip(1),
            from_cwd: cwd.ancestors(),
            target,
            target_deep: target.components().count(),
            roots,
            done: false,
        }
    }

    /// The next directory to open. `None` = both chains reached their end; `Some(Err(()))` =
    /// the budget is exhausted, and the caller treats it as "cannot be proven" (back to the
    /// owner).
    fn next_dir(&mut self, budget: &mut AliasBudget) -> Option<Result<PathBuf, ()>> {
        if self.done {
            return None;
        }
        while let Some((dir, from_cwd)) = self.step() {
            // Opening a directory makes the kernel walk it segment by segment from the
            // start; judging whether it is inside a root and whether it overlaps the target
            // chain likewise compares this whole level segment by segment. All the work of
            // this level is capped by its own depth, so **charge first, look after** — what
            // cannot be charged stops on the spot, and all that stays off the account is the
            // one pass that sized this level.
            let deep = dir.components().count();
            if !budget.take_segments(deep) {
                self.done = true;
                return Some(Err(()));
            }
            // The roots are already canonical and `target` / `cwd` are already resolved, so
            // a segment-wise comparison is enough — canonicalizing again here would hand
            // "the root validated at bind time" back to whatever the symlinks say now.
            if !self.roots.iter().any(|r| dir.starts_with(r)) {
                continue;
            }
            // The target chain handed out all of `target`'s true ancestors, so a shallower
            // segment-wise prefix on the cwd chain is a duplicate. Equal depth can only be
            // `target` itself — it was never handed out, so hand it out.
            if from_cwd && deep < self.target_deep && self.target.starts_with(dir) {
                continue;
            }
            return Some(Ok(dir.to_path_buf()));
        }
        self.done = true;
        None
    }

    /// The next level of the two chains joined end to end, plus "it came from the cwd
    /// chain".
    fn step(&mut self) -> Option<(&'a Path, bool)> {
        match self.from_target.next() {
            Some(dir) => Some((dir, false)),
            None => self.from_cwd.next().map(|dir| (dir, true)),
        }
    }
}

/// Does this control-surface name reach `target`? `true` = it does, or it cannot be
/// answered.
fn anchor_reaches(anchor: &Path, target: &Path, budget: &mut AliasBudget) -> bool {
    // Resolving an anchor to its landing is the same thing at the same price as resolving a
    // link inside [`scan_for_alias`], so it goes on the same budget. The number of anchors is
    // not "structurally small": the `.agit` prefix rule lets one directory hold arbitrarily
    // many matching names, and with no account the expensive half falls entirely off it.
    if !budget.take_link() {
        return true;
    }
    let Some(at) = location_of(anchor, budget) else {
        return true;
    };
    // Directory-level alias: the control surface lands on `target`, or on one of its
    // ancestors — where `.claude` points at `tooling/`, every file under `tooling/` is also
    // called `.claude/…`.
    if resolves_under(target, &at) {
        return true;
    }
    // File-level alias: is there a link **inside** this control surface (walking through the
    // links it carries itself) that points at `target`.
    scan_for_alias(&at, target, budget)
}

/// Is there a link inside the control surface that points at `target`? `true` = there is, or
/// **the walk did not finish**.
///
/// # Only a finished walk may say `false`
///
/// This function's `false` means "the operator may allow this write". So it holds under one
/// condition only: **every last alias** reachable from this control surface that the runtime
/// can really execute has been looked at. Hence no depth limit, and a link landing on a
/// directory is walked into — after `.git/hooks -> ../shared-hooks`, the
/// `shared-hooks/pre-commit` alias that gets executed is in the landing directory, not next
/// to the link; and **any** branch that was not walked — an exhausted budget, a directory
/// that cannot be read, an entry whose type cannot be asked, a link whose target cannot be
/// answered — is `true` back to the owner, uniformly. "Did not finish" and "there is no
/// alias" are not the same thing, and must never be read as one `false`.
///
/// # Why it walks exhaustively and not only the paths each control surface really executes
///
/// The other road is a table per control surface of "the paths the runtime really executes"
/// (`.git` only `config` and `hooks/`, `.claude` only `settings.json` and `hooks/`, ...), and
/// resolving only what is on it. It is faster, far faster: `.git/objects` and `.git/refs`
/// never have to be opened. **It was not chosen, because its error is allow.** Nearly every
/// name in [`CONTROL_SURFACES`] belongs to someone else's tool, and such a table is our
/// **guess** about them: missing `.claude/plugins/`, missing `.cargo/config` (the old
/// spelling without `.toml`), missing a config path some tool adds in its next release —
/// every omission becomes the sentence "no alias found", and no test recognizes it. The table
/// also goes stale, and stale points at allow the same way. Walking exhaustively is the other
/// way round: what we did not think of is merely walked over, at the cost of time rather than
/// of a code execution with no second approval. That time is capped by [`AliasBudget`], and
/// an exhausted budget likewise goes back to the owner.
///
/// # Termination
///
/// The queue holds only **canonicalized** directory paths (`location_of` /
/// [`resolve_existing_ancestor`] resolved every link segment), and `visited` deduplicates on
/// that canonical form, so one directory is walked once. Cycles like `a -> .`, or `a -> b`
/// with `b -> a`, are stopped by `visited` on the second visit and the traversal must halt;
/// a cycle that truly cannot be resolved (`canonicalize` gives ELOOP outright) is caught by
/// `location_of` returning `None` and goes back to the owner. The queue is an explicit `Vec`
/// rather than recursion — a deliberately deep directory tree cannot blow the stack.
fn scan_for_alias(start: &Path, target: &Path, budget: &mut AliasBudget) -> bool {
    let mut visited: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    let mut queue: Vec<PathBuf> = Vec::new();
    if enqueue_if_dir(start.to_path_buf(), &mut visited, &mut queue).is_err() {
        return true;
    }
    while let Some(dir) = queue.pop() {
        // The directories in the queue are **resolved landings** whose depth is given by the
        // links in the workspace: one `mid -> d/d/…/d` turns every `read_dir` of this
        // traversal into a deep walk. Charge by segment.
        if !budget.take_segments(dir.components().count()) {
            return true;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            // There a moment ago and gone now: it has no entries to speak of.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return true,
        };
        for entry in entries {
            let Ok(entry) = entry else { return true };
            if !budget.take_dirent() {
                return true;
            }
            // `file_type` uses the `d_type` readdir hands back along the way, and usually
            // costs no extra syscall — so this scan's cost is "reading a few directories",
            // not "stat-ing tens of thousands of files".
            let Ok(ft) = entry.file_type() else {
                return true;
            };
            if ft.is_symlink() {
                // Resolving a link costs fifty times what reading a directory entry costs,
                // so it gets a budget of its own.
                if !budget.take_link() {
                    return true;
                }
                let Some(at) = location_of(&entry.path(), budget) else {
                    return true;
                };
                // The link points at the target outright, or at one of its ancestors.
                if resolves_under(target, &at) {
                    return true;
                }
                // Pointing at a **directory**: the vine has not ended. After
                // `.git/hooks -> ../shared-hooks`, that `pre-commit` alias is in
                // `shared-hooks/`, not in `.git/hooks/`.
                if enqueue_if_dir(at, &mut visited, &mut queue).is_err() {
                    return true;
                }
            } else if ft.is_dir() {
                // `dir` is already canonical and this segment is not a link, so what is
                // joined stays canonical.
                let here = entry.path();
                if visited.insert(here.clone()) {
                    queue.push(here);
                }
            }
        }
    }
    false
}

/// Queue an **already resolved location** to be walked — it has entries to speak of only if
/// it is a directory.
///
/// `Err(())` = what this location is cannot be asked, and the caller treats it as the
/// owner's.
/// # This `metadata` is not charged again
///
/// `at` is the landing [`location_of`] just resolved, and the last hop of that resolution
/// charged exactly **the landing's own depth** (see [`resolve_ancestor`]) — the same depth of
/// the same path is already on the account. Charging again bills one stretch of path twice
/// and leaves a line in the budget that no regression holds down.
fn enqueue_if_dir(
    at: PathBuf,
    visited: &mut std::collections::BTreeSet<PathBuf>,
    queue: &mut Vec<PathBuf>,
) -> Result<(), ()> {
    match std::fs::metadata(&at) {
        Ok(md) if md.is_dir() => {
            if visited.insert(at.clone()) {
                queue.push(at);
            }
            Ok(())
        }
        // Not a directory (a single-file control surface like `.envrc`, or a link pointing
        // at a file).
        Ok(_) => Ok(()),
        // Dangling: that location was already compared above ([`resolves_under`]), and
        // there is nothing under it now.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(()),
    }
}

/// Which **location** this name points at. When it cannot be answered the result is `None`,
/// and the caller treats it as a control surface.
///
/// It differs from plain `resolve_existing_ancestor` in the last segment: that function
/// returns `None` on a dangling link, while dangling is exactly the shape that matters most
/// here — a repo comes cloned with `.claude/settings.json -> ../shared/claude.json` while
/// `shared/claude.json` has not been created. That link points at a location that does not
/// exist, and the agent's write is what would make it exist. So the link itself is taken
/// apart first (`read_link` plus a join onto the directory it lives in) and then handed to
/// the same resolver — which splices a tail that does not exist yet on unchanged, so we still
/// get that location's name.
///
/// There is an answer even when `p` does not exist at all: probing a two-segment name like
/// `node_modules/.bin` asks about a location only this write would create. Only "exists but
/// cannot be answered" (permissions, ELOOP) is `None`.
///
/// Every segment this pass walks goes on `budget`: `p`'s own lstat and the segment-by-segment
/// resolution of the path spliced from the link's text are all deep walks by the kernel. See
/// "the unit of account must be the unit actually paid" in [`AliasBudget`].
fn location_of(p: &Path, budget: &mut AliasBudget) -> Option<PathBuf> {
    // `symlink_metadata` and `read_link` both make the kernel walk `p` from the start.
    if !budget.take_segments(p.components().count()) {
        return None;
    }
    match std::fs::symlink_metadata(p) {
        Ok(md) if md.is_symlink() => {
            let target = std::fs::read_link(p).ok()?;
            let spliced = if target.is_absolute() {
                target
            } else {
                p.parent()?.join(target)
            };
            resolve_metered(&spliced, budget)
        }
        Ok(_) => resolve_metered(p, budget),
        // Not there yet: the tail is spliced on unchanged, and the location's name still
        // works out.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => resolve_metered(p, budget),
        Err(_) => None,
    }
}

/// [`resolve_existing_ancestor`], but every hop is charged segments from [`AliasBudget`] at
/// its real depth **after expansion**; what cannot be charged returns `None`, and the caller
/// treats it as "cannot be answered" (back to the owner).
///
/// The reverse enumeration resolves **what is written in the workspace**: each segment the
/// resolver descends hands the kernel the whole `cur`, and how deep `cur` is is decided by
/// the link targets. So this side must not walk for free.
fn resolve_metered(p: &Path, budget: &mut AliasBudget) -> Option<PathBuf> {
    resolve_ancestor(p, Some(budget))
}

/// Where the `}` matching the `{` at `pattern[open]` is. **Count depth**, do not take the
/// first one.
fn matching_brace(pattern: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in pattern.char_indices().filter(|(i, _)| *i >= open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split on **top-level** commas. `a,{b,c},d` → `["a", "{b,c}", "d"]`.
fn top_level_commas(body: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in body.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&body[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&body[start..]);
    out
}

/// One level of brace expansion. `a/{x,y}/b` → `["a/x/b", "a/y/b"]`.
///
/// One level is enough: what is wanted here is not "bit-for-bit agreement with a glob
/// library" but "every **possible** expansion must go through the path test". What does not
/// expand (no braces, or nesting we cannot read) is returned unchanged, and that one takes
/// the fixed-prefix test below.
fn brace_alternatives(pattern: &str) -> Vec<String> {
    // The cap stops the combinatorial blow-up: ten `{a,b}` groups spell 1024 alternatives.
    // Reaching it returns empty — empty means "does not expand", the caller treats it as
    // `Unprovable`, and that direction is the safe one.
    const MAX: usize = 256;
    let mut out = vec![pattern.to_string()];
    // **Expand until no brace is left.** Expanding only the first group, `{,x}{/etc/**,y}`
    // still starts with `{` after one expansion, and the "relative pattern with no fixed
    // prefix" rule below allows it — while glob's full expansion contains `/etc/**`.
    loop {
        let Some(i) = out.iter().position(|p| p.contains('{')) else {
            return out;
        };
        let p = out.swap_remove(i);
        let Some(open) = p.find('{') else {
            out.push(p);
            return out;
        };
        // **Matching counts depth.** Closing the outermost `{` with the first `}` is wrong:
        // the first `}` of `{a,{,/}etc}` closes the **inner** one, the alternatives cut by it
        // hold no `/etc` while the real expansion does — so that Glob is judged a relative
        // path and allowed.
        let Some(close) = matching_brace(&p, open) else {
            // Unclosed: it does not expand. **Fail closed** — a pattern we cannot read must
            // not count as safe.
            return vec![];
        };
        let (head, tail) = (&p[..open], &p[close + 1..]);
        for alt in top_level_commas(&p[open + 1..close]) {
            out.push(format!("{head}{alt}{tail}"));
        }
        if out.len() > MAX {
            return vec![];
        }
    }
}

/// A glob's target: the stretch before the first wildcard is validated as a path.
///
/// **Every brace alternative is validated on its own.** Looking at the raw string's first
/// byte does not do: the first byte of `{/etc/**,x}` is `{`, so "the fixed prefix is empty"
/// holds and `starts_with('/')` does not, and it is allowed all the way — while every glob
/// implementation with brace expansion (minimatch / fast-glob / glob, which is what sits
/// under the Glob tool) expands the first alternative to `/etc/**` and walks out of the
/// workspace. `[/]etc/*` and `{,/}etc/**` are the same.
fn confined_glob(
    pattern: &str,
    roots: &CanonicalRoots,
    cwd: &Path,
) -> Result<(), crate::protocol::OwnerReason> {
    use crate::protocol::OwnerReason::Escalates;
    let alts = brace_alternatives(pattern);
    if alts.is_empty() {
        return Err(Escalates); // does not expand = cannot be judged, lean strict
    }
    for alt in alts {
        if alt.split('/').any(|seg| seg == "..") {
            return Err(Escalates);
        }
        // **A wildcarded directory segment cannot be proven to stay in the workspace.**
        //
        // `head` validates only the stretch **before** the first wildcard, so moving the
        // wildcard to the front means the whole test never runs a step: `*/.ssh/id_*` has an
        // empty fixed prefix and is not absolute, and is allowed outright. And the symlinks
        // this file keeps listing (`docs -> ../shared`, `target -> /Volumes/build`, a pnpm
        // store, and the `rootlink -> /` in the test above) — what `*` matches is exactly
        // their **names**, glob readdirs out along one of them, and the paths it returns are
        // inside no root. `rootlink/*` is stopped only because `confined_read_path` resolved
        // the `rootlink/` segment.
        //
        // The one exception is a segment that is exactly `**`: that is recursion "downward
        // from here", the same class as the `grep -r` the list allows. A named wildcarded
        // segment asks glob to readdir a directory we cannot say where it points, which is
        // the `-R` class (see
        // `a_recursive_flag_that_follows_symlinks_is_not_on_the_list`), and goes to the
        // owner.
        //
        // The last segment does not count: it is a file name, and whatever it matches is not
        // walked into.
        let segments: Vec<&str> = alt.split('/').collect();
        if segments[..segments.len().saturating_sub(1)]
            .iter()
            .any(|seg| *seg != "**" && seg.contains(['*', '?', '[']))
        {
            return Err(Escalates);
        }
        let head: String = alt
            .split(['*', '?', '[', '{'])
            .next()
            .unwrap_or_default()
            .to_string();
        if head.trim_end_matches('/').is_empty() {
            // No fixed prefix. The absolute form scans from the root and cannot be allowed;
            // the relative form acts on cwd, and cwd is already validated. "Absolute" is
            // judged on **this alternative**, not on the raw string.
            if alt.starts_with('/') {
                return Err(Escalates);
            }
            continue;
        }
        confined_read_path(&head, roots, cwd)?;
    }
    Ok(())
}

fn resolve_against(p: &str, cwd: &Path) -> Option<PathBuf> {
    resolve_existing_ancestor(&absolutize(p, cwd))
}

/// Resolve a path into "the location the kernel really opens".
///
/// # Why lexical normalization must not come first
///
/// Leading with `lexical(p)` does not work — it folds `a/../b` into `b` **literally**, while
/// `..` means "up one level from the **resolved** current location": with a `vendor -> /`
/// symlink in the workspace (a pnpm store, `docs -> ../shared`, `target -> /Volumes/build`,
/// all common), `vendor/../etc/passwd` folds lexically into `<root>/etc/passwd` — inside the
/// allowlist — while the kernel resolves `vendor` to `/`, goes up one level to `/` again, and
/// finally opens `/etc/passwd`.
///
/// So it walks segment by segment: each segment it descends, the part that **already exists**
/// is canonicalized (symlinks are resolved at that step), and `..` pops only after that. A
/// tail that does not exist yet (`Write` creating a file) is spliced on unchanged, since it
/// has no symlink to speak of.
///
/// # This pass has its own segment budget; the reverse enumeration ([`resolve_metered`]) too
///
/// Both sides need a floor, for different reasons. On the reverse side the number of
/// resolutions and the depth of each are **what is written in the workspace**, both ends
/// decided by whoever wrote the links, so that side charges one shared account all the way
/// down. This side resolves **the one** path the agent spelled this time, and the count is
/// fixed by the call site — but the length is not: the spelling is a JSON string in an
/// approval message, and the kernel never sees its whole form (the kernel holds only "link
/// target + tail not yet walked", and the prefix already walked is a vnode), so "the kernel's
/// own `PATH_MAX` caps it for us" does not hold for a spelling. And each segment this pass
/// descends hands the kernel the whole `cur` at that moment, so the cost grows with the
/// spelling's length and depth together. So segments are charged here too, just on an account
/// of its own ([`AliasBudget::for_one_path`]): exhausted = cannot be proven = back to the
/// owner, the same direction as everywhere else.
fn resolve_existing_ancestor(p: &Path) -> Option<PathBuf> {
    resolve_ancestor(p, Some(&mut AliasBudget::for_one_path()))
}

/// The segment-by-segment pass [`resolve_existing_ancestor`] and [`resolve_metered`] share.
///
/// # One `canonicalize` may not swallow a whole vine
///
/// Links are followed here one hop at a time (`read_link`, then the link's text goes back
/// onto the queue still to be walked), not handed to `canonicalize` to follow to the end in
/// one call. Both roads resolve to the same landing; what differs is the **account**: inside
/// one `canonicalize(cur)` the kernel follows up to [`MAX_LINK_HOPS`] hops, each hop's target
/// can be as long as `PATH_MAX`, and it hands back the **landing** alone. How deep the
/// landing is has nothing to do with how many segments the pass walked —
/// `mid -> d/a/…/a/../…/..` walks hundreds of segments and lands on `d` — so an account kept
/// by the landing records the small change, and the difference is decided by whoever wrote
/// the link. Following it ourselves makes every hop visible: the length of the link's text is
/// charged **before** the kernel walks it.
///
/// `canonicalize` is kept for its other half of the work — fetching back the spelling on
/// disk: on a case-insensitive volume `.CLAUDE` is stored as `.claude`, and which allowlist
/// root it lands under is compared byte for byte. By the time it runs, this segment is known
/// not to be a link and the prefix is already canonical, so that pass follows no link at all
/// and the segments it walks are this path's depth at that moment — the number already
/// charged.
///
/// # The account is settled before the kernel moves
///
/// While `work` is `Some`, every whole path is charged segments for its depth at that moment
/// **before** it is handed to the kernel; the link text about to be expanded is likewise
/// charged before the kernel walks it. So no segment is charged only after it was walked.
///
/// # `spliced`: this segment came out of a link's text
///
/// It is tracked separately because "does not exist" means two different things on the two
/// sides. A missing tail in the spelling is the ordinary case of `Write` creating a file, and
/// is spliced on unchanged; a missing segment in a link's text makes that link **dangling** —
/// `<root>/link -> /etc/cron.d/x` (target not yet created) judged as `<root>/link` lands
/// inside the allowlist, while one `Write` writes straight into /etc. What cannot be judged
/// says so: return `None`, and the caller takes the strictest reading.
///
/// # "Cannot be judged" and "not there" are two answers as well
///
/// What each descended segment hands the kernel is the whole `cur` **after expansion**, while
/// the kernel walking the same spelling never holds anything that long: it keeps only "link
/// target + tail not yet walked", and the prefix already walked is a vnode. So `cur` can grow
/// past the length the kernel accepts while the kernel still opens that spelling — and from
/// that segment on, `read_link`, `canonicalize` and `symlink_metadata` all answer
/// `ENAMETOOLONG`. Reading that as "this segment does not exist yet" does not cost one more
/// question to the owner; it means **this resolution stops following symlinks altogether**:
/// the remaining tail is spliced on unchanged, the location judged sits squarely inside an
/// allowlist root, and the kernel opens the file outside it. `ENOTDIR`, `EACCES` and `ELOOP`
/// are the same shape.
///
/// So only `NotFound` may say "not there", and everything else is `None` — the same rule as
/// in [`names_along_resolution`]. This check may err only toward asking once more.
fn resolve_ancestor(p: &Path, mut work: Option<&mut AliasBudget>) -> Option<PathBuf> {
    use std::path::Component;
    let mut cur = PathBuf::new();
    // The segments not yet walked, each carrying "did this come out of a link's text".
    let mut rest: std::collections::VecDeque<(std::ffi::OsString, bool)> = p
        .components()
        .map(|c| (c.as_os_str().to_os_string(), false))
        .collect();
    // How many link hops this pass has followed so far. The kernel's `MAXSYMLINKS` is the
    // allowance of **one resolution**, and one resolution is exactly this whole path walked
    // from start to end here: when the kernel walks `a/b/c`, the links followed on `a`, `b`
    // and `c` go on one counter, and filling it means `ELOOP` for the whole path. So this
    // number accumulates and is never reset per spelled segment — resetting lets every
    // segment buy its own [`MAX_LINK_HOPS`], and how many segments a spelling has is decided
    // by whoever writes the approval message.
    let mut hops = 0usize;
    while let Some((seg, spliced)) = rest.pop_front() {
        match Path::new(&seg).components().next() {
            Some(Component::Prefix(x)) => cur.push(x.as_os_str()),
            Some(Component::RootDir) => cur.push(std::path::MAIN_SEPARATOR_STR),
            Some(Component::CurDir) | None => {}
            // Up one level **from the resolved location**.
            Some(Component::ParentDir) => {
                cur.pop();
            }
            Some(Component::Normal(name)) => {
                cur.push(name);
                // Charge before handing it over: the kernel looks it up segment by segment
                // from the start, and how many lookups that is is this path's depth right
                // now.
                if let Some(b) = work.as_deref_mut()
                    && !b.take_segments(cur.components().count())
                {
                    return None;
                }
                match std::fs::read_link(&cur) {
                    // A link. `read_link` settles two things at once: that it is a link,
                    // and what **the path the kernel walks next** looks like.
                    Ok(target) => {
                        // A cycle, or a chain longer than the kernel follows: the kernel
                        // gives ELOOP here and this tool call never lands at all. What
                        // cannot be answered says so.
                        hops += 1;
                        if hops > MAX_LINK_HOPS {
                            return None;
                        }
                        // A link's text is the next stretch to walk, and its length is
                        // given by whoever wrote the link. Charge first.
                        if let Some(b) = work.as_deref_mut()
                            && !b.take_segments(target.components().count())
                        {
                            return None;
                        }
                        cur.pop();
                        // An absolute target starts over; a relative one joins onto the
                        // directory the link lives in — which is exactly the `cur` just
                        // popped back.
                        if target.is_absolute() {
                            cur = PathBuf::new();
                        }
                        for (i, c) in target.components().enumerate() {
                            rest.insert(i, (c.as_os_str().to_os_string(), true));
                        }
                    }
                    // Not a link, or this segment does not exist yet, or it cannot be asked
                    // — three answers, and `read_link`'s error codes do not separate them
                    // cleanly ("not a link" is not `EINVAL` at all on Windows), so keep
                    // asking.
                    Err(_) => match std::fs::canonicalize(&cur) {
                        // Exists and is not a link: this pass only fetches back the
                        // spelling on disk.
                        Ok(real) => cur = real,
                        // Normalization could not answer either. **Only "really not
                        // there" counts as not there**: every other case is "what this
                        // segment is cannot be judged" (see below).
                        Err(_) => match std::fs::symlink_metadata(&cur) {
                            // There, just unresolvable: permissions, `ELOOP`, swapped out
                            // after the lstat.
                            Ok(_) => return None,
                            // The whole segment does not exist yet. A missing tail in the
                            // spelling is the ordinary case of `Write` creating a plain
                            // file — spliced on unchanged, since it has no symlink to speak
                            // of; a missing segment in a link's text makes that link
                            // **dangling** (see above).
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !spliced => {}
                            Err(_) => return None,
                        },
                    },
                }
            }
        }
    }
    Some(cur)
}

#[derive(Clone, Copy, PartialEq)]
enum ArgShape {
    None,
    Opaque,
    Paths,
    PatternThenPaths,
}

/// A command that really can do nothing: read bytes, write stdout.
///
/// Three admission criteria hold at once: (1) it opens no network; (2) it forks no other
/// program; (3) it reads no config from the workspace to decide what to run. The third is why
/// **`git` is absent** — in a repo the agent can already write, `core.pager` /
/// `diff.external` / `filter.*.clean` / hooks make one `git diff` execute any program;
/// `find` (-exec), `env`, `xargs`, `sed -i` and every interpreter are the same. To let the
/// operator answer `git status`, go through the owner's granted list (`agit rc grant`), not
/// through another line here.
struct Inert {
    name: &'static str,
    /// Pure switch short flags, which may be bundled (`-la`).
    bool_short: &'static str,
    /// Short flags taking a number (`head -n 20`), allowed only as the last letter of a
    /// bundle.
    num_short: &'static str,
    /// Short flags taking a word that is **not read as a path** (`grep -e pat`,
    /// `rg -g '*.rs'`).
    word_short: &'static str,
    /// These flags **supply the pattern themselves** (`grep -e X`, `rg --regexp X`).
    ///
    /// After one of them, the first positional is no longer the pattern but a **path** — and
    /// a path position is validated while a pattern position is not (it is a regex). Without
    /// this field, `grep -rn -e TODO ~/.aws` swallows `~/.aws` as the pattern, runs no check
    /// at all, and the operator can allow reading AWS credentials into the transcript.
    pattern_flags: &'static [&'static str],
    /// Pure switch long flags.
    long: &'static [&'static str],
    /// Long flags taking a word. `--flag=value` goes through this table too.
    long_word: &'static [&'static str],
    /// Allow a bare number flag like `-20` (head / tail).
    bare_number: bool,
    args: ArgShape,
}

#[rustfmt::skip]
const INERT: &[Inert] = &[
    Inert { name: "ls", bool_short: "aAlhrtS1dFG", num_short: "", word_short: "", // `--recursive` is **not** here: it is the long spelling of `-R`, and `-R` follows
    // symlinks met during the traversal (`-r` does not, which is why the grep row keeps it).
    long: &["--all","--almost-all","--long","--human-readable","--reverse","--size","--classify","--color"], long_word: &[], bare_number: false, pattern_flags: &[], args: ArgShape::Paths },
    Inert { name: "cat", bool_short: "nbsETv", num_short: "", word_short: "", long: &["--number","--show-ends","--squeeze-blank"], long_word: &[], bare_number: false, pattern_flags: &[], args: ArgShape::Paths },
    Inert { name: "head", bool_short: "qv", num_short: "nc", word_short: "", long: &["--quiet","--verbose"], long_word: &["--lines","--bytes"], bare_number: true, pattern_flags: &[], args: ArgShape::Paths },
    Inert { name: "tail", bool_short: "qv", num_short: "nc", word_short: "", long: &["--quiet","--verbose"], long_word: &["--lines","--bytes"], bare_number: true, pattern_flags: &[], args: ArgShape::Paths },
    Inert { name: "wc", bool_short: "lwcmL", num_short: "", word_short: "", long: &["--lines","--words","--bytes","--chars"], long_word: &[], bare_number: false, pattern_flags: &[], args: ArgShape::Paths },
    Inert { name: "pwd", bool_short: "LP", num_short: "", word_short: "", long: &[], long_word: &[], bare_number: false, pattern_flags: &[], args: ArgShape::None },
    Inert { name: "echo", bool_short: "neE", num_short: "", word_short: "", long: &[], long_word: &[], bare_number: false, pattern_flags: &[], args: ArgShape::Opaque },
    // `-f` (read the pattern from a file) is deliberately on no table; `--pre` /
    // `--pre-glob` are deliberately off rg's — those are rg's only switches that fork another
    // program. The list is **per flag**, not per command.
    Inert { name: "grep", bool_short: "iIhHnrlLcoswxvFEqa", num_short: "ABCm", word_short: "e", pattern_flags: &["-e", "--regexp"], long: &["--ignore-case","--line-number","--recursive","--files-with-matches","--count","--word-regexp","--invert-match","--fixed-strings","--extended-regexp","--no-messages","--color"], long_word: &["--regexp","--after-context","--before-context","--context","--max-count","--include","--exclude"], bare_number: false, args: ArgShape::PatternThenPaths },
    Inert { name: "rg", bool_short: "inNlcoswxvFSUpH", num_short: "ABCm", word_short: "eg", pattern_flags: &["-e", "--regexp", "--files"], long: &["--ignore-case","--line-number","--files-with-matches","--count","--word-regexp","--invert-match","--fixed-strings","--hidden","--no-ignore","--json","--color","--files"], long_word: &["--regexp","--glob","--type","--type-not","--after-context","--before-context","--context","--max-count"], bare_number: false, args: ArgShape::PatternThenPaths },
];

enum Flag {
    Ok,
    WantsNumber,
    WantsWord,
    Bad,
}

fn classify_flag(spec: &Inert, tok: &str) -> Flag {
    if let Some(long) = tok.strip_prefix("--") {
        // `--type=rust`: one split is enough, and whether the value is read as a path is
        // decided by long_word.
        let (name, has_value) = match long.split_once('=') {
            Some((n, _)) => (n, true),
            None => (long, false),
        };
        let full = format!("--{name}");
        if spec.long.contains(&full.as_str()) {
            return if has_value { Flag::Bad } else { Flag::Ok };
        }
        if spec.long_word.contains(&full.as_str()) {
            return if has_value { Flag::Ok } else { Flag::WantsWord };
        }
        return Flag::Bad;
    }
    let cluster = &tok[1..];
    // `head -20` / `tail -5`
    if spec.bare_number && !cluster.is_empty() && cluster.bytes().all(|b| b.is_ascii_digit()) {
        return Flag::Ok;
    }
    let letters: String = cluster
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    if letters.is_empty() {
        return Flag::Bad; // a bare `-` is stdin, and is not recognized
    }
    let rest = &cluster[letters.len()..];
    let last = letters.chars().last().unwrap_or('\0');
    let init = &letters[..letters.len() - 1];
    // A flag that takes an argument may only be last: there is no answer to who gets the 20
    // in `-nq 20`.
    if init.chars().any(|c| !spec.bool_short.contains(c)) {
        return Flag::Bad;
    }
    if spec.bool_short.contains(last) {
        return if rest.is_empty() { Flag::Ok } else { Flag::Bad };
    }
    if spec.num_short.contains(last) {
        if rest.is_empty() {
            return Flag::WantsNumber;
        }
        return if rest.bytes().all(|b| b.is_ascii_digit()) {
            Flag::Ok
        } else {
            Flag::Bad
        };
    }
    if spec.word_short.contains(last) {
        return if rest.is_empty() {
            Flag::WantsWord
        } else {
            Flag::Ok
        };
    }
    Flag::Bad
}

/// One word, plus "was it quoted originally".
struct Tok {
    text: String,
}

/// Split a line into words, letting **one** matched pair of quotes wrap an argument that
/// carries spaces or a regex into a single word.
///
/// # Why quotes must be allowed
///
/// The metacharacter table holds `*`, `?`, `[`, `]`, `{`, `}`, while regex syntax and shell
/// glob syntax overlap almost entirely — `grep -rn "TODO" src`, `rg 'fn main' src` and
/// `rg -g '*.rs' TODO` are the shapes an agent actually writes. Scanning the whole line for
/// metacharacters makes the one capability that survives — "reading and searching the
/// workspace still belongs to the operator" — dead in practice, while its tests stay green,
/// because every command in them is written without quotes.
///
/// # A rule small enough to check at a glance
///
/// A quote may only be a word's **first byte**, and must close on the **last byte** of the
/// same word; `$`, a backtick and a backslash may not appear inside quotes. So
/// `bash -c 'curl x'` still dies on the head check, `"$(curl x)"` still dies on the `$`, and
/// `grep "a b" src` survives.
fn tokenize(cmd: &str) -> Option<Vec<Tok>> {
    let mut out = vec![];
    let mut it = cmd.chars().peekable();
    loop {
        while it.peek().is_some_and(|c| *c == ' ') {
            it.next();
        }
        let Some(&first) = it.peek() else { break };
        if first == '\'' || first == '"' {
            it.next();
            let mut body = String::new();
            loop {
                let c = it.next()?; // unclosed = cannot be judged
                if c == first {
                    break;
                }
                // **Nothing expands inside single quotes**, while `$` / backtick /
                // backslash are still live inside double quotes.
                //
                // Refusing `$`, backticks and backslashes in both kinds of quotes alike does
                // not work: **most** regexes an agent writes carry them —
                // `rg 'fn main$' src`, `rg '\bTODO\b' src`, `grep -E '^\s*fn ' src/main.rs`
                // — so the one capability that survives, "reading and searching the
                // workspace still belongs to the operator", is dead in practice, while a set
                // of cases without those two characters stays green and shows nothing.
                //
                // The double-quoted half cannot be let through: `"$(curl x)"` is substituted
                // by a real shell.
                if first == '"' && matches!(c, '$' | '`' | '\\') {
                    return None;
                }
                if matches!(c, '\n' | '\r') {
                    return None;
                }
                body.push(c);
            }
            // A closing quote must be followed by a word boundary; `'a'b` is not
            // recognized.
            if it.peek().is_some_and(|c| *c != ' ') {
                return None;
            }
            out.push(Tok { text: body });
        } else {
            let mut body = String::new();
            while let Some(&c) = it.peek() {
                if c == ' ' {
                    break;
                }
                if SHELL_META.contains(&c) || c == '\'' || c == '"' {
                    return None;
                }
                body.push(c);
                it.next();
            }
            out.push(Tok { text: body });
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Walk all the arguments once and settle **whether the positionals are paths**.
///
/// `PatternThenPaths` drops to `Paths` (that is, every positional must be validated) in two
/// cases:
///
/// * a flag has already supplied the pattern (`-e` / `--regexp`, including a bundle like
///   `-rne` and an attached value like `-e.`);
/// * a flag leaves the command with no pattern operand at all (`rg --files`).
///
/// **Scan to the end before settling.** Real grep / rg reorder options in front of the
/// operands, so `/etc` in `grep -r /etc -e TODO` is a search path and `TODO` is the pattern —
/// settling as it goes, `/etc` is swallowed as the pattern before `-e` ever appears.
fn prescan_shape(spec: &Inert, args: &[Tok]) -> ArgShape {
    if spec.args != ArgShape::PatternThenPaths {
        return spec.args;
    }
    let mut skip_next = false;
    for t in args {
        if skip_next {
            // This is the value the previous flag consumed. It may itself start with `-`
            // (`grep -e -v file`), so it must be skipped rather than parsed as a flag
            // again.
            skip_next = false;
            continue;
        }
        if !t.text.starts_with('-') {
            // What follows the first positional has more than one reading (see the
            // `POSIXLY_CORRECT` passage in `confined_command`). The prescan stops here, and
            // the main loop refuses the rest.
            return spec.args;
        }
        if supplies_pattern(spec, &t.text) {
            return ArgShape::Paths;
        }
        skip_next = matches!(
            classify_flag(spec, &t.text),
            Flag::WantsWord | Flag::WantsNumber
        );
    }
    spec.args
}

/// Does this flag supply the pattern (or remove the pattern operand outright)?
///
/// The comparison is against the **decoded** form, not the whole token: the last `e` of
/// `-rne` supplies the pattern just the same, `-e.` attaches the value behind it, and
/// `--regexp=x` uses an equals sign — none of the three equals `"-e"`, while all three mean
/// exactly the same to the real tool.
fn supplies_pattern(spec: &Inert, tok: &str) -> bool {
    if let Some(long) = tok.strip_prefix("--") {
        let name = long.split_once('=').map_or(long, |(n, _)| n);
        return spec
            .pattern_flags
            .iter()
            .any(|f| f.strip_prefix("--") == Some(name));
    }
    // Short flags: only the run of letters matters, and only its last letter can take an
    // argument.
    let cluster = &tok[1..];
    let letters: String = cluster
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let Some(last) = letters.chars().last() else {
        return false;
    };
    spec.pattern_flags
        .iter()
        .filter_map(|f| f.strip_prefix('-'))
        .filter(|f| !f.starts_with('-'))
        .any(|f| f.starts_with(last))
}

fn confined_command(
    cmd: &str,
    roots: &CanonicalRoots,
    cwd: &Path,
    granted_heads: &std::collections::BTreeSet<String>,
) -> Result<(), crate::protocol::OwnerReason> {
    confined_command_in(
        cmd,
        roots,
        cwd,
        granted_heads,
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`confined_command`], with `PATH` given by the caller.
///
/// This seam exists because one step of the test **resolves the command name on PATH** (the
/// list recognizes the system's `ls`, not the one the agent just wrote into the repo). That
/// step makes the conclusion depend on what is installed on this machine — which is what CI
/// runs into: where `rg` is not installed, `rg 'fn main' src` is judged `Unprovable`, green
/// locally and red in CI.
///
/// The behavior itself is right (on a machine where the command cannot run, judging it
/// unprovable is fair), but **a test must not read the developer machine's toolchain**. So
/// tests build a PATH of their own.
fn confined_command_in(
    cmd: &str,
    roots: &CanonicalRoots,
    cwd: &Path,
    granted_heads: &std::collections::BTreeSet<String>,
    path_env: Option<&std::ffi::OsStr>,
) -> Result<(), crate::protocol::OwnerReason> {
    confined_command_with(
        cmd,
        roots,
        cwd,
        granted_heads,
        path_env,
        pathext().as_deref(),
    )
}

/// [`confined_command_in`], with the extension table given by the caller as well.
///
/// The seam is here for the same reason as in [`resolve_in_path_with`], and it matters more:
/// getting the extension expansion wrong is not as light as "resolving to another file", it
/// **flips the conclusion** — a `tool.exe.cmd` in an external directory displaces the
/// `tool.exe` in the workspace that the shell really executes, and the test allows a program
/// the agent can write as an external tool the owner granted. `PATHEXT` exists only on
/// Windows, and behind a `#[cfg]` not one line of this machine's tests reaches it, so the
/// flip is silent.
fn confined_command_with(
    cmd: &str,
    roots: &CanonicalRoots,
    cwd: &Path,
    granted_heads: &std::collections::BTreeSet<String>,
    path_env: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> Result<(), crate::protocol::OwnerReason> {
    use crate::protocol::OwnerReason::{Escalates, Unprovable};
    let cmd = cmd.trim();
    if cmd.is_empty() || cmd.len() > 512 {
        return Err(Unprovable);
    }
    let Some(toks) = tokenize(cmd) else {
        return Err(Unprovable);
    };
    let head = &toks[0];
    // The command name must be a **bare name**: one with a `/` (`/usr/bin/curl`, `./x`) is
    // not recognized, and neither is a quoted one (`'ls'` is dodging the check).
    if !crate::rc::grants::is_bare_command_name(&head.text) {
        return Err(Unprovable);
    }
    // The list recognizes **the system's `ls`**, not the one the agent just wrote into the
    // repo. The direction here is the opposite of everywhere else: an executable **inside**
    // the allowlist is the suspicious one.
    let Some(bin) = resolve_in_path_with(&head.text, path_env, pathext) else {
        return Err(Unprovable);
    };
    // With the direction reversed, "cannot be judged" has to land the other way too: if we
    // cannot say where this executable is, it may not stand in for "the system's `ls`".
    let Some(at) = resolve_existing_ancestor(&bin) else {
        return Err(Unprovable);
    };
    if within(&at, roots) {
        return Err(Escalates);
    }

    let Some(spec) = INERT.iter().find(|s| s.name == head.text) else {
        // A name the owner granted: we know nothing about its arguments, so all that is
        // guaranteed is "it is that name, and the whole line has no metacharacter, no
        // substitution and no pipe". That is exactly what an owner's grant means.
        return if granted_heads.contains(&head.text) {
            Ok(())
        } else {
            Err(Unprovable)
        };
    };

    // **`rg --files` has no pattern operand** — the first positional is the search path.
    //
    // rg is always `PatternThenPaths` in the table, so `rg --files /etc` swallows `/etc` as
    // the pattern (a pattern position validates no path, it is a regex) and no path check
    // runs at all. The operator could allow one enumeration of a directory outside the
    // workspace.
    //
    // The test looks at this one flag: it is the only switch in this table that changes the
    // **argument shape**.
    // **The shape must be settled after one scan of all the arguments, never as it goes.**
    //
    // Real grep / rg **reorder options in front of the operands**: in
    // `grep -r /etc -e TODO` the pattern is TODO and the search path is /etc. And if
    // `seen_pattern` is only set inside the same left-to-right loop, the path operand written
    // **before** `-e` falls into the pattern position first — and a pattern position
    // validates no path — so no check runs at all.
    //
    // The test also has to use the **decoded** flag, not string equality: the `e` in `-rne`
    // supplies the pattern just the same, and so does `-e.`, while neither equals `"-e"`.
    let shape = prescan_shape(spec, &toks[1..]);

    // With shape `Paths` the first positional is a path too — set `seen_pattern` right
    // away.
    let mut seen_pattern = shape == ArgShape::Paths;
    // Whether a positional has been seen. Afterwards a following `-...` has more than one
    // reading (see the passage below).
    let mut seen_pattern_or_path = false;
    let mut expect_number = false;
    let mut expect_word = false;
    for t in &toks[1..] {
        if expect_number {
            if !t.text.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Unprovable);
            }
            expect_number = false;
            continue;
        }
        if expect_word {
            expect_word = false;
            continue; // not read as a path: a regex, a glob, a type name
        }
        // **Quotes do not change what the program sees.** Once the shell strips the quotes
        // from `grep "-R" TODO src`, grep still receives `-R`. Skipping quoted tokens with
        // `!t.quoted` turns them into positionals — which is how `rg "--pre" "curl ..." TODO`
        // gets around the flag list. A quote's only effect is to disarm shell
        // metacharacters, and that was already handled in tokenize.
        // **A `-...` appearing after a positional cannot be judged, uniformly.**
        //
        // "Options are reordered in front of the operands" is a GNU **extension**, not a
        // guarantee: with `POSIXLY_CORRECT` set, GNU grep explicitly treats an option after a
        // file name as a file name, and a harness child process inherits the daemon's
        // environment. So one command line has two readings — whether `/etc/passwd` in
        // `grep root src -e /etc/passwd` is the pattern or a file to read depends on an
        // environment variable we do not control.
        //
        // A machine-side check must not rest on "that variable happens to be unset". Two
        // readings that disagree means it cannot be judged. The normal spelling (flags first)
        // is unaffected.
        if seen_pattern_or_path && t.text.starts_with('-') {
            return Err(Unprovable);
        }
        if t.text.starts_with('-') {
            // Whether a flag supplies the pattern is settled once by `prescan_shape` on the
            // decoded flag form; comparing raw strings again here would miss `-rne` / `-e.`
            // and would create a second set of rules.
            match classify_flag(spec, &t.text) {
                Flag::Bad => return Err(Unprovable),
                Flag::Ok => {}
                Flag::WantsNumber => expect_number = true,
                Flag::WantsWord => expect_word = true,
            }
            continue;
        }
        seen_pattern_or_path = true;
        match shape {
            ArgShape::None => return Err(Unprovable),
            ArgShape::Opaque => {}
            ArgShape::PatternThenPaths if !seen_pattern => seen_pattern = true,
            ArgShape::Paths | ArgShape::PatternThenPaths => {
                // A path position still accepts no wildcard: who expands `ls src/*.rs`,
                // and into what, is not something we can answer.
                if t.text.chars().any(|c| SHELL_META.contains(&c)) {
                    return Err(Unprovable);
                }
                confined_read_path(&t.text, roots, cwd)?;
            }
        }
    }
    if expect_number || expect_word {
        return Err(Unprovable);
    }
    Ok(())
}

/// Which real file `head` resolves to on PATH.
///
/// PATH is passed in as an argument rather than read from the environment so a test can pin
/// "an `ls` planted in the repo is not the system's `ls`" — changing a process environment
/// variable is a global side effect.
///
/// Only tests use it: the production side ([`shell_command_is_provably_external`]) passes the
/// extension table down itself, because it reuses that table within the same decision. This
/// wrapper stays because most tests do not care about extensions, and
/// `resolve_in_path("ls", ..)` reads more clearly than adding `pathext().as_deref()`
/// everywhere.
#[cfg(test)]
fn resolve_in_path(head: &str, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    resolve_in_path_with(head, path_env, pathext().as_deref())
}

/// [`resolve_in_path`], with the table of "what a bare name is called on disk" given by the
/// caller as well.
///
/// The seam is here for the same reason as the `path_env` one: if the whole expansion rule
/// compiles only on Windows, not one line of this machine's tests reaches it, and getting it
/// wrong makes **the whole test fail silently** — see [`candidate_names`].
fn resolve_in_path_with(
    head: &str,
    path_env: Option<&std::ffi::OsStr>,
    pathext: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    // Only **executable** candidates count, matching the shell's resolution rule. Looking at
    // `is_file()` alone, a plain file of the same name in some earlier PATH directory (a note
    // called `ls`, say) stops the test right there, while the shell skips it and executes the
    // real executable behind it — which may be a script the agent just wrote into the
    // workspace. The test then says "this is the system's `ls`" about a file that will never
    // be executed, and that direction is allow.
    //
    // Directories outside, extensions inside: cmd also tries every `PATHEXT` in one directory
    // before moving to the next. Written the other way round, a `.exe` in a later PATH
    // directory would beat a `.cmd` in an earlier one.
    let path_env = path_env?;
    let names = candidate_names(head, pathext);
    for d in std::env::split_paths(path_env) {
        // **A relative PATH entry is judged "cannot be resolved" on the spot.**
        //
        // POSIX says an empty entry means "the current directory", and
        // `PATH="$MAYBE_UNSET:$PATH"` is written everywhere — what the daemon and every
        // harness child process it starts inherit is `:/usr/bin:/bin`. `split_paths` hands an
        // empty entry back as `""`, so `d.join("ls")` is the relative path `ls`, and
        // `is_executable_file` stats it relative to **agitd's own process cwd** — which has
        // nothing to do with the cwd the shell would use (the session's cwd), and this
        // function does not hold the session cwd at all.
        //
        // Both directions can be wrong, and the allow direction is fatal: the agent writes an
        // executable `ls` into the workspace (a Write the operator alone can allow) and then
        // asks for `ls -la src`. bash resolves that leading empty entry to the session cwd
        // and executes the agent's `ls`; the test does not find it under agitd's cwd, falls
        // all the way through to `/usr/bin/ls`, and says "this is the system's `ls`" about a
        // file that will never be executed. The list recognizes the system's `ls`, and this
        // is exactly the case where it cannot — so stop, and hand it to the owner.
        if !d.is_absolute() {
            return None;
        }
        if let Some(hit) = names
            .iter()
            .map(|n| d.join(n))
            .find(|c| is_executable_file(c))
        {
            return Some(canonical(&hit));
        }
    }
    None
}

/// Which file names the shell tries, in order, inside one PATH directory.
///
/// With `pathext` as `None` (Unix) there is only the bare name — extensions are not a thing
/// there.
///
/// Not expanding on Windows makes **the whole test fail**: what lies on disk is `git.exe`,
/// `npm.cmd`, `rg.exe`, joining a bare name matches none of them, `resolve_in_path` returns
/// `None` uniformly, and every shell command is judged `Unprovable` and handed to the owner —
/// the INERT list and every `agit rc grant` the owner approved might as well not exist on
/// that machine.
///
/// The other way round, a bare name is **not** a candidate on Windows: cmd and PowerShell
/// execute only files carrying a `PATHEXT` extension, and an extensionless file earlier on
/// PATH that happens to be called `git` (which the agent can write) must not stop the
/// resolution there — stopping means concluding about a file that will never be executed,
/// while what really runs is the `git.exe` behind it.
///
/// **When the name carries an extension itself, the only candidate is the literal name.**
/// `PATHEXT` supplements a command with **no** extension written (this is also what
/// `where /?` says): type `tool.exe` and cmd looks for `tool.exe`, never falling back to
/// `tool.exe.cmd`. Appending anyway **flips the conclusion**: a `tool.exe.cmd` in an external
/// directory earlier on PATH matches first, the resolution lands outside the workspace ⇒ the
/// test reads it as an external tool the owner granted and allows it; while on the shell's
/// side that external directory holds no `tool.exe` at all, and what really runs is the one
/// in the workspace that the agent can write.
///
/// **"Carries an extension" is a purely syntactic judgment, not "the extension is on the
/// `PATHEXT` list".** The documentation says an extension is appended when none is specified,
/// not when none from the list is specified — narrowing it to the latter is wrong, and the
/// counterexample sits on a machine whose `PATHEXT` lacks `.EXE`: `PATHEXT=.CMD`, an external
/// directory earlier on PATH holding `tool.exe.CMD`, and the workspace later on PATH holding
/// `tool.exe`. Judged by the list, `.exe` is not on it ⇒ keep appending ⇒ the external
/// `tool.exe.CMD` matches first ⇒ allowed as an external tool; while cmd sees a command with
/// an extension written, looks straight for `tool.exe`, and runs the one in the workspace
/// that the agent can write.
///
/// The reverse direction (an extension not on the list, like `my.tool`) is not the same kind
/// of risk: cmd likewise looks only for the literal `my.tool` and hands what it finds to
/// CreateProcess — an invalid image errors out, and it does **not** fall back to
/// `my.tool.cmd`. So wherever the literal name resolves is what runs (or nothing runs at
/// all), and test and execution do not diverge.
fn candidate_names(head: &str, pathext: Option<&std::ffi::OsStr>) -> Vec<String> {
    let Some(pathext) = pathext else {
        return vec![head.to_string()];
    };
    let pathext = pathext.to_string_lossy();
    let exts = || {
        pathext
            .split(';')
            .map(str::trim)
            .filter(|e| e.len() > 1 && e.starts_with('.'))
    };
    // By here `head` is a bare command name (`is_bare_command_name`) with no path separator,
    // so `Path::extension` asks about the last piece of this name itself.
    if Path::new(head).extension().is_some() {
        return vec![head.to_string()];
    }
    exts().map(|e| format!("{head}{e}")).collect()
}

/// `PATHEXT` decides "which file on disk a bare name means" on Windows only.
#[cfg(windows)]
fn pathext() -> Option<std::ffi::OsString> {
    // Unset must not fall back to "try the bare name only": that voids this machine's INERT
    // list along with every grant. The backstop is cmd's own built-in default.
    Some(std::env::var_os("PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into()))
}

#[cfg(not(windows))]
fn pathext() -> Option<std::ffi::OsString> {
    None
}

/// Is a path "the kind of file the shell really executes"?
///
/// The question is `access(X_OK)` — may **this process** execute it — rather than "is any
/// execute bit set". Where the two disagree is exactly where the test misreads: a root-owned
/// `0o700` file earlier on PATH has the bit set while the daemon's user gets EACCES, and the
/// shell skips it and keeps looking. Judged by the bit, the resolution stops there and says
/// "this is the system's `rg`" about a file that will never be executed — the direction is
/// allow, while what really runs may be a script of the same name the agent just wrote into
/// the workspace.
#[cfg(unix)]
fn is_executable_file(c: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !c.is_file() {
        return false;
    }
    // A path with a NUL cannot be opened anyway; treat it as not executable.
    let Ok(path) = std::ffi::CString::new(c.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: the pointer comes from the live `CString` above (NUL-terminated), and `X_OK`
    // is a constant. agitd is not setuid, so `access`'s real uid is the uid of the harness
    // child process.
    unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
}

/// Windows has no execute bit (executability is decided by `PATHEXT`, see
/// [`candidate_names`]), the candidate name already carries its extension by here, and what
/// is left is "does the file exist".
#[cfg(not(unix))]
fn is_executable_file(c: &Path) -> bool {
    c.is_file()
}

pub fn require_dir_under_home(target: &Path) -> Result<PathBuf, PolicyError> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Err(PolicyError::OutsideHome(target.display().to_string()));
    };
    require_dir_under(target, &home)
}

/// The half where the home directory is given by the caller.
///
/// "Where home is" is process-level and the test is not — separating them is what makes the
/// test checkable: the process environment is one slot shared by the whole binary, and a test
/// changing it writes it while another thread reads it (every `Command::spawn` does).
pub fn require_dir_under(target: &Path, home: &Path) -> Result<PathBuf, PolicyError> {
    // The path judged inside or outside the home directory and the path the caller then
    // lists must be one path.
    //
    // `is_under_home` answers only yes or no and drops the path it judged; computing another
    // one with [`canonical`] makes the two halves resolve one spelling by different rules —
    // and `canonical` falls back to a lexical fold when resolution fails, where a folded `..`
    // at the root is a no-op and can climb somewhere the resolver would never go. The test
    // then looks at a path inside home while the picker opens `/etc`.
    let roots = CanonicalRoots::from_untrusted([home.to_path_buf()]);
    let judged = require_within(target, &roots)
        .map_err(|_| PolicyError::OutsideHome(target.display().to_string()))?;
    if !judged.is_dir() {
        return Err(PolicyError::NotADirectory(target.display().to_string()));
    }
    Ok(judged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The denylist must not be defeated by a **symlink**.
    ///
    /// This pins a real platform-wide hole that disguises itself as "the test has an
    /// environment assumption": the list spells `/etc` while the comparison is against the
    /// path after `canonical()`, and on macOS `/etc → /private/etc`. Without this, the
    /// `/etc`, `/var/lib` and `/var/run` guardrails fail silently across the whole platform —
    /// and the direction of the failure is **allow**.
    ///
    /// Both directions are checked here: the literal path on the list and its resolved real
    /// name must both be stopped.
    #[test]
    fn the_never_bind_list_is_not_defeated_by_symlinks() {
        for d in ["/", "/etc", "/usr", "/dev"] {
            let p = Path::new(d);
            if !p.exists() {
                continue; // Linux and macOS roots differ; skip what does not exist.
            }
            assert!(
                matches!(
                    require_bindable_dir(p),
                    Err(PolicyError::SystemDirectory(_))
                ),
                "{d} must be refused"
            );
            // The real name must be refused too: a user can type `/private/etc` directly.
            if let Ok(real) = p.canonicalize()
                && real != p
            {
                assert!(
                    matches!(
                        require_bindable_dir(&real),
                        Err(PolicyError::SystemDirectory(_))
                    ),
                    "the real name {} of {d} must be refused too — getting around it takes \
                     a few more characters",
                    real.display()
                );
            }
        }
    }

    #[test]
    fn a_project_can_live_outside_home_but_not_at_a_system_root() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("srv-style-project");
        std::fs::create_dir_all(&proj).unwrap();

        // /tmp is not under $HOME, and it is a perfectly legitimate place for a project —
        // pinning binds to $HOME makes every real repo in /srv, /opt or /workspace
        // unusable.
        assert!(require_bindable_dir(&proj).is_ok());
        assert!(
            require_dir_under_home(&proj).is_err(),
            "the picker still browses $HOME only"
        );

        // Handing over a system root whole is another matter.
        assert!(matches!(
            require_bindable_dir(Path::new("/")),
            Err(PolicyError::SystemDirectory(_))
        ));
        assert!(matches!(
            require_bindable_dir(Path::new("/etc")),
            Err(PolicyError::SystemDirectory(_))
        ));

        // A path that does not exist must say "does not exist" rather than refuse vaguely —
        // the error message has to be actionable.
        assert!(matches!(
            require_bindable_dir(&tmp.path().join("nope")),
            Err(PolicyError::Missing(_))
        ));
        // A file is not a directory.
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(matches!(
            require_bindable_dir(&f),
            Err(PolicyError::NotADirectory(_))
        ));
    }

    #[test]
    fn dotdot_and_symlinks_do_not_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let roots = CanonicalRoots::from_untrusted([root.clone()]);
        assert!(is_within(&root.join("sub"), &roots));
        assert!(is_within(&root.join("sub/../sub"), &roots));
        assert!(!is_within(&root.join("../outside"), &roots));
        #[cfg(unix)]
        assert!(
            !is_within(&root.join("escape"), &roots),
            "symlink out of root must be rejected"
        );
        assert!(!is_within(&outside, &roots));
    }

    #[cfg(unix)]
    #[test]
    fn approval_checks_use_the_bound_root_proof_without_following_a_later_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let bound = tmp.path().join("bound");
        let moved = tmp.path().join("moved");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&bound).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "nope").unwrap();

        // This is the bind/adopt boundary: a legacy spelling is safely resolved once.
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&bound, &alias).unwrap();
        let roots = CanonicalRoots::from_untrusted([alias]);
        assert!(is_within(&bound, &roots));

        // After binding, replacing the old spelling with a symlink must not expand authority.
        // Re-canonicalizing roots inside every `is_within` would now turn the allowlist into
        // `outside` and let the production approval path return `None` (operator-answerable).
        std::fs::rename(&bound, &moved).unwrap();
        std::fs::remove_file(tmp.path().join("alias")).unwrap();
        std::os::unix::fs::symlink(&outside, &bound).unwrap();
        assert_eq!(
            approval_owner_reason(
                "Read",
                &serde_json::json!({ "file_path": outside.join("secret") }),
                &roots,
                &outside,
                &std::collections::BTreeSet::new(),
            ),
            Some(crate::protocol::OwnerReason::Unprovable),
            "a later symlink must not retarget a previously verified allowlist root"
        );
    }

    // ─────────────────── Approval classifier ──────────────────

    use crate::protocol::OwnerReason;
    use std::collections::BTreeSet;

    /// A workspace that really exists: `<tmp>/ws`, holding `src/main.rs` and
    /// `.git/config`.
    fn workspace() -> (tempfile::TempDir, CanonicalRoots, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let ws = d.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        let ws = canonical(&ws);
        (d, CanonicalRoots::from_verified(vec![ws.clone()]), ws)
    }

    /// A file the shell really executes (on Unix it also needs the execute bit).
    fn write_executable(p: &Path) {
        std::fs::write(p, "#!/bin/sh\ntrue\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn reason(tool: &str, input: serde_json::Value) -> Option<OwnerReason> {
        let (_d, roots, cwd) = workspace();
        approval_owner_reason(tool, &input, &roots, &cwd, &BTreeSet::new())
    }

    #[test]
    fn an_exec_relative_path_is_resolved_from_the_exec_approval_cwd() {
        let (_d, roots, workspace) = workspace();
        let session_cwd = workspace.join("api/src");
        std::fs::create_dir_all(&session_cwd).unwrap();

        // From the session cwd this would look like `<workspace>/api/.ssh/id_rsa` and pass.
        // The process will actually run at `<workspace>`, where the same spelling escapes to the
        // workspace's parent. The approval's cwd must therefore be the classifier's cwd too.
        assert_eq!(
            approval_owner_reason(
                "shell",
                &serde_json::json!({
                    "command": "cat ../.ssh/id_rsa",
                    "cwd": workspace,
                }),
                &roots,
                &session_cwd,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates)
        );

        assert_eq!(
            approval_owner_reason(
                "shell",
                &serde_json::json!({
                    "command": "cat ../src/main.rs",
                    "cwd": workspace.join("api"),
                }),
                &roots,
                &session_cwd,
                &BTreeSet::new(),
            ),
            None,
            "a relative path that stays inside when resolved from the exec cwd remains answerable"
        );
    }

    #[test]
    fn a_malformed_exec_cwd_is_not_treated_as_if_it_were_absent() {
        let (_d, roots, cwd) = workspace();
        assert_eq!(
            approval_owner_reason(
                "shell",
                &serde_json::json!({ "command": "cat src/main.rs", "cwd": 7 }),
                &roots,
                &cwd,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Unprovable)
        );
    }

    /// A `PATH` **built here**, holding empty executables under these names.
    ///
    /// One step of the test resolves the command name on PATH (the list recognizes the
    /// system's `ls`, not the one the agent just wrote into the repo), so the conclusion
    /// depends on what is installed on this machine. Where `rg` is not installed in CI,
    /// `rg 'fn main' src` is judged `Unprovable` — green locally, red in CI.
    ///
    /// The behavior itself is right: on a machine that cannot run `rg`, judging it unprovable
    /// is fair. But a test must not read the developer machine's toolchain, so it builds one
    /// here.
    fn fake_path(names: &[&str]) -> (tempfile::TempDir, std::ffi::OsString) {
        let d = tempfile::tempdir().unwrap();
        for n in names {
            let p = d.path().join(n);
            std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let s = d.path().as_os_str().to_os_string();
        (d, s)
    }

    const TOOLBOX: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "pwd", "echo", "grep", "rg", "curl", "wget", "python",
        "bash", "sh", "node", "git", "pip", "npm", "cargo", "make", "sudo", "true", "base64",
    ];

    fn bash(cmd: &str) -> Option<OwnerReason> {
        let (_d, roots, cwd) = workspace();
        let (_p, path) = fake_path(TOOLBOX);
        confined_command_in(cmd, &roots, &cwd, &BTreeSet::new(), Some(path.as_os_str())).err()
    }

    /// **The default must be "back to the owner".**
    ///
    /// A denylist of dangerous words means whatever it fails to recognize counts as safe.
    /// Every line in this set gets around such a list, and every one of them reaches the agent
    /// outside the workspace.
    #[test]
    fn every_way_around_the_old_denylist_now_lands_on_the_owner() {
        for cmd in [
            // wrapped in an interpreter
            r#"python -c "import urllib.request; urllib.request.urlopen('http://x')""#,
            "bash -c 'curl http://x'",
            "sh -c 'curl http://x'",
            "node -e 'require(\"http\").get(\"http://x\")'",
            // not in head position
            "echo hi; curl http://x",
            "true && curl http://x",
            "ls && wget http://x",
            // an absolute path as the head
            "/usr/bin/curl http://x",
            "/bin/sh -c 'curl http://x'",
            // command substitution
            "echo $(curl http://x)",
            "echo `curl http://x`",
            // never on a denylist at all
            "git fetch origin",
            "pip install evil",
            "npm i evil",
            "cargo install evil",
            "make deploy",
            "echo Y3VybCB4 | base64 -d | sh",
            // privilege escalation
            "sudo rm -rf /",
            "true; sudo id",
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
    }

    /// Reading and searching the workspace stays with the operator — otherwise that role is
    /// gone in practice.
    ///
    /// **Every line is written the way an agent really writes it, quotes and all.** Written
    /// without quotes this set stays green, while in the real world the quotes in
    /// `rg 'fn main' src` are full of metacharacters.
    #[test]
    fn reading_and_searching_inside_the_workspace_stays_with_the_operator() {
        for cmd in [
            "ls",
            "ls -la src",
            "cat src/main.rs",
            "head -n 20 src/main.rs",
            "tail -5 src/main.rs",
            "wc -l src/main.rs",
            "pwd",
            "echo hello world",
            "grep -rn \"TODO\" src",
            "rg 'fn main' src",
            "rg -g '*.rs' TODO",
            "rg --type=rust TODO",
            "grep -e 'a b' src/main.rs",
        ] {
            assert_eq!(bash(cmd), None, "`{cmd}` must stay with the operator");
        }
    }

    /// The list is **per flag**: the same command with another switch forks another
    /// program.
    #[test]
    fn a_flag_that_forks_another_program_is_not_covered_by_its_commands_entry() {
        for cmd in [
            "rg --pre /tmp/x TODO src", // rg's only switch that starts a child process
            "rg --pre-glob '*' TODO",
            "grep -f /etc/passwd src", // reads the pattern from elsewhere
            "ls --color=always",       // a long flag must not take a value
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
    }

    /// `rg --files` has no pattern operand — its first positional is the **search path**.
    ///
    /// rg is always `PatternThenPaths` in the table, so `rg --files /etc` swallows `/etc` as
    /// the pattern, and a pattern position validates no path (it is a regex).
    #[test]
    fn rg_files_has_no_pattern_operand_so_its_first_argument_is_a_path() {
        assert!(
            bash("rg --files /etc").is_some(),
            "a path outside the workspace must be refused"
        );
        assert!(bash("rg --files ../..").is_some());
        // Inside the workspace, as usual.
        assert_eq!(bash("rg --files src"), None);
        assert_eq!(bash("rg --files"), None);
        // Without --files the first positional is still the pattern, not read as a path.
        assert_eq!(bash("rg TODO src"), None);
    }

    /// Brace matching **counts depth**; it does not take the first `}`.
    ///
    /// The first `}` of `{a,{,/}etc}` closes the inner one: the alternatives cut by it hold
    /// no `/etc` while the real expansion does — so that Glob is judged a relative path and
    /// allowed outright.
    #[test]
    fn nested_braces_are_matched_by_depth_not_by_the_first_closing_brace() {
        for pat in ["{a,{,/}etc}", "{x,{y,{z,/etc}}}", "{,{,/}etc}/**"] {
            assert_eq!(
                reason("Glob", serde_json::json!({ "pattern": pat })),
                Some(OwnerReason::Escalates),
                "glob `{pat}`"
            );
        }
        // An unclosed brace cannot be read — and what cannot be read must not count as
        // safe.
        assert_eq!(
            reason("Glob", serde_json::json!({ "pattern": "{a,b" })),
            Some(OwnerReason::Escalates)
        );
        // Ordinary nesting, as usual.
        assert_eq!(
            reason(
                "Glob",
                serde_json::json!({ "pattern": "src/{a,{b,c}}/**/*.rs" })
            ),
            None
        );
    }

    /// The argument-shape decision must use the **same** unquoting semantics as the argument
    /// loop.
    ///
    /// With quoted flags recognized in the main loop but not in the `rg --files` shape
    /// decision, `rg "--files" /etc` falls back to `PatternThenPaths` and swallows `/etc` as
    /// the pattern.
    #[test]
    fn a_quoted_files_flag_still_changes_rgs_argument_shape() {
        assert!(bash(r#"rg "--files" /etc"#).is_some());
        assert!(bash(r#"rg '--files' ../.."#).is_some());
        assert_eq!(bash(r#"rg "--files" src"#), None);
    }

    /// "Inside the workspace" does not mean "confined".
    ///
    /// `.git/config` is inside the workspace, and agitd itself runs git in that directory on
    /// every settlement — one line of `core.fsmonitor` makes the daemon execute any command
    /// on your behalf, with no next approval.
    #[test]
    fn writing_to_a_control_surface_inside_the_workspace_still_needs_the_owner() {
        for p in [
            ".git/config",
            ".git/hooks/pre-commit",
            ".claude/settings.json",
            ".agit.toml",
            ".envrc",
            ".github/workflows/ci.yml",
            "node_modules/.bin/tsc",
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "writing `{p}`"
            );
        }
        // Reading them is not the problem; writing is.
        assert_eq!(
            reason("Read", serde_json::json!({ "file_path": ".git/config" })),
            None
        );
    }

    /// **When a flag supplies the pattern, the first positional is a path.**
    ///
    /// `-e` / `--regexp` take the pattern off the positionals, and `rg --files` leaves the
    /// command with no pattern operand at all. A pattern position **validates no path** (it
    /// is a regex), so `grep -rn -e TODO ~/.aws` runs no check at all — and the operator can
    /// allow reading AWS credentials into the transcript.
    ///
    /// Only the **first** positional slips through (the second takes a path position), which
    /// is exactly why it hides so well.
    #[test]
    fn a_pattern_given_by_a_flag_makes_the_first_positional_a_path() {
        for cmd in [
            "grep -rn -e TODO /etc",
            "grep --regexp=TODO /etc/passwd",
            "grep --regexp TODO /etc/passwd",
            "grep -e TODO -r /etc",
            "rg -e TODO /etc",
            "rg --regexp TODO /etc/passwd",
            "rg -g x -e TODO /etc",
            "rg --hidden --no-ignore -e TODO /etc",
            "rg --files /",
            "rg --json --files /",
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
        // Inside the workspace, allowed as usual.
        for cmd in ["grep -rn -e TODO src", "rg -e TODO src", "rg --files src"] {
            assert_eq!(bash(cmd), None, "`{cmd}`");
        }
    }

    /// The control-surface comparison must be **case-insensitive**.
    ///
    /// APFS on macOS is case-insensitive by default: `.CLAUDE/settings.json` and
    /// `.claude/settings.json` are one directory on disk, and while the directory does not
    /// exist yet `resolve_existing_ancestor` keeps the case the attacker wrote. Once written,
    /// Claude Code reads it as `.claude/` all the same, and that hook executes on the agent's
    /// next tool call.
    #[test]
    fn a_control_surface_spelled_in_another_case_is_still_a_control_surface() {
        for p in [
            ".CLAUDE/settings.json",
            ".Claude/settings.json",
            ".GIT/config",
            ".HUSKY/pre-commit",
            ".ENVRC",
            ".GITHUB/workflows/ci.yml",
            ".VSCODE/tasks.json",
            ".AGIT.toml",
            ".Agit.toml",
            "node_modules/.BIN/tsc",
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "writing `{p}`"
            );
        }
    }

    /// The path separator on Windows is a backslash.
    ///
    /// A `node_modules/.bin` test that matches the literal `/` as a substring is walked past
    /// entirely by the `node_modules\.bin\tsc` the harness reports on Windows — and whatever
    /// is in `.bin` is executed as a tool by the next `npx` / `npm run`, allowed by the
    /// operator.
    #[test]
    fn a_backslash_spelled_node_modules_bin_is_still_a_control_surface() {
        for p in ["node_modules\\.bin\\tsc", "pkg\\node_modules\\.BIN\\tsc"] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "writing `{p}`"
            );
        }
    }

    /// What Win32 strips is the trailing `.` and spaces of **every segment**, not just the
    /// ones at the end of the whole path.
    ///
    /// [`fold_name`] strips the end of the run it was handed, and handing the
    /// `node_modules/.bin` test the **whole path** at once leaves the middle `.` of
    /// `node_modules./.bin/tsc` in place, unrecognized — while what Win32 opens is
    /// `node_modules/.bin/tsc`: whatever is installed there is run as a tool by the next
    /// `npx` / `npm run`, allowed by the operator.
    ///
    /// **Recognize it even before the directory exists.** This test looks only at names, and
    /// this write is the one that would conjure `node_modules/.bin/` into being — recognizing
    /// it once it exists is too late.
    #[test]
    fn a_trailing_dot_inside_the_path_still_spells_node_modules_bin() {
        for p in [
            "node_modules./.bin/tsc",
            "node_modules /.bin/tsc",
            "node_modules/.bin./tsc",
            "node_modules. /.bin/tsc",
            "pkg/node_modules.\\.BIN\\tsc",
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "writing `{p}`"
            );
        }
    }

    /// **A segment the kernel drops before it walks the path must not separate the
    /// `node_modules/.bin` pair.**
    ///
    /// To the kernel, the spellings `//`, `.` and `seg/..` open the same file as
    /// `node_modules/.bin/tsc`, while a test matching literal substrings is split apart by
    /// each of them. `.` hides best: it folds to nothing left, so `node_modules/./.bin/tsc`
    /// spelled back is `node_modules//.bin/tsc`.
    ///
    /// The `Write` here is the **first stroke** that creates that chain of directories: the
    /// reverse enumeration recognizes this alias only once `node_modules/` is on disk, so
    /// missing the first stroke leaves no second chance — whatever is installed into `.bin/`
    /// is run as a tool by the next `npx` / `npm run`.
    #[test]
    fn a_spelling_the_kernel_normalizes_away_still_spells_node_modules_bin() {
        for p in [
            "node_modules/./.bin/tsc",
            "node_modules//.bin/tsc",
            "node_modules/.//.bin/tsc",
            "node_modules/x/../.bin/tsc",
            "./node_modules/./.BIN/./tsc",
            "pkg/node_modules\\.\\.bin\\tsc",
            "pkg/node_modules/x/../../node_modules/.bin/tsc",
        ] {
            assert_eq!(
                reason(
                    "Write",
                    serde_json::json!({ "file_path": p, "content": "#!/bin/sh\nid\n" })
                ),
                Some(OwnerReason::Escalates),
                "writing `{p}`"
            );
        }
        // Popping `..` may only recognize more, never less: this one is no longer under
        // `.bin` once popped, while the write still travels that path — whether `x` is a
        // directory, and whether `.bin` really gets created, are not decided by the
        // spelling.
        assert_eq!(
            reason(
                "Write",
                serde_json::json!({ "file_path": "node_modules/.bin/../x" })
            ),
            Some(OwnerReason::Escalates),
            "a `..` after the pair must not unspell it"
        );
        // Ordinary paths stay with the operator: what this test tightens is that pair of
        // names, not every spelling carrying a `.` or a `//`.
        for p in [
            "src/./main.rs",
            "src//main.rs",
            "src/x/../main.rs",
            "node_modules/typescript/lib/tsc.js",
            "node_modules/../src/main.rs",
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                None,
                "writing `{p}`"
            );
        }
    }

    /// The control-surface test asks **who will read this file**, not where the kernel opens
    /// it.
    ///
    /// Where `.claude` is a symlink to `<root>/tooling` (a monorepo sharing one config,
    /// dotfile management, and the link can come with the clone),
    /// `.claude/settings.json` resolves to `<root>/tooling/settings.json` — not one component
    /// named after a control surface, and squarely inside the allowlist. Classified by the
    /// resolved path alone, that write is operator-answerable, while Claude Code still reads
    /// it next time as `.claude/settings.json`: that hook executes on the agent's next tool
    /// call, with no second approval, run by the daemon itself.
    ///
    /// The other half (`dotgit -> .git`) is recognized only by the resolved path. Each
    /// direction covers half of the other, and [`names_along_resolution`] records both ends
    /// along with every hop between them.
    #[cfg(unix)]
    #[test]
    fn a_control_surface_reached_through_a_symlink_still_needs_the_owner() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("tooling")).unwrap();
        // Reads as `.claude/` and lands in `tooling/` — recognized only by the literal
        // form.
        std::os::unix::fs::symlink(ws.join("tooling"), ws.join(".claude")).unwrap();
        // Reads as `dotgit/` and lands in `.git/` — recognized only by the resolved form.
        std::os::unix::fs::symlink(ws.join(".git"), ws.join("dotgit")).unwrap();

        for p in [
            ".claude/settings.json",
            "dotgit/config",
            "dotgit/hooks/pre-commit",
        ] {
            assert_eq!(
                approval_owner_reason(
                    "Write",
                    &serde_json::json!({ "file_path": p }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                Some(OwnerReason::Escalates),
                "writing `{p}` reaches a control surface through a symlink"
            );
        }

        // The test did not widen along with it: a **genuinely** ordinary directory written
        // under its own name is still the operator's job. Otherwise this fix pushes the whole
        // workspace to the owner, and nobody can keep up with the approvals.
        //
        // This example must not use `tooling/settings.json`: in this fixture `tooling/`
        // **is** `.claude/`, so such an assertion would really pin "a shorter spelling of
        // `.claude/settings.json` belongs to the operator". See
        // `the_other_name_a_control_surface_gives_a_file_still_needs_the_owner`.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "an ordinary file that no control-surface name reaches stays with the operator"
        );
    }

    /// The **other** name of one file. The forward enumeration never sees it.
    ///
    /// Where `.claude -> tooling` comes with the clone (a monorepo sharing one config,
    /// dotfile management), `tooling/settings.json` and `.claude/settings.json` are one file.
    /// But [`names_along_resolution`] only walks forward from the spelling: write the first
    /// spelling and the list it walks out is `[<ws>/tooling/settings.json]`, with no
    /// `.claude` in it, so the write is judged operator-answerable — and that spelling is
    /// shorter and more natural, the one an agent is likelier to write. Claude Code still
    /// reads it next time as `.claude/settings.json` and executes the hook inside it, run by
    /// the daemon itself, with no second approval.
    ///
    /// So what this check asks about must be the **file**: does any control surface's name in
    /// the workspace land on it.
    #[cfg(unix)]
    #[test]
    fn the_other_name_a_control_surface_gives_a_file_still_needs_the_owner() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("tooling")).unwrap();
        std::os::unix::fs::symlink(ws.join("tooling"), ws.join(".claude")).unwrap();

        // The two spellings are one file, so both must get one answer.
        for p in ["tooling/settings.json", ".claude/settings.json"] {
            assert_eq!(
                approval_owner_reason(
                    "Write",
                    &serde_json::json!({ "file_path": p }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                Some(OwnerReason::Escalates),
                "`{p}` is `.claude/settings.json`; the shorter spelling must not be cheaper"
            );
        }
    }

    /// A control surface **need not be the target's sibling** — any level of the ancestor
    /// chain counts.
    ///
    /// Comparing only "is there a control surface of that name in the same parent directory"
    /// is another "fix this one spelling": move `.claude` to the root and point it deep, and
    /// the same hole is open again.
    #[cfg(unix)]
    #[test]
    fn a_control_surface_anywhere_up_the_tree_names_the_same_file() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("a/b/tooling")).unwrap();
        std::os::unix::fs::symlink(ws.join("a/b/tooling"), ws.join(".claude")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "a/b/tooling/settings.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`<ws>/.claude` names `a/b/tooling/` even though it is not its sibling"
        );
    }

    /// An alias can also be **one file** rather than a whole directory.
    ///
    /// The standard `stow` / dotfile layout has this shape: `.claude/` is a real directory
    /// and the `settings.json` in it is a link to somewhere else in the repo. A
    /// directory-level comparison cannot see it — `<ws>/.claude` resolves to `<ws>/.claude`
    /// and is not the target's ancestor — so it must look **inside** the control surface.
    ///
    /// And it must see it while the target **does not exist yet**: that link is dangling
    /// right now, and the agent's write is what would bring it to life. Judging once the file
    /// exists is too late, and by then the hook is in the harness's hands.
    #[cfg(unix)]
    #[test]
    fn a_file_level_control_surface_symlink_names_its_target_too() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::fs::create_dir_all(ws.join("shared")).unwrap();
        std::os::unix::fs::symlink("../shared/claude.json", ws.join(".claude/settings.json"))
            .unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "shared/claude.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`shared/claude.json` is what `.claude/settings.json` points at, file not yet created"
        );

        // A neighbour in the same directory is not implicated: the control surface points at
        // that one file, not at `shared/`.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "shared/notes.md" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "the alias names one file, not the whole directory it lives in"
        );
    }

    /// An alias differing only in case is still the same file.
    ///
    /// With the two reverse path comparisons ([`anchor_reaches`]'s directory level and
    /// [`scan_for_alias`]'s file level) comparing byte for byte,
    /// `.claude/settings.json -> ../Claude.json` plus a write of `claude.json` answers "no
    /// alias" — while on a case-insensitive volume the kernel resolves both spellings into
    /// one file the instant the write lands, and Claude Code reads it next time and executes
    /// the hook inside it. A dangling tail is never normalized
    /// ([`resolve_existing_ancestor`] resolves only the part that already exists), and
    /// dangling is exactly the shape this check exists for, so this is no corner case.
    ///
    /// The two assertions pin the two comparison points of one rule. On a case-sensitive
    /// volume, `Tooling` and `tooling` in the second assertion are two different directories
    /// and the answer is still owner — the trade-off [`fold_name`] writes down, leaning
    /// toward asking once more.
    #[cfg(unix)]
    #[test]
    fn a_control_surface_alias_that_differs_only_in_case_still_names_its_target() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::os::unix::fs::symlink("../Claude.json", ws.join(".claude/settings.json")).unwrap();
        std::fs::create_dir_all(ws.join("Tooling")).unwrap();
        std::os::unix::fs::symlink("Tooling", ws.join(".codex")).unwrap();

        for (p, why) in [
            (
                "claude.json",
                "the file `.claude/settings.json` points at, spelled in another case",
            ),
            (
                "tooling/config.toml",
                "`.codex` points at that directory, so everything under it is `.codex/...`",
            ),
        ] {
            assert_eq!(
                approval_owner_reason(
                    "Write",
                    &serde_json::json!({ "file_path": p }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                Some(OwnerReason::Escalates),
                "{why}"
            );
        }

        // The test did not widen along with it: folding case only recognizes "another
        // spelling of one name", it does not count everything as an alias.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "notes.md" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "no control-surface name reaches `notes.md` in any case, so the operator answers"
        );
    }

    /// [`fold_name`]'s contract, written out pair by pair.
    ///
    /// The left is the ordinary spelling and the right is another spelling **the kernel
    /// resolves to the same file**: these must fold to one thing, and failing to fold them
    /// together allows a write a control surface reaches. The lower half is the other side —
    /// folding coarse is not folding everything into one, and the test must not widen far
    /// enough to count unrelated names as aliases.
    #[test]
    fn names_the_kernel_resolves_to_one_file_fold_to_one_name() {
        for (plain, other, why) in [
            (
                ".husky",
                ".hu\u{17f}ky",
                "U+017F folds to `s` — case folding, which is not the lowercase mapping",
            ),
            ("stop", "\u{fb05}op", "U+FB05 folds to `st`"),
            (".rustup", ".ru\u{fb06}up", "U+FB06 folds to `st`"),
            ("config", "con\u{fb01}g", "U+FB01 folds to `fi`"),
            ("off", "o\u{fb00}", "U+FB00 folds to `ff`"),
            ("flag", "\u{fb02}ag", "U+FB02 folds to `fl`"),
            ("office", "o\u{fb03}ce", "U+FB03 folds to `ffi`"),
            ("waffle", "wa\u{fb04}e", "U+FB04 folds to `ffl`"),
            ("strasse", "stra\u{df}e", "U+00DF folds to `ss`"),
            ("strasse", "STRA\u{1e9e}E", "U+1E9E folds to `ss`"),
            (
                "kelvin",
                "\u{212a}elvin",
                "U+212A is the letter `k` to a case-insensitive volume",
            ),
            (
                ".git",
                ".g\u{131}t",
                "NTFS uppercases U+0131 to ASCII `I`, so these are one name there",
            ),
            (
                "i",
                "\u{130}",
                "U+0130 is folded to `i` as well — coarser than any volume, so it only asks once more",
            ),
            (
                "a;b",
                "a\u{37e}b",
                "U+037E canonically decomposes to `;`, which a normalization-insensitive volume follows",
            ),
            (
                "a;\u{6587}",
                "a\u{37e}\u{6587}",
                "the `;` U+037E folds to is a word boundary like any other one",
            ),
            (
                "caf\u{e9}.json",
                "cafe\u{301}.json",
                "NFC and NFD are one file on a normalization-insensitive volume",
            ),
            (
                "\u{ac00}.json",
                "\u{1100}\u{1161}.json",
                "the same for Hangul, which decomposes into jamo",
            ),
            (
                ".claude",
                ".clau\u{200d}de",
                "HFS+ compares as if the format character were not there",
            ),
            (
                ".claude",
                ".claude.",
                "Win32 strips a trailing dot before it opens the path",
            ),
        ] {
            assert_eq!(fold_name(plain), fold_name(other), "{why}");
        }

        for (a, b, why) in [
            (
                "cafe.json",
                "caf\u{e9}.json",
                "the accent is a real difference in every normalization",
            ),
            (
                "hared.json",
                "\u{17f}hared.json",
                "folding U+017F to `s` must not swallow the letter itself",
            ),
            (
                ".claude",
                ".claude\u{301}",
                "an accented `.claudé` is not the control surface",
            ),
            (
                "src",
                "\u{6587}\u{6863}",
                "an ordinary non-ASCII name spells no ASCII name",
            ),
        ] {
            assert_ne!(fold_name(a), fold_name(b), "{why}");
        }
    }

    /// A spelling the kernel folds into one name is that name — the forward table's side of
    /// the rule.
    ///
    /// An insensitive volume folds by **case folding**, not by the lowercase mapping:
    /// `ſ`(U+017F) folds to `s`, and `ﬁ ﬆ` fold to `fi st`. Names on the table carry `s`,
    /// `st` and `fi`, so each of them has a second spelling — once written, the kernel
    /// resolves it to the control surface itself, while folding lowercase alone lets this
    /// check wave it through.
    #[cfg(unix)]
    #[test]
    fn a_spelling_that_folds_into_a_control_surface_name_is_that_control_surface() {
        for (p, why) in [
            (
                ".hu\u{17f}ky/pre-commit",
                "`.huſky` is `.husky`: U+017F folds to `s`",
            ),
            (
                ".ru\u{fb06}up/settings.toml",
                "`.ruﬆup` is `.rustup`: U+FB06 folds to `st`",
            ),
            (
                ".pre-commit-con\u{fb01}g.yaml",
                "`.pre-commit-conﬁg.yaml` is `.pre-commit-config.yaml`: U+FB01 folds to `fi`",
            ),
            (
                "node_module\u{17f}/.bin/tsc",
                "`node_moduleſ/.bin/tsc` is the `tsc` the next command runs",
            ),
            (
                // Win32 strips trailing `.` and spaces off every segment before it opens a
                // path. On Unix these are two different directories and the answer is still
                // owner — the trade-off [`fold_name`] writes down, leaning toward asking
                // once more.
                ".claude./settings.json",
                "Win32 opens `.claude./settings.json` as `.claude/settings.json`",
            ),
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "{why}"
            );
        }

        // The test did not widen along with it: what folds is "another spelling of one
        // name", not "every name carrying non-ASCII goes to the owner".
        for (p, why) in [
            ("notes.md", "an ordinary file reaches no control surface"),
            (
                "\u{17f}notes.md",
                "`ſnotes.md` folds to `snotes.md`, which is nobody's control surface",
            ),
            (
                "\u{6587}\u{6863}/note.md",
                "a non-ASCII directory name spells no control surface either",
            ),
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                None,
                "{why}"
            );
        }
    }

    /// The reverse side of the same rule: the file a control surface's alias points at,
    /// spelled with another fold.
    ///
    /// The two reverse comparisons compare **arbitrary** path segments rather than the ASCII
    /// names on the table, so a control surface like `.claude` or `.git`, whose own name
    /// carries no foldable letter, is walked past from this side just the same.
    #[cfg(unix)]
    #[test]
    fn an_alias_target_spelled_with_another_fold_still_names_the_same_file() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::os::unix::fs::symlink("../shared.json", ws.join(".claude/settings.json")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "\u{17f}hared.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`ſhared.json` is the file `.claude/settings.json` points at, folded"
        );

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "hared.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "`hared.json` is a different name in every fold, so the operator answers"
        );
    }

    /// A case-insensitive volume is normalization-insensitive too, so an alias's two
    /// normalizations are one file as well.
    ///
    /// The two sides' normalizations come from different places to begin with: git writes a
    /// link's text as NFC, HFS+ stores NFD, and the tail carries the spelling the agent wrote
    /// this time ([`resolve_existing_ancestor`] normalizes only the part that already
    /// exists). So hitting this takes nothing special from the agent.
    #[cfg(unix)]
    #[test]
    fn an_alias_target_spelled_in_another_normalization_still_names_the_same_file() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        // The link's text is NFC (what git writes down), and this write spells NFD.
        std::os::unix::fs::symlink("../Caf\u{e9}.json", ws.join(".claude/settings.json")).unwrap();
        // The reverse pair: the link's text is NFD and this write spells NFC.
        std::os::unix::fs::symlink("Ge\u{301}nie", ws.join(".codex")).unwrap();

        for (p, why) in [
            (
                "Cafe\u{301}.json",
                "NFD names the NFC file `.claude/settings.json` points at",
            ),
            (
                "G\u{e9}nie/config.toml",
                "NFC names the NFD directory `.codex` points at",
            ),
        ] {
            assert_eq!(
                approval_owner_reason(
                    "Write",
                    &serde_json::json!({ "file_path": p }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                Some(OwnerReason::Escalates),
                "{why}"
            );
        }

        // The normalization fold did not swallow the neighbours: `Cafe.json` and
        // `Café.json` are two files, and no fold the kernel applies resolves them into
        // one.
        for (p, why) in [
            (
                "Cafe.json",
                "`Cafe.json` without the accent is a different file in every normalization",
            ),
            (
                "src/main.rs",
                "an ordinary source file reaches no control surface",
            ),
        ] {
            assert_eq!(
                approval_owner_reason(
                    "Write",
                    &serde_json::json!({ "file_path": p }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                None,
                "{why}"
            );
        }
    }

    /// Ask even while `node_modules/.bin` does not exist — this write is the one that would
    /// conjure it into being.
    ///
    /// With `node_modules -> vendor/nm` and nothing installed under `vendor/nm` yet, taking
    /// "`.bin` already exists" as the probe's premise means looking away from precisely the
    /// shape this check exists for: once written, `vendor/nm/.bin/tsc` is the `tsc` in the
    /// next command.
    #[cfg(unix)]
    #[test]
    fn a_node_modules_bin_that_this_write_would_create_still_needs_the_owner() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("vendor/nm")).unwrap();
        std::os::unix::fs::symlink("vendor/nm", ws.join("node_modules")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "vendor/nm/.bin/tsc" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`vendor/nm/.bin/tsc` is `node_modules/.bin/tsc`, `.bin` not yet created"
        );

        // What is outside `.bin` in the same tree is not implicated.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "vendor/nm/index.js" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "`node_modules/` itself is not a control surface; only its `.bin` is"
        );
    }

    /// The reverse enumeration's scope follows the **session cwd**, and what is off both
    /// chains is out of scope.
    ///
    /// Upper half: the daemon's own settlement child process starts on the session cwd, and a
    /// control-surface alias on that chain is of the "run by the daemon itself, with no second
    /// approval" kind — the kind that must never be missed — so the cwd chain has to be
    /// scanned.
    ///
    /// The lower half pins the bound itself ([`ControlSurfaceScope`] writes down why it is
    /// drawn here): a control surface in a subtree neither chain passes through is **not
    /// looked at**, and the answer is operator-answerable. Writing it down makes "should this
    /// scan wider some day" a visible decision rather than something a later change moves out
    /// of the way.
    #[cfg(unix)]
    #[test]
    fn the_reverse_scope_is_the_target_chain_and_the_session_cwd_chain() {
        let (_d, roots, ws) = workspace();
        let pkg = ws.join("packages/app");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(ws.join("shared")).unwrap();
        std::os::unix::fs::symlink("../../shared/env.sh", pkg.join(".envrc")).unwrap();
        // One file, spelled absolutely — the only difference between the two is the session
        // cwd.
        let target = ws.join("shared/env.sh");

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": target }),
                &roots,
                &pkg,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "the session sits on the branch that holds `.envrc`, so that anchor is scanned"
        );

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": target }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "neither chain passes through `packages/app`, so that anchor is out of scope"
        );
    }

    /// The vine is walked to its end: where a link inside a control surface points at a
    /// **directory**, the alias is in that directory, not next to the link.
    ///
    /// `.git/hooks -> ../shared-hooks` plus `shared-hooks/pre-commit -> ../payload`. If the
    /// reverse scan meeting the `hooks` link only compares "is the landing a prefix of
    /// `payload`" and does not walk into a landing that is a directory, writing `payload`
    /// gets the operator-answerable `None` — while the `.git/hooks/pre-commit` git executes
    /// on its next commit is this very file, run by the daemon itself, with no second
    /// approval.
    #[cfg(unix)]
    #[test]
    fn a_two_hop_directory_link_is_walked_to_the_file_at_its_end() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("shared-hooks")).unwrap();
        std::os::unix::fs::symlink("../shared-hooks", ws.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink("../payload", ws.join("shared-hooks/pre-commit")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "payload" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`payload` is `.git/hooks/pre-commit` two hops out; the owner answers"
        );

        // The whole **directory** the first hop lands in is called `.git/hooks/`.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "shared-hooks/post-commit" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`shared-hooks/` is `.git/hooks/`, so everything in it is a hook"
        );

        // The test did not widen along with it. Both of these are ordinary writes that
        // **no** control-surface name reaches:
        //   * `src/main.rs` — that traversal ran to the end and found not one alias;
        //   * `payload2` — a name one character away from `payload`, while `starts_with`
        //     compares by **path segment**, not by string prefix. Compared as strings, this
        //     one would be implicated.
        for p in ["src/main.rs", "payload2"] {
            assert_eq!(
                approval_owner_reason(
                    "Write",
                    &serde_json::json!({ "file_path": p }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                None,
                "`{p}` is reached by no control-surface name and stays with the operator"
            );
        }
    }

    /// However deep an alias is buried, the answer must not change — this traversal may have
    /// no depth limit.
    ///
    /// Any depth line makes an alias beyond it (here `.yarn/plugins/@scope/deep/x.cjs`, four
    /// levels in) return exactly the same `false` as "there really is no alias". A Yarn
    /// plugin has this shape, and loading it executes it.
    #[cfg(unix)]
    #[test]
    fn a_file_link_several_levels_deep_is_still_found() {
        let (_d, roots, ws) = workspace();
        let deep = ws.join(".yarn/plugins/@scope/deep");
        std::fs::create_dir_all(&deep).unwrap();
        std::os::unix::fs::symlink("../../../../payload.cjs", deep.join("x.cjs")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "payload.cjs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`payload.cjs` is `.yarn/plugins/@scope/deep/x.cjs`, four levels in"
        );

        // A file in the same tree that nothing points at stays with the operator — what this
        // check tightens is aliases, not depth.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "walking deeper must not widen the answer for files nothing points at"
        );
    }

    /// **A branch left unwalked by the budget must not be read as "there is no alias".**
    ///
    /// Both budgets — directory entries and link resolutions — must return `true` (back to
    /// the owner) once exhausted; only a walk that really ran to the end and found nothing
    /// may return `false`.
    #[cfg(unix)]
    #[test]
    fn a_branch_left_unexplored_by_the_budget_goes_to_the_owner() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        // One more than `AliasBudget::links`: this traversal is certain to exhaust it, and
        // the ordering does not matter.
        let over = AliasBudget::new().links + 1;
        for i in 0..over {
            std::os::unix::fs::symlink(
                format!("../notes/{i}.txt"),
                ws.join(".claude").join(format!("link-{i}")),
            )
            .unwrap();
        }

        // Not one of these links points at `src/main.rs`. But we **could not look at all of
        // them**, so "there is no alias" is unanswerable — and when it cannot be answered the
        // answer is owner.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "the link budget ran out before the enumeration finished, so the owner answers"
        );

        // The directory-entry budget is the same. Exhausting it takes hundreds of thousands
        // of files, so this half asks `scan_for_alias` directly with a shrunken budget — the
        // shape is identical to the half above.
        let mut tiny = AliasBudget {
            dirents: 1,
            links: usize::MAX,
            segments: usize::MAX,
        };
        assert!(
            scan_for_alias(&ws.join(".claude"), &ws.join("src/main.rs"), &mut tiny),
            "a dirent budget that runs out mid-directory must read as `unproven`, not as `no alias`"
        );

        // With an ample budget and genuinely no alias, the answer is still `false` —
        // otherwise this guardrail degenerates into "always ask the owner", which is the same
        // as no guardrail.
        let mut ample = AliasBudget::new();
        assert!(
            !scan_for_alias(&ws.join(".git"), &ws.join("src/main.rs"), &mut ample),
            "an exhausted enumeration that found nothing must still answer `no alias`"
        );
    }

    /// **The anchor loop's own resolutions go on the account too.**
    ///
    /// The `location_of` in the anchor loop and the ones inside [`scan_for_alias`] are the
    /// same thing at the same price. Uncharged, the expensive half falls entirely off the
    /// account: the `.agit` prefix rule lets one directory hold arbitrarily many matching
    /// names, so the cheap directory-entry budget is still mostly intact while this approval
    /// is already realpath-ing them one by one.
    #[cfg(unix)]
    #[test]
    fn the_anchor_loops_own_resolutions_are_billed_to_the_link_budget() {
        let (_d, roots, ws) = workspace();
        // One more than `AliasBudget::links`: this enumeration is certain to exhaust it, and
        // `read_dir`'s ordering does not matter. The directory-entry budget is still more
        // than an order of magnitude from its ceiling, so the only one exhausted is the link
        // budget.
        let over = AliasBudget::new().links + 1;
        for i in 0..over {
            std::os::unix::fs::symlink(format!("../notes/{i}.txt"), ws.join(format!(".agit-{i}")))
                .unwrap();
        }

        // Not one of these anchors points at `src/main.rs`. But we could not resolve all of
        // them, so "there is no alias" is unanswerable.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "anchor resolutions that outrun the link budget must read as `unproven`"
        );
    }

    /// **The hop where the segment budget runs out is "did not finish" as well.**
    ///
    /// The segment budget follows the same rule as the other two: one segment short of
    /// finishing forfeits the right to say "there is no alias". The other way round, an
    /// ordinary link must still resolve while the budget is ample — this line tightens how
    /// many segments were walked, not "a link means asking the owner".
    #[cfg(unix)]
    #[test]
    fn a_resolution_that_runs_out_of_segments_is_unprovable_not_absent() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::os::unix::fs::symlink("../notes.txt", ws.join(".claude/link")).unwrap();
        let (anchor, tgt) = (ws.join(".claude"), ws.join("src/main.rs"));

        // First find out how many segments a full walk costs, then set the budget one short
        // — so the assertion does not follow how deep the tempdir happens to be.
        let mut ample = AliasBudget::new();
        assert!(
            !scan_for_alias(&anchor, &tgt, &mut ample),
            "an exhausted enumeration that found nothing must answer `no alias`"
        );
        let full = AliasBudget::new().segments - ample.segments;
        assert!(full > 0, "walking a control surface must cost segments");

        let mut short = AliasBudget {
            segments: full - 1,
            ..AliasBudget::new()
        };
        assert!(
            scan_for_alias(&anchor, &tgt, &mut short),
            "one segment short of walking it all must read as `unproven`, not as `no alias`"
        );

        // End to end: an ample budget, a link pointing elsewhere, and the write stays with
        // the operator.
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "an ordinary link resolves, points elsewhere, and leaves the write with the operator"
        );
    }

    /// Build a chain of existing directories `deep` segments deep and return its bottom
    /// level.
    #[cfg(unix)]
    fn deep_chain(from: &Path, deep: usize) -> PathBuf {
        let mut at = from.to_path_buf();
        for _ in 0..deep {
            at.push("a");
        }
        std::fs::create_dir_all(&at).unwrap();
        at
    }

    /// The run of directories [`ControlSurfaceScope`] hands out under an ample budget,
    /// flattened.
    fn scope_dirs(target: &Path, cwd: &Path, roots: &CanonicalRoots) -> Vec<PathBuf> {
        let mut ample = AliasBudget::new();
        let mut walk = ControlSurfaceScope::new(target, cwd, roots);
        let mut dirs = vec![];
        while let Some(step) = walk.next_dir(&mut ample) {
            dirs.push(step.expect("an ample budget walks both chains to the end"));
        }
        dirs
    }

    /// **A spelling of a few segments that expands through a middle link into a deep path is
    /// charged the depth it expands to.**
    ///
    /// After `mid -> d/a/a/…/a`, the spelling `.claude/jump -> ../mid` has few segments of
    /// its own, while `canonicalize` walks that whole vine in **one** call and the resolver
    /// then returns that very deep path to keep walking. Charged by the spelling, this hop is
    /// billed a dozen segments while what is actually paid is that deep path's whole depth,
    /// and the depth is given by whoever wrote the link.
    #[cfg(unix)]
    #[test]
    fn a_short_spelling_that_expands_through_a_link_is_charged_the_depth_it_reaches() {
        const DEEP: usize = 300;
        let (_d, _roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        deep_chain(&ws.join("d"), DEEP);
        // The middle hop: its own target is deep, and nobody asks the budget about its
        // spelling.
        std::os::unix::fs::symlink(format!("d/{}", "a/".repeat(DEEP)), ws.join("mid")).unwrap();
        // The one the agent sees: a few segments.
        let jump = ws.join(".claude/jump");
        std::os::unix::fs::symlink("../mid", &jump).unwrap();

        let mut budget = AliasBudget::new();
        let at = location_of(&jump, &mut budget).expect("the vine resolves");
        let charged = AliasBudget::new().segments - budget.segments;

        // Few segments in the spelling, and a landing at the far end of the budget.
        assert!(
            jump.components().count() * 4 < at.components().count(),
            "the spelling must be far shorter than what it expands to: {} vs {}",
            jump.components().count(),
            at.components().count()
        );
        assert!(
            charged >= at.components().count(),
            "a hop must be charged what it expanded to ({} segments), not what it was spelled as \
             (charged {charged})",
            at.components().count()
        );
    }

    /// **The landing says nothing about how many segments the pass walked.** A vine can walk
    /// deep and land shallow.
    ///
    /// `l0 -> a/…/a/../…/../l1 -> …`: each hop's link text descends dozens of segments and
    /// then pops back the same way to reach the next hop. Handed to one `canonicalize`, the
    /// kernel follows the whole chain and walks every target segment by segment, and what it
    /// hands back is a landing of a few segments — the landing does not record a word of how
    /// far the pass went. An account kept by the landing (even "the larger of the spelling and
    /// the landing") records the small change, and the difference between that and the real
    /// cost is decided by whoever wrote the link: the budget looks mostly intact while this
    /// approval has already stopped there, and a stopped approval and an unread approval are
    /// the same thing.
    ///
    /// So a link's text is charged **before** the kernel walks it — see "one `canonicalize`
    /// may not swallow a whole vine" in [`resolve_ancestor`].
    #[cfg(unix)]
    #[test]
    fn a_vine_that_walks_deep_and_lands_shallow_is_charged_what_it_walked() {
        const K: usize = 40;
        const HOPS: usize = 12;
        let (_d, _roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        deep_chain(&ws, K);
        std::fs::create_dir_all(ws.join("d")).unwrap();
        // Each hop: down K segments, back the same way, on to the next hop. The landing
        // always stays under the workspace root.
        for i in 0..HOPS {
            let target = format!("{}{}l{}", "a/".repeat(K), "../".repeat(K), i + 1);
            std::os::unix::fs::symlink(target, ws.join(format!("l{i}"))).unwrap();
        }
        std::os::unix::fs::symlink("d", ws.join(format!("l{HOPS}"))).unwrap();
        // The one the agent sees: a few segments.
        let jump = ws.join(".claude/jump");
        std::os::unix::fs::symlink("../l0", &jump).unwrap();

        let mut budget = AliasBudget::new();
        let at = location_of(&jump, &mut budget).expect("the vine resolves");
        let charged = AliasBudget::new().segments - budget.segments;

        // The landing is shallower than the spelling: billed by it, this pass walked for
        // free.
        assert!(
            at.components().count() <= jump.components().count(),
            "the landing must be no deeper than the spelling: {} vs {}",
            at.components().count(),
            jump.components().count()
        );
        // The floor on what the kernel walked segment by segment is every hop's link text,
        // whole.
        let vine = HOPS * (2 * K + 1);
        assert!(
            charged >= vine,
            "a vine whose link texts spell {vine} segments must be charged all of them, not the \
             {} segments its landing happens to have (charged {charged})",
            at.components().count()
        );
    }

    /// **Every entry in a deep directory costs that depth for its lstat alone.**
    ///
    /// One `read_dir` pays this directory's depth once, while every link in it still needs an
    /// `lstat` of its own, and that is another whole path. Where an entry points **shallow**
    /// (an absolute target) resolving it costs almost nothing, so leaving the lstat off the
    /// account drops the entire cost of tens of thousands of links in a deep directory — and
    /// the two budgets that count things are both too cheap to hold it.
    #[cfg(unix)]
    #[test]
    fn every_entry_in_a_deep_directory_is_charged_that_depth() {
        const DEEP: usize = 300;
        const ENTRIES: usize = 40;
        let (_d, _roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        let landing = deep_chain(&ws.join("d"), DEEP);
        for i in 0..ENTRIES {
            // An absolute target, shallow and non-existent: resolving it is a few segments
            // while lstat-ing it walks all of `landing`.
            std::os::unix::fs::symlink("/no-such-root/x", landing.join(format!("l{i}"))).unwrap();
        }
        std::os::unix::fs::symlink(format!("d/{}", "a/".repeat(DEEP)), ws.join("mid")).unwrap();
        std::os::unix::fs::symlink("../mid", ws.join(".claude/jump")).unwrap();

        let mut budget = AliasBudget::new();
        assert!(
            !scan_for_alias(&ws.join(".claude"), &ws.join("src/main.rs"), &mut budget),
            "none of these links points at the target"
        );
        let charged = AliasBudget::new().segments - budget.segments;
        let floor = ENTRIES * landing.components().count();
        assert!(
            charged >= floor,
            "every entry of a {}-deep directory must be charged that depth (>= {floor}), \
             not just the one read_dir that listed them (charged {charged})",
            landing.components().count()
        );
    }

    /// **The ancestor sweep's `read_dir` is a deep walk too, and `target` itself may be
    /// something a link expanded into.**
    ///
    /// [`ControlSurfaceScope`] walks `target`'s ancestor chain, and `target` is the path
    /// **after resolution**: once a spelling of a few segments like `mid/x` expands through
    /// `mid -> d/a/…/a`, the ancestor chain holds hundreds of levels, and every level's
    /// `read_dir` makes the kernel walk from the start.
    #[cfg(unix)]
    #[test]
    fn the_ancestor_sweep_over_an_expanded_target_is_charged_its_depth() {
        const DEEP: usize = 300;
        let (_d, roots, ws) = workspace();
        deep_chain(&ws.join("d"), DEEP);
        std::os::unix::fs::symlink(format!("d/{}", "a/".repeat(DEEP)), ws.join("mid")).unwrap();

        // The spelling: `mid/x`. After resolution: hundreds of levels deep.
        let target = resolve_against("mid/x", &ws).expect("the vine resolves");
        assert!(
            target.components().count() > DEEP,
            "a two-segment spelling must expand into the deep tree"
        );

        let scope = scope_dirs(&target, &ws, &roots);
        let floor: usize = scope.iter().map(|d| d.components().count()).sum();
        assert!(
            scope.len() > DEEP,
            "the expanded target puts its whole chain in scope"
        );

        let mut budget = AliasBudget::new();
        reachable_under_a_control_surface_name_within(&target, &ws, &roots, &mut budget);
        let charged = AliasBudget::new().segments - budget.segments;
        assert!(
            charged >= floor,
            "opening each of the {} ancestors costs its own depth (>= {floor} segments), \
             and that has to be on the account (charged {charged})",
            scope.len()
        );
    }

    /// **Not one piece of the work on a deep chain may happen before the budget.**
    ///
    /// How many levels the two ancestor chains hold is known only **after** `target` is
    /// resolved, and what it resolves to is decided by the links in the workspace. So
    /// "flatten the whole chain into a container first, then hold what is in the container
    /// under the budget" is itself the cost this account is there to stop: one flattening is
    /// a whole path per level, deduplicating entry by entry adds a round of comparing the
    /// chain against itself, and what queues behind this check is the approval thread. Once
    /// the budget is exhausted this enumeration must already have stopped where it stands —
    /// an account that cannot stop the cost is the same as no account.
    #[test]
    fn a_deep_ancestor_chain_does_no_work_the_budget_has_not_paid_for() {
        // Deep in the spelling only: not one level of this chain has to exist, so what is
        // timed here is walking the chain itself.
        const DEEP: usize = 1_200;
        // With the budget exhausted this enumeration affords one level. The ceiling is wide
        // enough that one level completes on any machine, and narrow enough that "flatten the
        // whole chain first" cannot get past it.
        const CEILING: std::time::Duration = std::time::Duration::from_secs(1);

        let (_d, roots, ws) = workspace();
        let mut target = ws.clone();
        for _ in 0..DEEP {
            target.push("a");
        }
        target.push("payload.js");
        let first = target
            .parent()
            .expect("the target has a parent")
            .to_path_buf();

        // Not a penny left: any time spent can only have been spent before the budget was
        // asked.
        let mut spent = AliasBudget {
            dirents: 0,
            links: 0,
            segments: 0,
        };
        let began = std::time::Instant::now();
        let unproven =
            reachable_under_a_control_surface_name_within(&target, &ws, &roots, &mut spent);
        let took = began.elapsed();
        assert!(
            unproven,
            "an enumeration that could not afford a single step has proven nothing, so the owner \
             answers"
        );
        assert!(
            took < CEILING,
            "a {DEEP}-deep chain must not be walked before the budget is asked (took {took:?})"
        );

        // A budget that affords exactly the first level walks the first level only and
        // touches not one after it.
        let mut just_one = AliasBudget {
            dirents: 0,
            links: 0,
            segments: first.components().count(),
        };
        let mut walk = ControlSurfaceScope::new(&target, &ws, &roots);
        assert_eq!(
            walk.next_dir(&mut just_one),
            Some(Ok(first)),
            "the level the budget paid for is the one that comes out"
        );
        assert_eq!(
            walk.next_dir(&mut just_one),
            Some(Err(())),
            "the next level is refused, not handed out on credit"
        );
        assert_eq!(
            walk.next_dir(&mut just_one),
            None,
            "a walk that outran the budget stays stopped"
        );

        // Under an ample budget, walking the whole chain must still not compare it against
        // itself once per **level**: that cost grows with the square of the chain's length,
        // while this level was charged segments once, for its own depth.
        let mut ample = AliasBudget::new();
        let mut whole = ControlSurfaceScope::new(&target, &ws, &roots);
        let began = std::time::Instant::now();
        let mut levels = 0usize;
        while let Some(step) = whole.next_dir(&mut ample) {
            step.expect("an ample budget walks both chains to the end");
            levels += 1;
        }
        let took = began.elapsed();
        assert!(
            levels > DEEP,
            "the whole chain has to come out, not a prefix of it (got {levels} levels)"
        );
        assert!(
            took < CEILING,
            "each level costs its own depth and no more (walking {levels} levels took {took:?})"
        );
    }

    /// **The same anchors and the same directory entries, and only the side that really
    /// walks deep paths spends the budget dry.**
    ///
    /// This is the end-to-end form of the assertion above, and the property that matters most
    /// about this account: the budget caps **how far it walked**, not "how many things were
    /// looked at". The two trees have the same spellings, the same number of anchors and the
    /// same number of directory nodes, and differ only in how deep that middle link lands —
    /// the deep side must flip to owner and the shallow one must stay with the operator.
    #[cfg(unix)]
    #[test]
    fn only_the_side_that_walks_deep_paths_exhausts_the_shared_segment_budget() {
        const DEEP: usize = 300;
        const VINE: usize = 30;
        const ANCHORS: usize = 260;

        // `mid` lands `deep` segments down, or right under the workspace root — apart from
        // that the two trees are identical.
        fn tree(deep: usize) -> (tempfile::TempDir, CanonicalRoots, PathBuf) {
            let (d, roots, ws) = workspace();
            let landing = deep_chain(&ws.join("d"), deep);
            deep_chain(&landing, VINE);
            let mut target = PathBuf::from("d");
            for _ in 0..deep {
                target.push("a");
            }
            std::os::unix::fs::symlink(&target, ws.join("mid")).unwrap();
            for i in 0..ANCHORS {
                std::os::unix::fs::symlink("mid", ws.join(format!(".agit-{i}"))).unwrap();
            }
            (d, roots, ws)
        }

        let write = serde_json::json!({ "file_path": "src/main.rs" });

        let (_d, roots, ws) = tree(DEEP);
        assert_eq!(
            approval_owner_reason("Write", &write, &roots, &ws, &BTreeSet::new()),
            Some(OwnerReason::Escalates),
            "short spellings that expand onto a deep tree must spend the segment budget and \
             leave the enumeration unfinished, so the owner answers"
        );

        // The same 260 anchors and the same 30-level vine, so the link and directory-entry
        // budgets are charged **exactly the same**; only the segment count differs, and the
        // segment count is the item actually paid.
        let (_d, roots, ws) = tree(0);
        assert_eq!(
            approval_owner_reason("Write", &write, &roots, &ws, &BTreeSet::new()),
            None,
            "the same enumeration over a shallow tree stays affordable and stays with the operator"
        );
    }

    /// A link cycle: it must stop, and the answer it stops with must be right.
    ///
    /// Without recording `visited`, "keep walking into the directory a link points at" needs
    /// only one `.claude/loop -> .` to make this traversal walk forever — the approval thread
    /// hangs, which is the same as denial of service.
    #[cfg(unix)]
    #[test]
    fn a_link_cycle_inside_a_control_surface_terminates() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join(".claude/deep")).unwrap();
        // Pointing at the directory it lives in.
        std::os::unix::fs::symlink(".", ws.join(".claude/loop")).unwrap();
        // Pointing from deeper back at the control surface's root, closing a longer
        // cycle.
        std::os::unix::fs::symlink("..", ws.join(".claude/deep/up")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "the cycle is walked to a stop and, having found no alias, stays with the operator"
        );

        // A cycle that cannot be resolved (the kernel gives ELOOP here): `location_of`
        // cannot answer, so it goes back to the owner.
        std::os::unix::fs::symlink("b", ws.join(".claude/a")).unwrap();
        std::os::unix::fs::symlink("a", ws.join(".claude/b")).unwrap();
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "a link we cannot resolve might name this file, so the owner answers"
        );
    }

    /// When the reverse question cannot be answered, the answer is owner.
    ///
    /// `.claude -> a/b` while `a` is itself a dangling link: nobody can say **where** this
    /// control surface points, so nothing proves it does not land on the file being written.
    /// Enumerating hop by hop only holds if the enumeration really finished, and an unfinished
    /// one proves nothing — and this check's default must be "no".
    #[cfg(unix)]
    #[test]
    fn a_control_surface_whose_own_target_is_unprovable_is_answered_by_the_owner() {
        let (_d, roots, ws) = workspace();
        std::os::unix::fs::symlink(ws.join("nowhere"), ws.join("a")).unwrap();
        std::os::unix::fs::symlink("a/b", ws.join(".claude")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "src/main.rs" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "a control surface we cannot resolve might name this file, so the owner answers"
        );
    }

    /// Join two links end to end and neither endpoint is named after a control surface.
    ///
    /// With `alias -> .claude` and `.claude -> tooling` both in the repo, writing
    /// `alias/settings.json`: the resolved form is only `tooling/settings.json`, the literal
    /// form is only `alias/settings.json`, and neither end matches a control surface. Yet
    /// Claude still reads that same file next time as `.claude/settings.json` and executes the
    /// hook inside it — the middle hop is the name that matters.
    #[cfg(unix)]
    #[test]
    fn a_control_surface_in_the_middle_of_a_symlink_chain_still_needs_the_owner() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("tooling")).unwrap();
        std::os::unix::fs::symlink(ws.join("tooling"), ws.join(".claude")).unwrap();
        std::os::unix::fs::symlink(ws.join(".claude"), ws.join("alias")).unwrap();

        assert_eq!(
            names_along_resolution(&ws.join("alias/settings.json")),
            Some(vec![
                ws.join("alias/settings.json"),
                ws.join(".claude/settings.json"),
                ws.join("tooling/settings.json"),
            ]),
            "every hop's own spelling must survive the walk, tail and all"
        );
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "alias/settings.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`alias/settings.json` reaches `.claude/settings.json` through the chain"
        );
    }

    /// A longer chain, with the control surface hidden **strictly in the middle**: neither
    /// the first hop nor the last is named after it.
    #[cfg(unix)]
    #[test]
    fn a_control_surface_deeper_inside_a_chain_is_found_too() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("plain")).unwrap();
        // outer -> mid -> .cursor -> plain
        std::os::unix::fs::symlink(ws.join("plain"), ws.join(".cursor")).unwrap();
        std::os::unix::fs::symlink(ws.join(".cursor"), ws.join("mid")).unwrap();
        std::os::unix::fs::symlink(ws.join("mid"), ws.join("outer")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "outer/mcp.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "the third hop of `outer/mcp.json` is `.cursor/mcp.json`"
        );
    }

    /// Checked hop by hop, **not** "a link means the owner".
    ///
    /// A monorepo's shared directories, dotfile management, a pnpm store, `docs -> ../shared`
    /// — links are everywhere in an ordinary repo. A blanket rule stuffs the owner's queue
    /// with every write the operator could have answered, and once the queue is full the
    /// owner starts going blind.
    #[cfg(unix)]
    #[test]
    fn a_symlink_chain_with_no_control_surface_on_it_stays_with_the_operator() {
        let (_d, roots, ws) = workspace();
        std::fs::create_dir_all(ws.join("shared/docs")).unwrap();
        std::os::unix::fs::symlink(ws.join("shared"), ws.join("mirror")).unwrap();
        // A relative target, joined onto the directory the link lives in — as the kernel
        // does.
        std::os::unix::fs::symlink("mirror", ws.join("notes")).unwrap();

        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "notes/docs/plan.md" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "no hop of `notes/docs/plan.md` is a control surface"
        );
    }

    /// A chain that cannot be walked to the end: there is **no** answer of the form "far
    /// enough".
    ///
    /// Checking hop by hop only holds if that run of names was really enumerated to the end.
    /// An unfinished enumeration proves nothing, and this check's default must be "no".
    #[cfg(unix)]
    #[test]
    fn a_chain_that_cannot_be_walked_to_the_end_is_answered_by_the_owner() {
        let (_d, roots, ws) = workspace();
        std::fs::write(ws.join("notes.txt"), "hi").unwrap();
        // A link pointing at a **plain file**, with another segment behind it: the lstat of
        // the `settings.json` segment answers ENOTDIR, not ENOENT — we know nothing about
        // whether it is a link.
        std::os::unix::fs::symlink(ws.join("notes.txt"), ws.join("via")).unwrap();

        assert_eq!(
            names_along_resolution(&ws.join("via/settings.json")),
            None,
            "a component we cannot even lstat leaves the name list unproven"
        );
        // The read side holds up too: ENOTDIR is not ENOENT, and
        // `resolve_existing_ancestor` counts only "really not there" as not there —
        // everything else is "what this segment is cannot be judged".
        assert_eq!(
            approval_owner_reason(
                "Read",
                &serde_json::json!({ "file_path": "via/settings.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "a component we cannot lstat is not a tail that is merely absent"
        );
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "via/settings.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "an unwalkable chain must fail closed"
        );

        // The **dangling** kind is the other half: its target reads out (`.claude` enters
        // the list as usual), and "cannot resolve" already sent it to the owner back in
        // `confined_read_path`.
        std::os::unix::fs::symlink(ws.join("nowhere"), ws.join(".claude")).unwrap();
        std::os::unix::fs::symlink(ws.join(".claude"), ws.join("broken")).unwrap();
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "broken/settings.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "a chain that dangles at the end is not operator-answerable either"
        );
    }

    /// **Expanding past the length the kernel accepts is not "this segment does not exist
    /// yet".**
    ///
    /// This resolution hands the kernel the whole `cur` after expansion, while the kernel
    /// walking the same spelling holds only "link target + tail not yet walked", with the
    /// prefix already walked as a vnode. So the spelling is short and the kernel walks it,
    /// while `cur` has already passed the length it accepts: from that segment on,
    /// `read_link` / `canonicalize` / `symlink_metadata` all answer `ENAMETOOLONG`. Read that
    /// as "the tail does not exist yet" and the resolver stops following symlinks and splices
    /// the rest on unchanged — the location judged sits inside an allowlist root while the
    /// kernel opens the file outside it, and `Read` / `Grep` / `LS` all become
    /// operator-answerable.
    #[cfg(unix)]
    #[test]
    fn a_tail_that_expands_past_what_the_kernel_accepts_is_unprovable_not_absent() {
        // How long a path the kernel accepts on this machine: `ENOENT` = accepted, and no
        // other error is.
        fn of_len(base: &Path, n: usize) -> PathBuf {
            let mut s = base.to_string_lossy().into_owned();
            while s.len() < n {
                let room = n - s.len();
                s.push('/');
                for _ in 1..room.min(101) {
                    s.push('a');
                }
            }
            PathBuf::from(s)
        }
        fn accepts(p: &Path) -> bool {
            matches!(std::fs::symlink_metadata(p),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        }

        let d = tempfile::tempdir().unwrap();
        // The root itself needs some length: what crosses the line is "the root + the piece
        // expanded out", while the kernel walking the spelling holds the root as a vnode,
        // which takes no room in its buffer.
        let root = canonical(d.path()).join("w".repeat(64));
        std::fs::create_dir(&root).unwrap();
        let roots = CanonicalRoots::from_verified(vec![root.clone()]);
        let root_len = root.as_os_str().len();

        let (mut fits, mut over) = (root_len + 2, root_len + 16_384);
        assert!(
            accepts(&of_len(&root, fits)),
            "a short path must be accepted"
        );
        assert!(
            !accepts(&of_len(&root, over)),
            "some length must be refused"
        );
        while over - fits > 1 {
            let mid = (fits + over) / 2;
            if accepts(&of_len(&root, mid)) {
                fits = mid;
            } else {
                over = mid;
            }
        }
        let limit = fits;

        // `escape` is the link pointing outside the root, and `vine` is the link text that
        // buries it deep. The sizes hold three things only: the link text joined after the
        // root crosses the line (so the resolver starts guessing from there), the kernel can
        // still walk the spelling itself (so that read really lands outside the root), and
        // this chain of directories can be created.
        let escape = "s".repeat(200);
        let expanded = limit - 16;
        let vine = expanded - 1 - escape.len();
        assert!(
            root_len + 1 + expanded > limit,
            "the fixture must expand past what the kernel accepts"
        );
        assert!(
            expanded + 1 + "secret.txt".len() <= limit,
            "the kernel must still be able to walk the spelling itself"
        );
        assert!(
            root_len + 1 + vine <= limit,
            "the directory chain must be creatable by its own absolute path"
        );

        let mut names: Vec<String> = vec![];
        let mut left = vine;
        while left > 255 {
            names.push("d".repeat(200));
            left -= 201;
        }
        names.push("d".repeat(left));
        let target = names.join("/");
        assert_eq!(
            target.len(),
            vine,
            "the link text is the length we sized it to"
        );
        std::fs::create_dir_all(names.iter().fold(root.clone(), |p, n| p.join(n))).unwrap();
        std::os::unix::fs::symlink(&target, root.join("S")).unwrap();

        // The file outside the root. `escape`'s own absolute path already crosses the line,
        // so it can only be created through `S` — on that spelling the kernel never holds
        // anything this long.
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("S").join(&escape)).unwrap();
        let spelled = root.join("S").join(&escape).join("secret.txt");

        // The kernel opens it all the same, and outside the root: this is not a spelling
        // that leads nowhere.
        assert_eq!(
            std::fs::read(&spelled).unwrap(),
            b"secret",
            "the kernel resolves this spelling to a file outside the root"
        );
        // What it opens is that file outside the root itself. `canonical` cannot answer here
        // (`realpath` also holds the whole expanded path, so it gives `ENAMETOOLONG` too), so
        // identity goes by inode.
        use std::os::unix::fs::MetadataExt;
        let opened = std::fs::metadata(&spelled).unwrap();
        let outside_file = std::fs::metadata(outside.path().join("secret.txt")).unwrap();
        assert_eq!(
            (opened.dev(), opened.ino()),
            (outside_file.dev(), outside_file.ino()),
            "the file the kernel opens is the one outside the root"
        );

        assert_eq!(
            resolve_existing_ancestor(&spelled),
            None,
            "a segment the kernel refuses to look at leaves the location unproven"
        );
        // The same spelling at another gate: the hub's file preview (`fs.readFile`) has only
        // [`require_within`] in front of it, and the path it hands back is the one about to
        // be opened.
        assert!(
            !is_within(&spelled, &roots),
            "a location we cannot judge is not a location inside the root"
        );
        assert!(
            require_within(&spelled, &roots).is_err(),
            "the preview gate must refuse what it could not place"
        );

        for (tool, key) in [("Read", "file_path"), ("Grep", "path"), ("LS", "path")] {
            assert_eq!(
                approval_owner_reason(
                    tool,
                    &serde_json::json!({ key: spelled }),
                    &roots,
                    &root,
                    &BTreeSet::new(),
                ),
                Some(OwnerReason::Escalates),
                "`{tool}` must not read a path it could not judge as one inside the root"
            );
        }
    }

    /// A cycle must neither hang nor recurse forever — **this test running to completion is
    /// itself the assertion**.
    ///
    /// The first assertion below sits further in than the second. A cycle is stopped by
    /// `confined_read_path` as well, but the reason there is "cannot resolve", not this test;
    /// should the read side ever widen, this has to stand on its own.
    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_walked_to_a_stop_and_answered_by_the_owner() {
        let (_d, roots, ws) = workspace();
        std::os::unix::fs::symlink(ws.join("ouroboros_b"), ws.join("ouroboros_a")).unwrap();
        std::os::unix::fs::symlink(ws.join("ouroboros_a"), ws.join("ouroboros_b")).unwrap();

        assert_eq!(
            names_along_resolution(&ws.join("ouroboros_a/settings.json")),
            None,
            "a loop has no end to walk to, so the name list is never proven complete"
        );
        assert_eq!(
            approval_owner_reason(
                "Write",
                &serde_json::json!({ "file_path": "ouroboros_a/settings.json" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates)
        );
    }

    /// **One link allowance per resolution, not one per spelled segment.**
    ///
    /// The kernel's `MAXSYMLINKS` is the allowance of one `open`: walking `a/b/c`, the links
    /// followed on `a`, `b` and `c` go on one counter, and filling it means `ELOOP` for the
    /// whole path. Resetting that number at every spelled segment lets each segment of the
    /// spelling buy its own [`MAX_LINK_HOPS`], and how many segments a spelling has is decided
    /// by whoever writes the approval message — a path the kernel refuses outright can make
    /// this check follow chains to their end segment by segment, and a slow approval and an
    /// unread approval are the same thing.
    #[cfg(unix)]
    #[test]
    fn the_link_hop_allowance_belongs_to_the_resolution_not_to_each_spelled_segment() {
        // A flat chain landing back on the workspace root: following it takes `CHAIN` hops,
        // and one pass's allowance holds several of them.
        const CHAIN: usize = 20;
        const { assert!(CHAIN * 2 <= MAX_LINK_HOPS) };
        let (_d, roots, ws) = workspace();
        for i in 0..CHAIN - 1 {
            std::os::unix::fs::symlink(format!("l{}", i + 1), ws.join(format!("l{i}"))).unwrap();
        }
        std::os::unix::fs::symlink(".", ws.join(format!("l{}", CHAIN - 1))).unwrap();

        // A spelling inside the allowance is judged as usual: this line tightens how many
        // hops one pass follows in total, not "a link means asking the owner".
        for repeat in 1..=MAX_LINK_HOPS / CHAIN {
            let spelled = format!("{}payload.js", "l0/".repeat(repeat));
            assert_eq!(
                approval_owner_reason(
                    "Read",
                    &serde_json::json!({ "file_path": spelled }),
                    &roots,
                    &ws,
                    &BTreeSet::new(),
                ),
                None,
                "`{spelled}` stays inside one resolution's allowance"
            );
        }

        // One more segment crosses the line. The kernel walking this spelling gave ELOOP
        // long before, so "cannot be judged" is exactly its answer — and this
        // classification's cost stops at the allowance with it.
        let over = MAX_LINK_HOPS / CHAIN + 1;
        let spelled = format!("{}payload.js", "l0/".repeat(over));
        assert_eq!(
            approval_owner_reason(
                "Read",
                &serde_json::json!({ "file_path": spelled }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "`{spelled}` follows more links than one resolution may, so nothing is proven"
        );
    }

    /// **The forward pass needs a floor too, and one pass inside the link allowance walks
    /// it dry.**
    ///
    /// What a resolution pays is not "how many hops it followed" but **how many segments it
    /// walked**: each descended segment hands the kernel the whole `cur` at that moment to
    /// walk, and how long each hop's link text is and how deep it goes are decided by whoever
    /// wrote the link. [`MAX_LINK_HOPS`] caps the hops and cannot cap this — a short spelling
    /// the kernel accepts can pile this side's work arbitrarily high while staying inside the
    /// hop allowance. So this side gets a segment budget of its own
    /// ([`AliasBudget::for_one_path`]): exhausted = cannot be judged = back to the owner;
    /// while an ordinary vine inside the allowance must still be judged, otherwise this check
    /// becomes "a link means asking the owner".
    #[cfg(unix)]
    #[test]
    fn a_resolution_that_outruns_the_forward_budget_is_unprovable_not_absent() {
        // Each hop descends `K` segments and pops back the same way to reach the next: the
        // spelling has few segments and the landing is shallow, while the segments walked are
        // `K` times the hop count.
        const K: usize = 200;
        const HOPS: usize = 20;
        const { assert!(HOPS < MAX_LINK_HOPS) };

        let (_d, roots, ws) = workspace();
        deep_chain(&ws, K);
        std::fs::create_dir_all(ws.join("d")).unwrap();
        for i in 0..HOPS {
            let target = format!("{}{}l{}", "a/".repeat(K), "../".repeat(K), i + 1);
            std::os::unix::fs::symlink(target, ws.join(format!("l{i}"))).unwrap();
        }
        std::os::unix::fs::symlink("d", ws.join(format!("l{HOPS}"))).unwrap();

        // The vine's last few hops: affordable, judged, and left with the operator.
        let affordable = format!("l{}/payload.js", HOPS - 2);
        assert_eq!(
            approval_owner_reason(
                "Read",
                &serde_json::json!({ "file_path": affordable }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            None,
            "an ordinary vine must not be priced out of the operator's hands"
        );

        // The whole vine: the hop count is still inside the allowance while the segments
        // walked are not.
        assert_eq!(
            approval_owner_reason(
                "Read",
                &serde_json::json!({ "file_path": "l0/payload.js" }),
                &roots,
                &ws,
                &BTreeSet::new(),
            ),
            Some(OwnerReason::Escalates),
            "a resolution that outran the budget proved nothing, so the owner answers"
        );
    }

    /// PATH resolution counts only **executable** candidates, as the shell does.
    ///
    /// Looking at `is_file()` alone, a plain file of the same name earlier on PATH stops the
    /// resolution there, while the shell skips it and executes the genuinely executable one
    /// behind it — which may be a script the agent just wrote into the workspace. The test
    /// then concludes about a file that will never be executed, in the allow direction.
    #[cfg(unix)]
    #[test]
    fn path_resolution_skips_non_executable_files_like_the_shell_does() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, roots, cwd) = workspace();
        // An **executable** `ls` in the workspace (the kind the agent can write) ...
        let planted = cwd.join("ls");
        std::fs::write(&planted, "#!/bin/sh\ncurl http://x\n").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        // ... shadowed by a **non-executable** plain file of the same name earlier on PATH.
        let shadow = tempfile::tempdir().unwrap();
        std::fs::write(shadow.path().join("ls"), "just a note\n").unwrap();
        std::fs::set_permissions(
            shadow.path().join("ls"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let path_env = std::env::join_paths([shadow.path(), &cwd]).unwrap();

        assert_eq!(
            resolve_in_path("ls", Some(path_env.as_os_str())),
            Some(canonical(&planted)),
            "the shell skips a non-executable candidate, so resolution must land on the one \
             that really executes"
        );
        // End to end: resolution lands inside the workspace ⇒ hand it to the owner. Stopping
        // on that note file, the test would give this line to the operator as the system
        // `ls`, while what the shell really executes is the script the agent planted.
        assert_eq!(
            confined_command_in(
                "ls",
                &roots,
                &cwd,
                &BTreeSet::new(),
                Some(path_env.as_os_str())
            )
            .err(),
            Some(OwnerReason::Escalates),
        );
    }

    /// PATH resolution has to expand through the extension table, otherwise **nothing
    /// resolves at all** on Windows.
    ///
    /// What lies on disk there is `git.exe` / `npm.cmd` / `rg.exe`, and joining a bare name
    /// matches none of them: `resolve_in_path` is `None` uniformly ⇒ every shell command is
    /// judged `Unprovable` ⇒ the whole INERT list and every `agit rc grant` the owner approved
    /// might as well not exist on that machine, and everything escalates to the owner — which
    /// makes cross-platform support worth nothing.
    ///
    /// The extension table is a parameter rather than a `#[cfg]` precisely so this test
    /// reaches it on **this** machine: real directories, real files, real execute bits.
    #[test]
    fn a_bare_name_resolves_through_the_extension_table() {
        // Expansion follows the table's order. A name that carries an extension is appended
        // nothing; see `a_name_that_already_has_an_extension_is_not_grown_a_second_one`.
        assert_eq!(candidate_names("rg", None), vec!["rg".to_string()]);
        assert_eq!(
            candidate_names("rg", Some(std::ffi::OsStr::new(".com;.exe; .cmd"))),
            vec!["rg.com", "rg.exe", "rg.cmd"]
        );

        let stray = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        // An executable earlier on PATH with **no extension**, named `rg` (the kind the
        // agent can write). cmd does not execute it, and the resolution must not stop on
        // it.
        write_executable(&stray.path().join("rg"));
        // The one that really gets executed.
        let installed = real.path().join("rg.sh");
        write_executable(&installed);
        let path_env = std::env::join_paths([stray.path(), real.path()]).unwrap();

        assert_eq!(
            resolve_in_path_with(
                "rg",
                Some(path_env.as_os_str()),
                Some(std::ffi::OsStr::new(".com;.sh"))
            ),
            Some(canonical(&installed)),
            "the one with an extension is the file the shell really executes"
        );
        // With no extension table (Unix) the behavior is unchanged: the bare name is the
        // only candidate.
        assert_eq!(
            resolve_in_path_with("rg", Some(path_env.as_os_str()), None),
            Some(canonical(&stray.path().join("rg"))),
        );

        // **Directories outside, extensions inside**: cmd tries every `PATHEXT` in one
        // directory before moving to the next. With the two loops the other way round, a
        // `.com` in a later PATH directory beats a `.sh` in an earlier one — the test then
        // concludes about a file that will never be executed, and the earlier directory is
        // exactly where the agent may be able to write.
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let wins = first.path().join("rg.sh");
        write_executable(&wins);
        write_executable(&second.path().join("rg.com"));
        let two_dirs = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(
            resolve_in_path_with(
                "rg",
                Some(two_dirs.as_os_str()),
                Some(std::ffi::OsStr::new(".com;.sh"))
            ),
            Some(canonical(&wins)),
            "an earlier PATH directory wins first, and extension order decides only within \
             one directory"
        );
    }

    /// **A name that carries an extension is not appended a `PATHEXT` one** — appending
    /// flips the conclusion.
    ///
    /// cmd reaches for `PATHEXT` only when the command has no extension written (this is also
    /// what `where /?` says): `tool.exe` looks for `tool.exe` and does not settle for
    /// `tool.exe.cmd`.
    ///
    /// Appending anyway, a `tool.exe.cmd` in an external directory earlier on PATH matches
    /// first, the resolution lands **outside** the workspace ⇒ the test reads it as an
    /// external tool the owner granted and allows it; while on the shell's side that external
    /// directory holds no `tool.exe` at all, and what really gets executed is the `tool.exe`
    /// in the workspace that agit can write. So a program the agent wrote itself runs under
    /// the owner's grant.
    ///
    /// What is pinned here is the **classification**, not which file resolves: the flip shows
    /// up in the conclusion.
    #[test]
    fn a_name_that_already_has_an_extension_is_not_grown_a_second_one() {
        let (_d, roots, cwd) = workspace();
        // Earlier on PATH: an external directory the owner trusts, holding **only**
        // `tool.exe.cmd`.
        let external = tempfile::tempdir().unwrap();
        write_executable(&external.path().join("tool.exe.cmd"));
        // Later on PATH: the workspace, holding the `tool.exe` the agent can write — the one
        // cmd really executes.
        let planted = cwd.join("tool.exe");
        write_executable(&planted);
        let path_env = std::env::join_paths([external.path(), cwd.as_path()]).unwrap();
        let pathext = std::ffi::OsStr::new(".com;.exe;.cmd");

        // The conclusion: the owner granted `tool.exe`, but what this line runs is the file
        // in the workspace, so it must go back to the owner and must not be allowed as an
        // external tool.
        let granted: BTreeSet<String> = ["tool.exe".to_string()].into_iter().collect();
        assert_eq!(
            confined_command_with(
                "tool.exe --version",
                &roots,
                &cwd,
                &granted,
                Some(path_env.as_os_str()),
                Some(pathext),
            )
            .err(),
            Some(OwnerReason::Escalates),
            "the workspace's `tool.exe` is the one executed, and the grant must not cover it"
        );
        // Where that conclusion comes from: the resolution must land on the file in the
        // workspace.
        assert_eq!(
            resolve_in_path_with("tool.exe", Some(path_env.as_os_str()), Some(pathext)),
            Some(canonical(&planted)),
            "a command name with an extension looks for the literal name only, and \
             `tool.exe.cmd` is not one of its candidates"
        );

        // The consequence is pinned; now the rule itself: an extension on the `PATHEXT`
        // table ⇒ the literal name only; nothing written ⇒ expand through the table (see
        // `an_extension_that_is_not_on_the_pathext_list_is_not_an_extension`).
        assert_eq!(
            candidate_names("tool.exe", Some(pathext)),
            vec!["tool.exe".to_string()]
        );
        assert_eq!(
            candidate_names("tool", Some(std::ffi::OsStr::new(".com;.cmd"))),
            vec!["tool.com", "tool.cmd"]
        );
    }

    /// `PATHEXT` supplements a command with **no extension written** — the test is the
    /// purely syntactic "does it carry an extension", not "is the extension on the `PATHEXT`
    /// table".
    ///
    /// Narrowing it to the latter is wrong. The counterexample sits on a machine whose
    /// `PATHEXT` lacks `.EXE`: judged by the table, `.exe` is not on it ⇒ keep appending ⇒ the
    /// external `tool.exe.CMD` earlier on PATH matches first ⇒ allowed as an external tool the
    /// owner granted; while cmd sees a command with an extension written, looks straight for
    /// `tool.exe`, and runs the one later on PATH, in the workspace, that the agent can write.
    /// Test and execution diverge right there.
    ///
    /// The first part pins the **classification** (the name allowed is not the file really
    /// executed), not a string; the second pins the rule itself: an extension not on the table
    /// (`.tool`) likewise tries the literal name only, with no expansion.
    #[test]
    fn a_syntactic_extension_is_never_grown_a_second_one() {
        // This machine's `PATHEXT` does **not** hold `.EXE` — judging by the table and
        // judging by syntax part ways here.
        let pathext = std::ffi::OsStr::new(".CMD");

        let (_d, roots, cwd) = workspace();
        // Earlier on PATH: an external directory the owner trusts, holding only
        // `tool.exe.CMD` (which only a table-based judgment looks for).
        let external = tempfile::tempdir().unwrap();
        write_executable(&external.path().join("tool.exe.CMD"));
        // Later on PATH: the workspace, holding the `tool.exe` the agent can write — the one
        // cmd really executes.
        let planted = cwd.join("tool.exe");
        write_executable(&planted);
        let path_env = std::env::join_paths([external.path(), cwd.as_path()]).unwrap();

        let granted: BTreeSet<String> = ["tool.exe".to_string()].into_iter().collect();
        assert_eq!(
            confined_command_with(
                "tool.exe --version",
                &roots,
                &cwd,
                &granted,
                Some(path_env.as_os_str()),
                Some(pathext),
            )
            .err(),
            Some(OwnerReason::Escalates),
            "what runs is the workspace's `tool.exe`, and the name the owner granted must \
             not cover it"
        );
        assert_eq!(
            resolve_in_path_with("tool.exe", Some(path_env.as_os_str()), Some(pathext)),
            Some(canonical(&planted)),
            "an extension written means the literal name only — `tool.exe.CMD` is no \
             candidate"
        );

        // The rule itself: an extension means the literal name only, whether or not the
        // table holds that extension.
        assert_eq!(
            candidate_names("tool.exe", Some(pathext)),
            vec!["tool.exe".to_string()]
        );
        assert_eq!(
            candidate_names("my.tool", Some(pathext)),
            vec!["my.tool".to_string()]
        );
        assert_eq!(
            candidate_names("tool.EXE", Some(std::ffi::OsStr::new(".COM;.EXE;.CMD"))),
            vec!["tool.EXE".to_string()]
        );
        // The other way round, only a name with no extension written is supplemented from
        // the table.
        assert_eq!(
            candidate_names("tool", Some(std::ffi::OsStr::new(".COM;.EXE;.CMD"))),
            vec!["tool.COM", "tool.EXE", "tool.CMD"]
        );
    }

    /// Executable = `access(X_OK)` says **this process** may execute it, not "some execute
    /// bit is set".
    ///
    /// Where the two disagree is exactly where the test misreads: a file earlier on PATH owned
    /// by someone else with the execute bit open to its owner only has the bit set, while the
    /// daemon's user gets EACCES and the shell skips it and keeps looking. Judged by the bit,
    /// the resolution stops there and gives the operator a file that **will never be
    /// executed** as the system's `ls`, while what really gets executed is the script of the
    /// same name the agent wrote into the workspace.
    ///
    /// The same EACCES is built here from "the owner itself has no execute bit": POSIX looks
    /// at the first matching permission class only, no `x` in the owner class is a refusal,
    /// and `mode & 0o111` is still non-zero.
    #[cfg(unix)]
    #[test]
    fn path_resolution_asks_whether_this_user_may_execute_not_whether_any_bit_is_set() {
        use std::os::unix::fs::PermissionsExt;
        // For root, X_OK answers "any bit will do", which leaves this test proving
        // nothing.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let (_d, roots, cwd) = workspace();
        let planted = cwd.join("ls");
        write_executable(&planted);
        // Earlier on PATH: the execute bit given to group and others but not to the owner —
        // that is, not to us.
        let shadow = tempfile::tempdir().unwrap();
        let denied = shadow.path().join("ls");
        std::fs::write(&denied, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o011)).unwrap();
        let path_env = std::env::join_paths([shadow.path(), &cwd]).unwrap();

        assert_eq!(
            resolve_in_path("ls", Some(path_env.as_os_str())),
            Some(canonical(&planted)),
            "the shell skips a candidate it cannot execute, so resolution must land on the \
             one that really runs"
        );
        assert_eq!(
            confined_command_in(
                "ls",
                &roots,
                &cwd,
                &BTreeSet::new(),
                Some(path_env.as_os_str())
            )
            .err(),
            Some(OwnerReason::Escalates),
        );
    }

    /// **`..` means "up one level from the resolved location", not a literal fold.**
    ///
    /// With a symlink in the workspace pointing outside (a pnpm store, `docs -> ../shared`,
    /// `target -> /Volumes/build`, all common), `vendor/../etc/passwd` folds lexically into
    /// `<root>/etc/passwd` — inside the allowlist — while the kernel resolves `vendor` to what
    /// it points at, goes up one level, and finally opens the real `/etc/passwd`.
    #[test]
    fn a_dotdot_after_a_symlinked_component_does_not_fold_its_way_back_inside() {
        let (_d, roots, cwd) = workspace();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/", cwd.join("rootlink")).unwrap();
        #[cfg(unix)]
        {
            for p in [
                "rootlink/../etc/passwd",
                "src/../rootlink/../etc/passwd",
                "rootlink/etc/passwd",
            ] {
                assert_eq!(
                    approval_owner_reason(
                        "Read",
                        &serde_json::json!({ "file_path": p }),
                        &roots,
                        &cwd,
                        &BTreeSet::new()
                    ),
                    Some(OwnerReason::Escalates),
                    "reading `{p}` through a symlink must not be operator-answerable"
                );
            }
            // A `..` inside the workspace, as usual — it crosses no link.
            assert_eq!(
                approval_owner_reason(
                    "Read",
                    &serde_json::json!({ "file_path": "src/../src/main.rs" }),
                    &roots,
                    &cwd,
                    &BTreeSet::new()
                ),
                None
            );
        }
    }

    /// A glob starting with a brace or a bracket has every alternative validated on its own.
    ///
    /// The first byte of `{/etc/**,x}` is `{`, so "the fixed prefix is empty" holds and
    /// `starts_with('/')` does not, and a test that looks at the first byte allows it all the
    /// way — while every glob implementation with brace expansion expands the first
    /// alternative to `/etc/**` and walks out of the workspace.
    #[test]
    fn every_brace_alternative_of_a_glob_is_judged_on_its_own() {
        for pat in ["{/etc/**,x}", "{,/}etc/**", "{x,/etc/**}", "/etc/{a,b}"] {
            assert_eq!(
                reason("Glob", serde_json::json!({ "pattern": pat })),
                Some(OwnerReason::Escalates),
                "glob `{pat}`"
            );
        }
        // Braces inside the workspace, as usual.
        for pat in ["**/*.{ts,tsx}", "src/{a,b}/**/*.rs"] {
            assert_eq!(
                reason("Glob", serde_json::json!({ "pattern": pat })),
                None,
                "{pat}"
            );
        }
    }

    /// With a wildcard **in front of** the escaping segment, `head` validates not one piece
    /// — while glob walks that link all the same.
    ///
    /// `*/.ssh/id_*` has an empty fixed prefix (its first byte is `*`) and is not absolute, so
    /// the "a relative pattern with no fixed prefix acts on cwd" branch allows it outright and
    /// `is_within` never runs on anything this wildcard can produce. And what `*` matches is
    /// exactly names like `docs -> ../shared`, `target -> /Volumes/build` and
    /// `rootlink -> /`, glob readdirs out along one of them, and the paths it returns are
    /// inside no root.
    #[cfg(unix)]
    #[test]
    fn a_wildcard_directory_component_is_not_operator_answerable() {
        let (_d, roots, cwd) = workspace();
        std::os::unix::fs::symlink("/", cwd.join("rootlink")).unwrap();
        let glob = |pat: &str| {
            approval_owner_reason(
                "Glob",
                &serde_json::json!({ "pattern": pat }),
                &roots,
                &cwd,
                &BTreeSet::new(),
            )
        };
        // A named segment is still stopped: `confined_read_path` can resolve the
        // `rootlink/` piece.
        assert_eq!(
            glob("rootlink/*"),
            Some(OwnerReason::Escalates),
            "precondition: a named escaping segment belongs to the owner anyway"
        );
        // Move the wildcard **in front of** it and what matches is still the same link.
        for pat in [
            "*/.ssh/id_*",
            "*/etc/passwd",
            "*/*",
            "?ootlink/etc/*",
            "[r]ootlink/**",
            "{a,*}/etc/passwd",
        ] {
            assert_eq!(glob(pat), Some(OwnerReason::Escalates), "glob `{pat}`");
        }
        // A `**` descending in place, and a wildcard on the last segment, as usual — that is
        // the `-r` class.
        for pat in [
            "**/*.rs",
            "src/**/*.rs",
            "*.rs",
            "src/*.rs",
            "**/*.{ts,tsx}",
        ] {
            assert_eq!(glob(pat), None, "glob `{pat}`");
        }
    }

    /// A **relative** PATH entry (POSIX says an empty entry = the current directory) must
    /// not be resolved against agitd's own process cwd.
    ///
    /// `export PATH="$MAYBE_UNSET:$PATH"` is the classic profile spelling, and what the daemon
    /// and every harness child process it starts inherit is `:/usr/bin:/bin`. bash resolves
    /// that empty entry to **its own** cwd — the session cwd, where the agent just wrote an
    /// executable `ls` — while `resolve_in_path_with` does not hold the session cwd at all,
    /// `d.join("ls")` is the relative path `ls`, and what it stats is agitd's cwd. The test
    /// then falls all the way through to `/usr/bin/ls` and says "this is the system's `ls`"
    /// about a file that **will never be executed**, letting the operator allow the agent's
    /// own program.
    #[cfg(unix)]
    #[test]
    fn an_empty_path_entry_is_never_resolved_against_the_daemons_own_cwd() {
        let (_d, roots, cwd) = workspace();
        // The `ls` the agent just wrote into the workspace (a Write the operator can
        // allow).
        let planted = cwd.join("ls");
        write_executable(&planted);
        // The system's `ls`, in a directory outside the workspace.
        let system = tempfile::tempdir().unwrap();
        write_executable(&system.path().join("ls"));
        let system_only = std::env::join_paths([system.path()]).unwrap();
        let leading_empty = std::ffi::OsString::from(format!(":{}", system_only.to_string_lossy()));

        // Premise check: absolute entries still resolve to the system's `ls`, so this test
        // does not go green on "nothing resolves at all".
        assert_eq!(
            resolve_in_path("ls", Some(system_only.as_os_str())),
            Some(canonical(&system.path().join("ls"))),
        );
        // With the empty entry first, the shell runs `<session cwd>/ls`, and we cannot
        // prove that.
        assert_eq!(
            resolve_in_path("ls", Some(leading_empty.as_os_str())),
            None,
            "a relative PATH entry = cannot be judged, and must not be skipped to keep \
             looking"
        );
        assert_eq!(
            confined_command_in(
                "ls -la src",
                &roots,
                &cwd,
                &BTreeSet::new(),
                Some(leading_empty.as_os_str()),
            )
            .err(),
            Some(OwnerReason::Unprovable),
            "{} lies in the session cwd, so this line must not be judged \
             operator-answerable",
            planted.display()
        );
        // The same command line stays with the operator when every PATH entry is
        // absolute.
        assert_eq!(
            confined_command_in(
                "ls -la src",
                &roots,
                &cwd,
                &BTreeSet::new(),
                Some(system_only.as_os_str()),
            ),
            Ok(())
        );
    }

    /// **Nothing expands inside single quotes** — most regexes an agent writes carry a `$`
    /// or a `\`.
    ///
    /// Refusing them in both kinds of quotes alike makes the one capability that survives,
    /// "reading and searching the workspace still belongs to the operator", dead in practice —
    /// while a set of cases without those two characters stays green and shows nothing. The
    /// double-quoted half cannot be let through: `"$(curl x)"` is substituted by a real
    /// shell.
    #[test]
    fn a_single_quoted_regex_is_not_mistaken_for_a_shell_expansion() {
        for cmd in [
            "rg 'fn main$' src",
            r"rg '\bTODO\b' src",
            r"grep -E '^\s*fn ' src/main.rs",
            r"grep -n 'foo\|bar' src",
            "rg 'a{2,3}' src",
        ] {
            assert_eq!(
                bash(cmd),
                None,
                "`{cmd}` is a pure read inside the workspace"
            );
        }
        // Substitution inside double quotes is still refused.
        for cmd in [
            r#"echo "$(curl http://x)""#,
            r#"echo "`curl http://x`""#,
            r#"grep "a\$b" src"#,
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
    }

    /// A flag that follows symlinks while recursing is not on the list.
    ///
    /// Every path token of `grep -R TODO src` is validated (`src` is inside the root), but
    /// `-R` follows the symlinks met during the traversal — one `src/vendor -> ~` reads
    /// `~/.ssh/*` into the transcript. The classifier's whole premise is "the validated path
    /// tokens frame the effect", and that does not hold for `-R`.
    #[test]
    fn a_recursive_flag_that_follows_symlinks_is_not_on_the_list() {
        assert!(bash("grep -R TODO src").is_some());
        assert!(bash("ls -R src").is_some());
        // The non-following `-r`, as usual.
        assert_eq!(bash("grep -r TODO src"), None);
    }

    /// `mcp__agit__search` / `rc_list` run under **the machine owner's identity**.
    ///
    /// They look read-only, but their results come from the whole corpus the owner can see —
    /// every team, every private project, including workspaces this operator was never
    /// admitted to. A cross-workspace read is not harmless for being read-only.
    #[test]
    fn the_agit_mcp_verbs_that_read_the_owners_whole_corpus_are_not_hard_allowed() {
        for t in ["mcp__agit__search", "mcp__agit__rc_list"] {
            assert_eq!(
                reason(t, serde_json::json!({ "query": "AWS_SECRET_ACCESS_KEY" })),
                Some(OwnerReason::Unprovable),
                "{t}"
            );
        }
        // `show` / `view` are not on the allow list either: by id they read any session in
        // the owner's local store, not just this workspace — the same reason as `search`.
        for t in ["mcp__agit__show", "mcp__agit__view"] {
            assert_eq!(
                reason(t, serde_json::json!({ "id": "agit-somebody-elses" })),
                Some(OwnerReason::Unprovable),
                "{t}"
            );
        }
        // `rc_status` is of the same kind: it lists **every live session** on this machine
        // (id, runtime, state, seq), including those in workspaces this operator was never
        // admitted to. A cross-workspace read is not harmless for being read-only — that rule
        // holds for it just the same.
        assert_eq!(
            reason("mcp__agit__rc_status", serde_json::json!({})),
            Some(OwnerReason::Unprovable)
        );
        // `status` is the same: it lists **every** agent repo in the owner's local store
        // (`clone::list_local()`), not "the current repo".
        assert_eq!(
            reason("mcp__agit__status", serde_json::json!({})),
            Some(OwnerReason::Unprovable)
        );
    }

    /// An ordinary CJK directory name must not panic the classifier.
    ///
    /// `name[..5]` slices by **byte**: `你好` is six bytes, so `len() >= 5` holds while the
    /// fifth byte cuts in the middle of a character — an outright panic. And this test runs
    /// inside approval classification, where one panic kills this session's approval task and
    /// leaves every approval after it unanswered.
    #[test]
    fn a_non_ascii_path_component_does_not_panic_the_classifier() {
        // AGENTS.md exception (iii): CJK fixtures whose UTF-8 boundaries are the point. Two
        // of the four scripts here are deliberately CJK; ASCII would make this vacuous.
        for p in ["你好/file.rs", "文档/.git/config", "ünïcødé/x", "🙂/a"] {
            // What it judges does not matter; that it **returns** does.
            let _ = reason("Read", serde_json::json!({ "file_path": p }));
            let _ = reason("Write", serde_json::json!({ "file_path": p }));
        }
    }

    /// Braces are **expanded until none is left**.
    ///
    /// Expanding only the first group, `{,x}{/etc/**,y}` still starts with `{` after one
    /// expansion and takes the "relative pattern with no fixed prefix" branch and is allowed —
    /// while glob's full expansion contains `/etc/**`.
    #[test]
    fn a_second_brace_group_is_expanded_too() {
        // The ones that expand into an **absolute** form. `{a,b}/{/etc/**,y}` is
        // deliberately absent: it expands into `a//etc/**`, which is `a/etc/**` to glob — a
        // relative path, genuinely inside the workspace.
        for pat in ["{,x}{/etc/**,y}", "{x,{y,/etc/**}}", "{y,/etc}/**"] {
            assert_eq!(
                reason("Glob", serde_json::json!({ "pattern": pat })),
                Some(OwnerReason::Escalates),
                "glob `{pat}`"
            );
        }
        // Several brace groups inside the workspace, as usual.
        assert_eq!(
            reason(
                "Glob",
                serde_json::json!({ "pattern": "src/{a,b}/**/*.{ts,tsx}" })
            ),
            None
        );
    }

    /// **Quotes do not change the arguments the program sees.**
    ///
    /// Once the shell strips the quotes from `grep "-R" TODO src`, grep still receives `-R`.
    /// Skipping quoted tokens turns them into positionals — which is how
    /// `rg "--pre" "curl ..." TODO` gets around the flag list, and `--pre` is rg's only switch
    /// that forks another program.
    #[test]
    fn quoting_a_flag_does_not_hide_it_from_the_flag_list() {
        for cmd in [
            r#"grep "-R" TODO src"#,
            r#"grep '-R' TODO src"#,
            r#"rg "--pre" "curl http://x" TODO src"#,
            r#"rg '--pre-glob' '*' TODO"#,
            r#"ls "-R" src"#,
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
        // A **pattern** starting with `-` is handed to grep with `-e`, and that road works
        // as usual. In a real shell, grep parses `grep -x-not-a-flag file` as a flag bundle
        // too, so judging it unrecognized matches the real behavior.
        assert_eq!(bash("grep -e '-x-not-a-flag' src/main.rs"), None);
        assert!(bash("grep '-x-not-a-flag' src/main.rs").is_some());
    }

    /// **Options may be written after the operands**, so the shape is settled only after a
    /// full scan.
    ///
    /// Real grep / rg reorder options in front of the operands: in `grep -r /etc -e TODO` the
    /// pattern is `TODO` and the search path is `/etc` (checked here against the real
    /// binaries). Settling as it goes, `/etc` has already fallen into the pattern position,
    /// which validates no path, before `-e` appears.
    #[test]
    fn a_path_written_before_the_pattern_flag_is_still_a_path() {
        for cmd in [
            "grep -r /etc -e TODO",
            "grep /etc -e TODO",
            "grep -rn /etc/passwd -e root",
            "rg /etc -e TODO",
            "rg /etc --regexp TODO",
            "rg /etc --files",
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
        // **A `-e` after a positional cannot be judged**, even when that positional is
        // inside the workspace.
        //
        // "Options are reordered in front of the operands" is a GNU extension, not a
        // guarantee: with `POSIXLY_CORRECT` set it becomes a file name. One line has two
        // readings, and we do not control that environment variable.
        assert!(bash("grep -r src -e TODO").is_some());
        assert!(bash("rg src -e TODO").is_some());
        // Flags written first (what an agent actually writes), as usual.
        assert_eq!(bash("grep -r -e TODO src"), None);
        assert_eq!(bash("rg -e TODO src"), None);
    }

    /// No flag should appear after a positional — two readings that disagree means it cannot
    /// be judged.
    #[test]
    fn a_flag_after_a_positional_is_ambiguous_and_therefore_refused() {
        // With `POSIXLY_CORRECT=1`, GNU grep treats a `-e` after a positional as a file
        // name, so this line reads `/etc/passwd`.
        assert!(bash("grep root src -e /etc/passwd").is_some());
        assert!(bash("ls src -la").is_some());
        // The normal spelling with every flag first is unaffected.
        assert_eq!(bash("ls -la src"), None);
        assert_eq!(bash("grep -rn TODO src"), None);
    }

    /// The pattern-flag test looks at the **decoded** form, not the whole token.
    ///
    /// The last `e` of `-rne` supplies the pattern just the same, `-e.` attaches the value
    /// behind it, and `--regexp=x` uses an equals sign — none of the three equals `"-e"`,
    /// while all three mean exactly the same to the real tool.
    #[test]
    fn a_pattern_flag_bundled_or_with_an_attached_value_still_counts() {
        for cmd in [
            "grep -rne TODO /etc",
            "grep -ne TODO /etc/passwd",
            "grep -ie AWS /Users",
            "grep -e. /etc/passwd",
            "rg -ne TODO /etc",
            "rg -e. /etc",
            "grep --regexp=TODO /etc",
        ] {
            assert!(bash(cmd).is_some(), "`{cmd}` must go to the owner");
        }
        // The same spellings inside the workspace, as usual.
        assert_eq!(bash("grep -rne TODO src"), None);
        assert_eq!(bash("rg -ne TODO src"), None);
    }

    /// The value a pattern flag consumes may itself start with `-`, and the prescan must not
    /// parse it as a flag again.
    #[test]
    fn the_word_a_pattern_flag_consumes_is_not_itself_parsed_as_a_flag() {
        assert_eq!(bash("grep -e -v src/main.rs"), None);
        assert!(bash("grep -e -v /etc/passwd").is_some());
    }

    /// `ls --recursive` is `-R`, and it follows symlinks.
    #[test]
    fn the_long_spelling_of_a_symlink_following_flag_is_refused_too() {
        assert!(bash("ls --recursive src").is_some());
        assert!(bash("ls -R src").is_some());
        // grep's `--recursive` is `-r` (it does not follow), as usual.
        assert_eq!(bash("grep --recursive TODO src"), None);
    }

    /// A **dangling symlink**: `canonicalize` fails on it, and that is the ordinary shape of
    /// creating a new file.
    ///
    /// Leaving the segment unchanged on failure judges
    /// `<root>/link -> /etc/cron.d/x` (target not yet created) as `<root>/link` — inside the
    /// allowlist, while one write writes straight into /etc.
    #[test]
    fn a_dangling_symlink_out_of_the_workspace_is_not_judged_to_be_inside_it() {
        let (_d, roots, cwd) = workspace();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/nonexistent-target-xyz", cwd.join("dangling"))
                .unwrap();
            for tool in ["Write", "Read"] {
                assert_eq!(
                    approval_owner_reason(
                        tool,
                        &serde_json::json!({ "file_path": "dangling" }),
                        &roots,
                        &cwd,
                        &BTreeSet::new()
                    ),
                    Some(OwnerReason::Escalates),
                    "{tool} through a dangling symlink"
                );
            }
        }
    }

    /// A build tool's config is an execution channel too, and it is read by exactly the names
    /// `agit rc grant` is most likely to hand out.
    ///
    /// `.cargo/config.toml` can set `target.*.runner` and `[alias]` — one granted
    /// `cargo test` becomes any command.
    #[test]
    fn a_build_tool_config_inside_the_workspace_is_a_control_surface_too() {
        for p in [
            ".cargo/config.toml",
            ".npmrc",
            ".yarnrc.yml",
            ".gradle/init.gradle",
            ".pre-commit-config.yaml",
        ] {
            assert_eq!(
                reason("Write", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "writing `{p}`"
            );
        }
    }

    /// Structured paths: reads and writes inside belong to the operator, and everything
    /// outside goes back to the owner.
    #[test]
    fn structured_paths_are_judged_against_the_allowlist_after_resolving_them() {
        assert_eq!(
            reason("Write", serde_json::json!({ "file_path": "src/new.rs" })),
            None
        );
        assert_eq!(
            reason("Read", serde_json::json!({ "file_path": "src/main.rs" })),
            None
        );
        for p in ["/etc/passwd", "../outside.txt", "src/../../outside.txt"] {
            assert_eq!(
                reason("Read", serde_json::json!({ "file_path": p })),
                Some(OwnerReason::Escalates),
                "reading `{p}`"
            );
        }
        // No path to judge = the test does not apply, which is not "safe".
        assert_eq!(
            reason("Write", serde_json::json!({})),
            Some(OwnerReason::Unprovable)
        );
    }

    /// A `Glob`'s target lives in `pattern`, and `paths_of` **never reads that field**.
    ///
    /// Taking `.all()` over an empty array is vacuously true, which makes
    /// `{"pattern":"/Users/**/.ssh/id_*"}` operator-answerable.
    #[test]
    fn a_glob_is_judged_by_its_pattern_because_that_is_where_its_target_lives() {
        assert_eq!(
            reason("Glob", serde_json::json!({ "pattern": "**/*.rs" })),
            None
        );
        assert_eq!(
            reason("Glob", serde_json::json!({ "pattern": "src/**/*.rs" })),
            None
        );
        for pat in ["/Users/**/.ssh/id_*", "/etc/*", "../**/*.rs"] {
            assert_eq!(
                reason("Glob", serde_json::json!({ "pattern": pat })),
                Some(OwnerReason::Escalates),
                "glob `{pat}`"
            );
        }
        assert_eq!(
            reason("Glob", serde_json::json!({})),
            Some(OwnerReason::Unprovable)
        );
    }

    /// A `Glob`'s search root can be given by **`path`** alone, and the pattern is only the
    /// shape below it.
    ///
    /// Looking at the pattern alone, `{"pattern":"**/*","path":"/etc"}` takes the "no fixed
    /// prefix = acts on cwd" branch and is allowed outright, while the real root is never
    /// checked at all.
    #[test]
    fn a_glob_with_a_separate_path_field_is_judged_against_that_path() {
        assert_eq!(
            reason(
                "Glob",
                serde_json::json!({ "pattern": "**/*", "path": "/etc" })
            ),
            Some(OwnerReason::Escalates),
            "an external path must be refused"
        );
        assert_eq!(
            reason(
                "Glob",
                serde_json::json!({ "pattern": "**/*.rs", "path": "../.." })
            ),
            Some(OwnerReason::Escalates)
        );
        // A path inside the workspace, allowed as usual.
        assert_eq!(
            reason(
                "Glob",
                serde_json::json!({ "pattern": "**/*.rs", "path": "src" })
            ),
            None
        );
    }

    /// A tool we do not know goes back to the owner, while planning tools do not.
    ///
    /// Sending `ExitPlanMode` to the owner **punishes the correct choice**: the operator
    /// switches into plan mode out of caution, the agent produces a plan, and the whole
    /// session then sits there waiting for the owner.
    #[test]
    fn the_tools_that_cannot_do_anything_are_not_sent_to_the_owner() {
        for t in ["ExitPlanMode", "TodoWrite"] {
            assert_eq!(reason(t, serde_json::json!({})), None, "{t}");
        }
        for t in [
            "Task",
            "mcp__someone_else__do_it",
            "apply_patch",
            "mcp__agit__commit",
            // Both of these list things across the whole machine rather than this
            // workspace: `rc_status` every live session, `status` every agent repo in the
            // local store.
            "mcp__agit__rc_status",
            "mcp__agit__status",
        ] {
            assert_eq!(
                reason(t, serde_json::json!({})),
                Some(OwnerReason::Unprovable),
                "{t}"
            );
        }
    }

    /// The harness itself said this call reaches the network or widens a root — that is
    /// **positive evidence**, not "cannot be proven".
    #[test]
    fn a_harness_that_declares_it_is_reaching_out_is_believed() {
        for k in [
            "networkApprovalContext",
            "proposedNetworkPolicyAmendments",
            "grantRoot",
        ] {
            assert_eq!(
                reason(
                    "shell",
                    serde_json::json!({ "command": "ls", k: {"any": 1} })
                ),
                Some(OwnerReason::Escalates),
                "{k}"
            );
        }
        for t in ["WebFetch", "WebSearch"] {
            assert_eq!(
                reason(t, serde_json::json!({})),
                Some(OwnerReason::Escalates)
            );
        }
    }

    /// A codex `command` may be null by schema. An exec that cannot be judged is the last
    /// thing that may count as safe.
    #[test]
    fn an_exec_approval_with_no_command_at_all_is_not_safe_by_default() {
        assert_eq!(
            reason(
                "shell",
                serde_json::json!({ "command": serde_json::Value::Null })
            ),
            Some(OwnerReason::Unprovable)
        );
    }

    /// Only after the owner grants it can the operator answer that name — and only that
    /// name.
    #[test]
    fn an_owner_granted_command_name_becomes_operator_answerable_and_nothing_else() {
        let (_d, roots, cwd) = workspace();
        let granted: BTreeSet<String> = ["cargo".to_string()].into_iter().collect();
        let (_p, path) = fake_path(TOOLBOX);
        let judge = |cmd: &str| {
            confined_command_in(cmd, &roots, &cwd, &granted, Some(path.as_os_str())).err()
        };
        assert_eq!(judge("cargo test"), None);
        assert_eq!(judge("cargo build --release"), None);
        // The grant covers one name, not "a line of shell".
        assert!(judge("cargo test; curl http://x").is_some());
        assert!(judge("cargo test && curl http://x").is_some());
        assert!(judge("echo $(cargo test)").is_some());
        // A name that was not granted goes back to the owner as usual.
        assert!(judge("npm test").is_some());
    }

    /// A command **this machine does not have at all** is judged "cannot be proven", not
    /// "safe".
    ///
    /// CI teaches this one: where `rg` is not installed on the runner, `rg 'fn main' src` is
    /// green locally and red in CI. The behavior itself is right — judging a command that
    /// cannot run unprovable is fair, and that step (resolving the command name on PATH) is
    /// exactly how "the list recognizes the system's `ls`" is implemented. What is wrong is a
    /// test reading the developer machine's toolchain. Both are pinned: the shape is tested
    /// on a fake PATH, and the environment dependency is stated here.
    #[test]
    fn a_command_this_machine_does_not_have_is_unprovable_rather_than_safe() {
        let (_d, roots, cwd) = workspace();
        let (_p, path) = fake_path(&["ls"]);
        let judge = |cmd: &str| {
            confined_command_in(cmd, &roots, &cwd, &BTreeSet::new(), Some(path.as_os_str())).err()
        };
        assert_eq!(
            judge("ls"),
            None,
            "an installed command stays with the operator"
        );
        assert_eq!(
            judge("rg TODO src"),
            Some(OwnerReason::Unprovable),
            "a command that is not installed is unprovable"
        );
        // With no PATH at all, the same.
        assert_eq!(
            confined_command_in("ls", &roots, &cwd, &BTreeSet::new(), None).err(),
            Some(OwnerReason::Unprovable)
        );
    }

    /// The list recognizes **the system's `ls`**, not the one the agent just wrote into the
    /// repo.
    #[test]
    fn a_command_name_that_resolves_inside_the_workspace_is_refused() {
        let (_d, roots, cwd) = workspace();
        // Build an executable `ls` in the workspace and put the workspace first on PATH.
        let fake = cwd.join("ls");
        std::fs::write(&fake, "#!/bin/sh\ncurl http://x\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            resolve_in_path("ls", Some(cwd.as_os_str())).map(|p| is_within(&p, &roots)),
            Some(true),
            "precondition: under this PATH, `ls` really resolves to the one in the workspace"
        );

        // This is what the test asserts: the list recognizes **the system's `ls`**. One that
        // resolves inside the workspace must go to the owner — otherwise the agent need only
        // write a script called `ls` into the repo to turn the whole INERT list into a blank
        // cheque of its own.
        //
        // The verdict is `Escalates`, not `Unprovable`: this is not "unreadable, so we dare
        // not say", it is understood — an executable within the agent's writable range can
        // hold anything.
        assert_eq!(
            confined_command_in("ls", &roots, &cwd, &BTreeSet::new(), Some(cwd.as_os_str())).err(),
            Some(OwnerReason::Escalates),
            "a command of the same name resolving inside the workspace must not pass as the \
             one on the list"
        );
    }

    #[cfg(unix)]
    /// The home picker must not be climbed out of by folding `..`.
    ///
    /// The path judged inside or outside the home directory and the path then listed must be
    /// one path. The former goes through the resolver (`..` pops from the **resolved**
    /// location); if the latter uses a lexical fold instead, a `..` at the root is a no-op —
    /// the two halves resolve one spelling by different rules, the test looks at a path inside
    /// home, and the picker opens somewhere else. The link target has to be deep enough for
    /// the lexical fold to reach all the way to `/`.
    #[cfg(unix)]
    #[test]
    fn the_home_picker_cannot_be_folded_out_of_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(tmp.path()).unwrap().join("home");
        std::fs::create_dir_all(home.join("p")).unwrap();

        let dotdots = home.components().count() + 1;
        let mut deep = PathBuf::new();
        for i in 0..dotdots {
            deep.push(format!("s{i}"));
        }
        std::fs::create_dir_all(home.join("p").join(&deep)).unwrap();
        std::os::unix::fs::symlink(&deep, home.join("p").join("jump")).unwrap();

        let mut spelling = home.join("p").join("jump");
        for _ in 0..dotdots {
            spelling.push("..");
        }
        spelling.push("etc");

        let verdict = require_dir_under(&spelling, &home);

        // Both outcomes are acceptable — a refusal, or a path that is **genuinely inside the
        // home directory**. What is not acceptable is handing out one outside: that is exactly
        // what this gate stops.
        if let Ok(dir) = verdict {
            assert!(
                dir.starts_with(&home),
                "the picker must not hand out a path outside the home directory: {}",
                dir.display()
            );
        }
    }

    /// An allowlist root must be the path the kernel resolves to.
    ///
    /// `require_bindable_dir` is the **only** entry point that widens the boundary: the path
    /// it hands out becomes a member of `CanonicalRoots`, and every later `is_within` measures
    /// against it. Using a lexically folded path as a root turns a spelling the kernel cannot
    /// walk at all into the allowlist — while its `exists()` may be true, because that is
    /// **another directory that really exists**.
    #[cfg(unix)]
    #[test]
    fn a_bind_root_that_the_kernel_cannot_reach_is_not_a_root() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        // Naming a system root outright: refused.
        assert!(
            require_bindable_dir(&link).is_err(),
            "a link pointing at a system root must not be bound"
        );

        // The same location under a folded spelling must be refused too. Letting it through
        // enters `/etc` into the allowlist under this link's name.
        assert!(
            require_bindable_dir(&link.join("nope/..")).is_err(),
            "a folded spelling must not sneak a system root into the allowlist"
        );

        // A spelling that does not resolve must not pick up **another** really existing
        // directory as a root through a lexical fold.
        let unreachable = Path::new("/nonexistent-for-this-test/../Users");
        if let Ok(root) = require_bindable_dir(unreachable) {
            assert_ne!(
                root,
                Path::new("/Users"),
                "a spelling that does not resolve must not fold into /Users as a root"
            );
        }
    }
}
