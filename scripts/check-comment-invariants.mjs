#!/usr/bin/env node
// Comments state invariants only: scan the comment lines a branch **adds** against the merge base.
//
// The prose half of the rule is the "Comments state invariants, not history" section of
// AGENTS.md; this is its machine half: prose is enforced by remembering, an exit code is not.
// Two repositories each keep a verbatim-identical copy and share this one contract — the rule
// is written in both AGENTS.md files, and a gate that runs on one side only holds back half.
//
// ── Why only added lines ──────────────────────────────────────────────────
// Existing comments are full of sentences like these. Checking all of them at once is a wall on
// day one, and the only ending a wall has is being switched off. Firing only on comment lines
// added after the merge base, this gate does one thing: no more of them.
//
// ── What the machine judges, what a person judges ─────────────────────────
// The machine knows a few shapes it can settle at a glance: numbers with units, ratios, counts
// of tests or table entries, wording that points at another implementation or another round of
// review. **What a sentence is trying to say is still for a person to judge** — what the machine
// hits is "something here will expire"; the fix (state an invariant, or delete it) is the
// person's business.
//
// ── Numbers with units: the test is the shape, the exit is an exemption ───
// The machine cannot tell an observed number from a guaranteed one: `ask the store every N
// seconds` is this system's own contract, `one run stalled every N seconds` reports on that
// machine, and the two sentences have exactly the same shape.
//
// What cannot be told apart must not be guessed. Guessing from observational wording
// (`observed`, `ran`, `took`) hands the test to the verb: the writer picks another verb and
// walks straight through, and the thing this gate claims to watch goes unwatched — a gate you
// get around by changing a word differs from no gate only in that it still prints "nothing
// found".
//
// So the test is the shape: a number with a unit always counts as a **candidate**. The numbers
// that genuinely must stay in a comment — this system's own contract, a hard cap imposed from
// outside, a transcript of a constant in the code — carry `comment-rule-allow: reason` on the
// same line. The trade has a cost: contract numbers are many, and each one costs an explicit
// exemption. The cost goes on this side, because the cost on the other side is a gate that works
// only while nobody routes around it; an exemption is at least a decision written into the code,
// visible, and open to objection in review.
//
// Wording signals have no such exit, so they still miss rather than convict: `used to` can mean
// "it once was" or "it serves to", and `上一轮` can mean the previous retry; a shape with a
// second reading counts as a hit only where it is unambiguous. A number with a unit has a shape
// to judge and an exit whose reason can be written down; wording has neither, and a wrong guess
// convicts the innocent — a gate that convicts the innocent gets switched off, and once it is
// off, what it could rightly have caught goes with it.
//
// The same line is drawn through past-tense wording: naming another implementation hits, while
// "there was an `events_since` here; when it is wanted, write it the way `events_tail` is
// written" says **how the absent thing is restored** — a constraint on future changes, not a
// hit. `曾经` / `历史上` / `修复前` / `previously` are used overwhelmingly in the latter sense
// across these two repositories (also the domain sense on the `ever_dangerous` path, "it once
// ran with no check", and git history as in "on a completely healthy history"), so they are not
// signals.
//
// ── Which files are scanned ───────────────────────────────────────────────
// The `//` and `/*…*/` family (Rust / JS / TS), which is this repository's source. Strings,
// character literals, Rust raw strings and JS regex literals all have to be recognized and
// skipped: `let s = "// TODO"` is code, not a comment, and counting it is the kind of false
// positive that teaches people to switch the whole gate off. Of a template literal only the
// quoted half is skipped: inside `${…}` it is JS code again, and a `/* … */` there is a real
// comment.
//
// The other way round, **an unpaired quote must not swallow the rest of the file**: take a lone
// apostrophe for the start of a string and every comment after it in this file disappears at
// once — which is this gate's worst failure mode (exit 0, saying "nothing found"). So a quote
// that does not pair is walked as an ordinary character.
//
// JSX text is the same thing one layer further down: child text has **no lexis**, and backticks,
// apostrophes and `//` are all literal text there. Whether a character opens some kind of
// literal is not answered by the characters around it but by which stretch it fell into — so JSX
// follows the lexical state (see `tag` / `text` / `brace` in `scanOnce`) instead of guessing at
// one backtick from its context. Anything still open at end of file is demoted and rescanned.
//
// The exit code is the only criterion: exit 1 on a problem, 0 otherwise. An unreadable base, a
// missing diff, a file that cannot be read — each **errors out** rather than passing silently: a
// gate that can silently do nothing is worse than no gate. Likewise, a quoted path from git
// (non-ASCII, or any special byte) has to be **unquoted**, never skipped because its extension
// went unrecognized.

import { readFileSync, realpathSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

// ── The exemption marker ──────────────────────────────────────────────────
// A line carrying `comment-rule-allow: <reason>` is skipped. The reason must not be empty: an
// exemption without a reason is a silent bypass.
//
// It matches the **comment text**, not the raw line. Against the raw line, the ` */` closing a
// block comment counts as the reason, so `/* … comment-rule-allow: */` becomes a silent blanket
// bypass — worse than the exemption it means to stop, because it swallows the violation too.
const ALLOW = /comment-rule-allow:[ \t]*(\S.*)?$/

// The `jsx` / `js` split follows TypeScript's own: in `.ts`, `<T>x` is a type assertion and JSX
// is a syntax error; in `.tsx` it is exactly the other way round. Reading both as JSX makes
// every type assertion in `.ts` an opening tag that swallows the rest of the file and is only
// recovered by the demotion rescan — the result is right, but every assertion costs one more
// pass.
const EXTENSIONS = new Map([
    ['.rs', 'rust'],
    ['.mjs', 'js'],
    ['.cjs', 'js'],
    ['.js', 'jsx'],
    ['.jsx', 'jsx'],
    ['.ts', 'js'],
    ['.tsx', 'jsx'],
])

// Build output and the dependency tree: comments in these directories are not written by this
// repository and are not bound by this rule.
const SKIP_DIRS = /(^|\/)(node_modules|target|dist|build|coverage|vendor|\.cache|\.git)\//

// The rule's own two copies are not bound by the rule: a specification has to be able to show
// the shapes it forbids, or every example needs another exemption marker, and an escape hatch
// that appears everywhere is no longer a visible decision. What is exempt is **giving
// examples**, not everything in these two files — their own prose still follows the rule, pinned
// by the self-check regression in `check-comment-invariants.test.mjs`.
//
// The exemption is **derived** from `import.meta.url` rather than listed as a table of canonical
// paths: this file is verbatim-identical in two repositories, so a static table would have to
// list both sides' paths, and each copy would then hand out an exemption for the **other
// repository's** path — a new file of that name on the other side is skipped whole, while having
// nothing to do with the gate that is running. Verbatim-identical demands that both sides
// compute the same answer, not that they share one table: the derived pair is always **the
// checker that is running** and the test file next to it.
//
// The comparison is by real path, not by repository-relative path: a symlink, or another way of
// spelling a relative path, must not change the answer to "is this the same file". This is the
// gate's only skip by path, so it must not be silent: how many files were skipped goes into the
// summary line.
const realPath = (path) => {
    try {
        return realpathSync(path)
    } catch {
        return resolve(path)
    }
}
const SELF_MODULE = fileURLToPath(import.meta.url)
const SELF = new Set([SELF_MODULE, SELF_MODULE.replace(/\.mjs$/, '.test.mjs')].map(realPath))
const isSelf = (path) => SELF.has(realPath(path))

// The Chinese in every class and pattern below is behavior, not prose (AGENTS.md exception ii):
// these are character classes matching another script, and they stay byte-identical.
//
// Chinese numerals as one character class, so a duration written in Han digits and one written
// in Arabic digits fall into the same signal.
const CN_NUM = '[零一二两三四五六七八九十百千万几]'
// Tally: a Chinese numeral has to carry a place (十/百/千/万). `一条测试` and `两个用例` are
// measure words, not a tally.
const CN_TALLY = `${CN_NUM}*[十百千万]${CN_NUM}*`
// Times: `每八次…两次` is a ratio, `每一次…这一次` is not. So `一` and `几` do not count as
// times.
const CN_TIMES = `(?:${CN_TALLY}|[两三四五六七八九])`

// ── Numbers with units: the shape ─────────────────────────────────────────
const NUM = '\\d+(?:[.,]\\d+)?'
// `(?![1-5]\d\ds\b)`: `404s` and `500s` are plural HTTP status codes, not seconds.
const UNIT =
    '(?:ns|µs|μs|ms|sec|secs|seconds?|s|min|mins|minutes?|hours?|h|MiB|GiB|KiB|TiB|kB|[KMGT]B|bytes?)'
const CN_UNIT = '(?:纳秒|微秒|毫秒|秒钟|秒|分钟|小时|字节)'
// `几` is not a quantity: `几分钟` carries no number to check, to move into an assertion, or to
// expire; it gives an order of magnitude. The test is still the shape — and replacing `210 秒`
// with `几分钟` is exactly the fix this rule asks for.
const CN_QTY = '[零一二两三四五六七八九十百千万]'
// The percent sign has to sit **against** the number: `92.3%` is a percentage, `8192 % 8` is a
// modulo.
// The numeral branch must not start in the middle of a run of numerals: without this check,
// `百毫秒` inside `几百毫秒` starts a second match and an order-of-magnitude word turns back
// into a quantity.
const QTY =
    `(?:(?<![\\w.$])(?![1-5]\\d\\ds\\b)${NUM}(?:[ \\t]*${UNIT}|%)(?![\\w])` +
    `|(?:${NUM}|(?<!${CN_NUM})${CN_QTY}+)[ \\t]*${CN_UNIT})`

// ── Signals ───────────────────────────────────────────────────────────────
// Each signal is a **candidate**, not a verdict: `why` says why this shape will expire, `fix`
// says what it usually becomes.
const SIGNALS = [
    {
        id: 'number-with-unit',
        why: 'a number with a unit is either an observation or a transcript of some constant: the first stops holding on another machine, and on the day the second changes nobody comes back to fix this sentence',
        fix: 'state the guaranteed property; if the number is load-bearing, put it in an assertion or a benchmark, where something checks it. What genuinely must stay (a contract of this system, a hard cap from an external system) carries `comment-rule-allow: reason` on the same line',
        patterns: [new RegExp(QTY, 'gi')],
    },
    {
        id: 'ratio',
        why: '"red so many times out of so many" reports the load on that machine at that moment, not a property of the code',
        fix: 'state the property this gate pins, and how a wrong implementation escapes it',
        patterns: [
            /(?<![\w.])\d+[ \t]+of[ \t]+\d+(?![\w.])/gi,
            /(?<![\w.])best[ \t-]of[ \t-]\d+(?![\w.])/gi,
            /(?<![\w.])\d+[ \t]*\/[ \t]*\d+[ \t]*(?:runs?|tests?|cases?|次|全量)/gi,
            new RegExp(`每[ \\t]*(?:\\d+|${CN_TIMES})[ \\t]*次[^。；\\n]{0,16}(?:\\d+|${CN_TIMES})[ \\t]*次`, 'g'),
        ],
    },
    {
        id: 'count',
        why: 'a count of tests or of table entries expires on the next addition or deletion, and nothing updates this sentence',
        fix: 'say what this table, this set of tests guarantees; do not say how many of them there are',
        patterns: [
            /(?<![\w.])\d+[ \t]+(?:tests?|assertions?|cases?|entries|entry|runs?)(?![\w])/gi,
            // A tally says **how many there are now**. Preceded by `约` / `最多` / `不到` it
            // is an estimate or an upper bound; followed by `上限` / `容量` / `容器` it is the
            // capacity constant in the code — neither end is a tally, and neither expires
            // because somebody added a case.
            new RegExp(
                // The first lookbehind is for the numerals themselves: without it,
                // `百万个条目` inside `约八百万个条目` restarts the match from the middle and
                // an estimated upper bound turns back into a tally.
                `(?<![\\d零一二两三四五六七八九十百千万几])(?<!(?:大?约|最多|至多|不到|超过|近))` +
                    `(?:\\d+|${CN_TALLY})[ \\t]*[条个项张][ \\t]*(?:测试|用例|断言|条目|表项|目录项|名字|entries)` +
                    `(?![ \\t]*的?(?:上限|容量|容器|预算|配额))`,
                'g',
            ),
        ],
    },
    {
        id: 'history',
        why: '"how the previous version got it wrong" has no referent once that version is gone, and the reader still takes it as true',
        fix: 'keep the failure **shape** (it explains why the rule exists) and drop that it once happened; that sentence belongs in the commit message',
        // A shape with a second reading never counts as a hit; only the ones with no other
        // reading stay: `used to` can also mean "serves to", `当初` can mean the transaction of
        // the time, `上一轮` can mean the previous retry.
        //
        // The words kept out for the same reason each carry a second reading, and in these two
        // repositories that reading is the mainstream one:
        //   `曾经` — "there was an X here (deleted)" constrains a future change; on the
        //            `ever_dangerous` path, "it once ran with no approval" is the runtime state
        //            of the session.
        //   `历史上` — "erroring on a completely healthy history" is git history.
        //   `修复前` — "falls back to the pre-fix behavior" states the guarantee after
        //              degrading.
        //   `previously` — the same ambiguity in English: `a previously unknown active turn`
        //                  and `previously committed but not yet pushed` both describe runtime
        //                  state, not another implementation. The shape where `used to` is
        //                  followed directly by `be` has no such second reading, so it stays.
        // `before this change` likewise: `change` is a domain noun in these two repositories (a
        // visibility change), so only `before this <fix|commit|patch>` stays.
        patterns: [
            /\b(?:first|second|third|previous|earlier|older|old|original|last)[ \t]+(?:version|revision|implementation|round)\b/gi,
            /\b(?:it|this|that|they|we|there)[ \t]+used[ \t]+to\b|\bused[ \t]+to[ \t]+be\b/gi,
            // **"we have not measured this"** is not an observation report; it constrains a
            // future change.
            /(?<!not[ \t])(?<!never[ \t])\bmeasured\b|\bregressed\b/gi,
            /\bbefore[ \t]+(?:this|the)[ \t]+(?:fix|commit|patch)\b/gi,
            /\breview[ \t]+round\b|\bthe[ \t]+(?:old|previous)[ \t]+fix\b/gi,
            /上一版|前一版|上个版本|原先|先前|从前|第[一二三]版|实测/g,
            /(?:这次|本次|这轮|上次|上一次|上一轮)[ \t]*review/gi,
        ],
    },
]

// ── git ───────────────────────────────────────────────────────────────────
const git = (...args) => {
    const r = spawnSync('git', args, { encoding: 'utf8', maxBuffer: 1 << 28 })
    return r.status === 0 ? r.stdout : null
}

const die = (...lines) => {
    for (const l of lines) console.error(l)
    process.exit(2)
}

function resolveBase(explicit) {
    if (explicit) {
        const sha = git('rev-parse', '--verify', `${explicit}^{commit}`)
        if (!sha) die(`cannot resolve --base ${explicit}`)
        return sha.trim()
    }
    // GitLab hands the merge base straight to an MR pipeline; with it there is no need to look
    // for the default branch.
    const given = process.env.CI_MERGE_REQUEST_DIFF_BASE_SHA
    if (given) {
        const sha = git('rev-parse', '--verify', `${given}^{commit}`)
        if (sha) return sha.trim()
        die(
            `CI_MERGE_REQUEST_DIFF_BASE_SHA=${given} does not exist in this clone.`,
            'most likely a shallow clone: set GIT_DEPTH: 0 on this job.',
        )
    }
    const named = process.env.CI_DEFAULT_BRANCH
    const candidates = [
        ...(named ? [`origin/${named}`, named] : []),
        'origin/main',
        'origin/master',
        'main',
        'master',
    ]
    for (const c of candidates) {
        const mb = git('merge-base', 'HEAD', c)
        if (mb) return mb.trim()
    }
    die(
        `no merge base found: tried ${candidates.join(', ')}, none of them resolves.`,
        'name one with --base <ref>, or fetch the default branch.',
    )
}

// git quotes a path C-style when it holds non-ASCII or special bytes (`"b/src/\346\265\213.rs"`).
// Left quoted, the extension reads as `.rs"` and the file is skipped silently — and a silent
// skip is the very thing this gate exists for.
const C_ESCAPES = { n: 10, t: 9, r: 13, f: 12, b: 8, v: 11, a: 7, '\\': 92, '"': 34 }

export function unquotePath(raw) {
    if (!raw.startsWith('"') || !raw.endsWith('"') || raw.length < 2) return raw
    const body = raw.slice(1, -1)
    const bytes = []
    for (let i = 0; i < body.length; i++) {
        const c = body[i]
        if (c !== '\\') {
            for (const b of Buffer.from(c, 'utf8')) bytes.push(b)
            continue
        }
        const e = body[++i]
        if (e === undefined) break
        if (e in C_ESCAPES) {
            bytes.push(C_ESCAPES[e])
            continue
        }
        if (e >= '0' && e <= '7') {
            bytes.push(parseInt(body.slice(i, i + 3), 8) & 0xff)
            i += 2
            continue
        }
        for (const b of Buffer.from(e, 'utf8')) bytes.push(b)
    }
    return Buffer.from(bytes).toString('utf8')
}

// `git diff <base>` compares base against the **worktree**, so uncommitted changes count on a
// local run; in CI the worktree is clean, which is equivalent to base..HEAD. File contents are
// read from the worktree too, so the two always agree.
function addedLines(base) {
    const diff = git('diff', '--no-color', '--unified=0', '--diff-filter=ACMR', base, '--')
    if (diff === null) die(`git diff ${base} failed`)

    const byFile = new Map()
    let file = null
    for (const line of diff.split('\n')) {
        if (line.startsWith('+++ ')) {
            const path = unquotePath(line.slice(4).trim())
            file = path === '/dev/null' ? null : path.replace(/^b\//, '')
            continue
        }
        if (!file || !line.startsWith('@@')) continue
        const m = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@/.exec(line)
        if (!m) continue
        const start = Number(m[1])
        const count = m[2] === undefined ? 1 : Number(m[2])
        if (count === 0) continue
        let set = byFile.get(file)
        if (!set) byFile.set(file, (set = new Set()))
        for (let n = start; n < start + count; n++) set.add(n)
    }

    // An untracked new file is not in the diff, but all of it is added. `-z` makes git emit the
    // path verbatim, so there is no quoting and nothing to unquote.
    const untracked = git('ls-files', '--others', '--exclude-standard', '-z')
    if (untracked !== null) {
        for (const path of untracked.split('\0').filter(Boolean)) {
            if (!EXTENSIONS.has(extensionOf(path)) || SKIP_DIRS.test(path)) continue
            let count
            try {
                count = readFileSync(path, 'utf8').split('\n').length
            } catch {
                continue
            }
            const set = new Set()
            for (let n = 1; n <= count; n++) set.add(n)
            byFile.set(path, set)
        }
    }
    return byFile
}

function extensionOf(path) {
    const i = path.lastIndexOf('.')
    return i < 0 ? '' : path.slice(i)
}

// ── Comment extraction ────────────────────────────────────────────────────
// One character-by-character pass skips strings, character literals, Rust raw strings and JS
// regex literals, and files the text landing inside a comment by line. A line-by-line regex
// cannot do this: quoted code can hold something that looks like a comment, and a block comment
// spans lines.
export function commentSegments(text, lang) {
    // Anything still open at end of file is not what it claimed to be: a backtick that opens no
    // template, a `<` that is really a type assertion — each swallows the rest of the file as
    // template body or as JSX text. "There is another backtick further on" does not prove **this
    // one** pairs with it, and nothing but walking the file replaces walking the file — so let
    // the main loop walk it, record the opener that never closed, and walk again with that
    // opener demoted to an ordinary character.
    //
    // Every pass removes at least one opener, so it terminates — and that premise requires every
    // opener to pass `plain`. It is checked here, on the spot, rather than by reading every place
    // that pushes: miss one and the next pass is identical to this one, and the loop never gets
    // out. A gate that hangs is worse than a gate that fires wrongly: it does not even say
    // whether it found anything, and all CI shows is a timeout. When nothing can be removed, hand
    // back this pass's result and stop.
    const plain = new Set()
    for (;;) {
        const { perLine, unclosed } = scanOnce(text, lang, plain)
        if (unclosed === undefined) return perLine
        const before = plain.size
        plain.add(unclosed)
        if (plain.size === before) return perLine
    }
}

// ── JSX: child text is not code ───────────────────────────────────────────
// Text inside JSX children has no lexis: backticks, apostrophes, `//` and `/*` are all literal
// text there, and only `<` and `{` still mean anything. So "can this backtick open a template"
// is the wrong question inside JSX text — neither the character to its left nor the presence of
// another backtick to its right answers it, because the answer does not turn on the backtick, it
// turns on which stretch it fell into. The test is therefore the **state**: JSX follows the
// lexical state, with one set of rules inside a tag, another in child text, another inside a
// `{…}` container, and inside the container it is JS code again (`{/* … */}` is a real comment
// for that reason).
//
// The step into that state is still judged: `<` can also be a less-than, a TS type assertion, a
// type parameter list. The test is the same one as for a regex literal — a JSX element is a
// primary expression and appears only at the start of an expression, while the `<` in `a < b`
// follows an operand. Plus a shape: `<` is followed either directly by a tag name whose end is
// whitespace, `/`, `>` or `{`, or by the `>` of `<>`. An empty name is spelled `<>` and nothing
// else: count `< ` as a fragment too and the `a < b` in child text becomes an opening tag whose
// "child text" swallows everything to end of file.
//
// Entering wrongly is not silent either: a wrong entry swallows to end of file, and what is
// still open at end of file is demoted and rescanned (see `commentSegments`). The worst outcome
// is one more pass, not a hidden comment.
//
// The shapes it does not recognize are listed here, and all of them land on the "one more pass"
// or "walk it as an ordinary character" side:
//   * `.ts` does not look for JSX; the reason is in `EXTENSIONS`.
//   * a generic arrow in `.tsx` is routed away **before** this state, and the test is what the
//     angle brackets hold (see `genericArrowHead`).
//   * the attribute list is opaque (see the `tag` branch), so in `<Foo<Bar> x={1}>` with an
//     explicit type argument the tag ends at the first `>` and `x={1}` falls into child text.
//   * a closing tag whose name does not match is not this layer's business: `<a>…</b>` is not
//     valid JSX to begin with, so this layer swallows to end of file and is then demoted.
//   * the `<` and `{` in child text have no position to judge by: valid JSX child text may not
//     hold a bare `<` or `{`, so a `<` there can only open a tag and a `{` can only open a
//     container. This gate is therefore wider than the code-position one — so demotion starts
//     from the innermost layer, removing what came in through this one first (see the return
//     value of `scanOnce`).
//
// The one shape left that still hides a comment silently is **a misread that happens to close**:
// a `<Name …>` that is not an element, followed by a closing tag that shuts it. Everything that
// does not close is caught back by the demotion rescan, so what remains is this one shape, not a
// list enumerated by spelling. Demotion cannot save it — a `</T>` anywhere inside a string closes
// the misread, and a closed misread is silent. So every reading that can be settled at the `<`
// itself is settled there, never left to "does it close later" (see `genericArrowHead`).
//
// Once what can be settled is settled, one entry into that class remains: the parameter lists
// `genericArrowHead` cannot walk to the end (an unmatched quote, unbalanced brackets). Failing to
// finish falls back to JSX, and after falling back only the layer that does not close is caught.
// Identifiers follow the ECMAScript definition, not ASCII: `Ä`, `変数` and `_$` are all valid
// names, and `Ä` is another spelling of the same name. These tables are load-bearing in
// different places, but missing one character has the same consequence everywhere: two readings
// that were separable at some position stop being separable, and the case falls into "misread
// and balanced" — the silent class.
//
// ZWNJ / ZWJ are legal only inside a name, so they go only into `ID_PART`.
const ID_START = String.raw`[\p{ID_Start}$_]`
const ID_PART = String.raw`[\p{ID_Continue}$\u200C\u200D]`
// An escape spelling is legal only in a real identifier, never in a tag name (JSX names do not
// take escape sequences).
const ID_ESCAPE = String.raw`\\u(?:[0-9a-fA-F]{4}|\{[0-9a-fA-F]+\})`
const IDENT = `(?:${ID_START}|${ID_ESCAPE})(?:${ID_PART}|${ID_ESCAPE})*`
// A tag name allows `-` on top of an identifier, plus the namespaced and member spellings `.`
// and `:`.
const JSX_NAME = `${ID_START}(?:${ID_PART}|[.:-])*`

const JSX_OPEN = new RegExp(`<(?:(${JSX_NAME})(?=[\\s/>{])|(?=>))`, 'uy')
const JSX_CLOSE = new RegExp(`</(${JSX_NAME})?[ \\t\\r\\n]*>`, 'uy')
const matchAt = (re, text, i) => {
    re.lastIndex = i
    return re.exec(text)
}

function scanOnce(text, lang, plain) {
    const perLine = new Map()
    let line = 1
    let buf = ''
    const flush = () => {
        if (buf.trim()) {
            const prev = perLine.get(line)
            perLine.set(line, prev === undefined ? buf : `${prev} ${buf}`)
        }
        buf = ''
    }
    // A line comment eats to the newline; end of line is its boundary. Code position and a JSX
    // opening tag both use this one: comments are allowed between attributes in a tag, and a `//`
    // there is the same thing as a `//` at code position.
    const lineComment = (from) => {
        let end = text.indexOf('\n', from)
        if (end < 0) end = text.length
        buf += text.slice(from, end)
        flush()
        return end
    }

    const js = lang !== 'rust'
    const jsx = lang === 'jsx'
    const n = text.length
    let i = 0
    let depth = 0 // block comment depth: Rust block comments nest, JS ones do not
    let lastSig // the most recent non-whitespace code character; only tells `/` regex from division
    // The lexical state stack. Each layer records the character it opened on, and that index is
    // what the demotion rescan removes.
    //   `template`: `braces < 0` is the quoted half, `>= 0` is the count of `{` still open inside
    //               `${…}`. Inside `${…}` it is JS code again, the comments there are real
    //               comments, and another template can nest.
    //   `tag`: inside a `<Name …`.
    //   `text`: the child text of an element.
    //   `brace`: a `{…}` container in JSX, JS code again inside.
    const stack = []

    while (i < n) {
        const c = text[i]
        const c2 = text[i + 1]
        const frame = stack[stack.length - 1]

        if (depth > 0) {
            if (lang === 'rust' && c === '/' && c2 === '*') {
                depth++
                buf += c + c2
                i += 2
                continue
            }
            if (c === '*' && c2 === '/') {
                depth--
                i += 2
                if (depth === 0) flush()
                continue
            }
            if (c === '\n') {
                flush()
                line++
                i++
                continue
            }
            buf += c
            i++
            continue
        }

        // The quoted half of a template: no comments here, only escapes, `${` and the closing
        // backtick.
        if (frame !== undefined && frame.kind === 'template' && frame.braces < 0) {
            if (c === '\\') {
                // The escaped character can still be a newline — a line continuation inside a
                // template is valid JS. Skipping these two characters without counting `line`
                // puts every comment after it one line number low, and the added-line filter
                // drops it on that basis: the gate then exits 0 saying "nothing found". `\r\n`
                // skips only to `\r`; the remaining `\n` is counted by the shared newline branch
                // below.
                if (c2 === '\n') line++
                i += 2
                continue
            }
            if (c === '`') {
                stack.pop()
                lastSig = 'x'
                i++
                continue
            }
            if (c === '$' && c2 === '{') {
                frame.braces = 0
                // The start of an interpolation is the start of an expression, so a `/` there
                // opens a regex.
                lastSig = undefined
                i += 2
                continue
            }
            if (c === '\n') line++
            i++
            continue
        }

        // Inside a JSX opening tag. Between attributes is **a position where a comment can
        // appear** (`<Foo // why it is passed this way`, newline, then more attributes), so a
        // `//` or `/*` in a tag is the same thing as one at code position: not recognizing them
        // turns a comment written on an attribute into a stretch of unread text inside the tag,
        // gone silently.
        //
        // Quotes in an attribute value have **no escapes** (`title="C:\"` ends at that quote) and
        // do span lines, so they cannot go through `skipQuoted`: that one follows the JS rules,
        // where anything not paired within the line is not a string, so a multi-line attribute
        // value would cut this element short and the child text behind it would fall back to code
        // position.
        if (frame !== undefined && frame.kind === 'tag') {
            if (c === '\n') {
                line++
                i++
                continue
            }
            if (c === '/' && c2 === '/') {
                i = lineComment(i)
                continue
            }
            if (c === '/' && c2 === '*') {
                depth = 1
                i += 2
                continue
            }
            if (c === '"' || c === "'") {
                const end = text.indexOf(c, i + 1)
                // This quote runs to end of file: this layer can no longer close, the remaining
                // characters cannot change that, so stop and go demote. The top of the stack is
                // this layer, so "demote the innermost layer" below picks up exactly it.
                if (end < 0) break
                line += countNewlines(text, i, end + 1)
                i = end + 1
                continue
            }
            if (c === '{' && !plain.has(i)) {
                stack.push({ kind: 'brace', at: i, braces: 0 })
                lastSig = undefined
                i++
                continue
            }
            if (c === '/' && c2 === '>') {
                stack.pop()
                lastSig = 'x'
                i += 2
                continue
            }
            if (c === '>') {
                frame.kind = 'text'
                i++
                continue
            }
            // The rest of the attribute list is opaque. "This is an element" was decided back
            // at `<Name`, and nothing appearing in the attributes overturns it — under "a tag may
            // hold only these characters", `<Foo<Bar> x={1}>` with an explicit type argument would
            // be demoted wholesale to ordinary characters, its child text would fall back to code
            // position, a backtick there could pair with some real template further on, and the
            // comments in between would vanish silently.
            i++
            continue
        }

        // JSX child text: only `<` and `{` mean anything, everything else is literal text.
        if (frame !== undefined && frame.kind === 'text') {
            if (c === '\n') {
                line++
                i++
                continue
            }
            if (c === '{' && !plain.has(i)) {
                stack.push({ kind: 'brace', at: i, braces: 0 })
                lastSig = undefined
                i++
                continue
            }
            if (c === '<' && !plain.has(i)) {
                const close = c2 === '/' ? matchAt(JSX_CLOSE, text, i) : null
                if (close && (close[1] ?? '') === frame.name) {
                    line += countNewlines(text, i, i + close[0].length)
                    stack.pop()
                    lastSig = 'x'
                    i += close[0].length
                    continue
                }
                const open = c2 === '/' ? null : matchAt(JSX_OPEN, text, i)
                if (open) {
                    stack.push({ kind: 'tag', at: i, name: open[1] ?? '' })
                    i += open[0].length
                    continue
                }
            }
            i++
            continue
        }

        if (c === '/' && c2 === '/') {
            i = lineComment(i)
            continue
        }
        if (c === '/' && c2 === '*') {
            depth = 1
            i += 2
            continue
        }
        if (c === '\n') {
            line++
            i++
            continue
        }

        // Braces inside an interpolation or a JSX container: counting them is what tells which
        // `}` closes this layer. Braces in strings, regexes and comments never reach here — they
        // were skipped whole above. By this point the top of the stack can only be one of these
        // two counting layers; the others were routed away above.
        if (frame !== undefined && (c === '{' || c === '}')) {
            if (c === '{') frame.braces++
            else if (frame.braces > 0) frame.braces--
            else if (frame.kind === 'brace') stack.pop()
            else frame.braces = -1
            lastSig = c
            i++
            continue
        }

        // Rust raw strings r"…" / r#"…"# / br##"…"##: nothing inside is a comment, and there are
        // **no escapes** inside — `r"C:\"` ends at that quote. Handled as an ordinary string,
        // `\"` counts as an escape, the string runs to the next quote in the file, and every
        // comment in between disappears.
        if (lang === 'rust' && (c === 'r' || c === 'b') && !isWordChar(codePointEndingAt(text, i - 1))) {
            const raw = /^b?r(#*)"/.exec(text.slice(i, i + 12))
            if (raw) {
                const close = `"${raw[1]}`
                const end = text.indexOf(close, i + raw[0].length)
                if (end >= 0) {
                    line += countNewlines(text, i, end + close.length)
                    i = end + close.length
                    lastSig = 'x'
                    continue
                }
                // Not closed: it is not a raw string, so do not swallow the rest of the file.
            }
        }
        if (lang === 'rust' && c === "'") {
            // `'a` is a lifetime, `'x'` is a character literal. Only the latter is skipped
            // whole; taking a lifetime for the start of a string swallows half the file behind
            // it.
            const ch = /^'(?:\\.|[^\\'])'/.exec(text.slice(i, i + 12))
            i += ch ? ch[0].length : 1
            lastSig = 'x'
            continue
        }
        // A JSX element: this enters a lexical state, not a stretch of text skipped whole — a
        // `{…}` container is JS code again and the comments there are real comments.
        //
        // The type parameter list of a generic arrow occupies the same position; the two are told
        // apart by what the angle brackets hold (see `genericArrowHead`). A parameter list is
        // walked as ordinary characters: what is left inside is code, so a comment written on a
        // parameter, as in `<T extends /* why the constraint */ X>`, is still a real comment.
        if (
            jsx &&
            c === '<' &&
            !plain.has(i) &&
            jsxPosition(text, i, lastSig) &&
            !genericArrowHead(text, i)
        ) {
            const open = matchAt(JSX_OPEN, text, i)
            if (open) {
                stack.push({ kind: 'tag', at: i, name: open[1] ?? '' })
                i += open[0].length
                continue
            }
        }
        // A JS template: pushed onto the stack rather than skipped whole — the comments inside
        // `${…}` are real comments.
        //
        // A backtick at code position has **no second reading**: in JS a backtick is only a
        // template delimiter, opening one or closing one. So this does not ask "can a template
        // start here" — that question needs an answer only while JSX child text is read as code
        // too, and child text is routed away in the `text` state (see the JSX branches of
        // `scanOnce`).
        //
        // Not asking buys a stronger property: pairing goes purely by **position** — the 1st,
        // 3rd, 5th ... backtick in a stretch of code is an opener. No "can this one be an opener"
        // test can do that, because it gives openers and closers two different answers: the same
        // backtick is judged an ordinary character on the opening side, while on the closing side
        // it sits against a word or a `)` and is judged an opener. Once such a test misses the
        // opener of a real template, that template's closing backtick steps up as the next
        // stretch's opener, every backtick after it is off by one role, and the comments in
        // between turn wholesale into template body — silently, and silence is this gate's only
        // failure mode. Positional pairing has no such shift: missing an opener means missing a
        // whole pair.
        if (js && c === '`' && !plain.has(i)) {
            stack.push({ kind: 'template', at: i, braces: -1 })
            i++
            continue
        }
        if (c === '"' || c === "'") {
            // JS `'…'` / `"…"` cannot span lines, so anything not paired within the line is not
            // a string: handle a lone apostrophe as a string and every comment after it in this
            // file disappears with it.
            const stopAtNewline = js
            const end = skipQuoted(text, i, c, stopAtNewline)
            if (end >= 0) {
                line += countNewlines(text, i, end)
                i = end
                lastSig = 'x'
                continue
            }
            // A quote that does not pair: walk it as an ordinary character. Falls through to the
            // two lines below.
        }
        // JS regex literals. Without them, a spelling like `/["']/` opens a bogus string and
        // every comment boundary in the file is wrong from there on — wrong in both directions,
        // misses and false positives alike.
        if (js && c === '/' && regexPosition(text, i, lastSig)) {
            const end = skipRegex(text, i)
            i = end
            lastSig = 'x'
            continue
        }

        if (!/\s/.test(c)) lastSig = c
        i++
    }
    flush()
    // Among the layers still open at end of file, the **innermost** one is demoted. An outer
    // layer opened at a position that was judged (a `<` at code position passes `jsxPosition`,
    // backticks pair by position); the `<` and `{` in child text have no position to judge by and
    // enter on shape alone — the innermost layer is therefore the least grounded one. The two ways
    // of being wrong do not cost the same: demote the outer JSX and its child text falls back to
    // code position, where a lone backtick can pair with some real template further on and the
    // comments in between vanish silently; demote the innermost layer and the worst case is the
    // outer layer staying unclosed, to be demoted on the next pass.
    //
    // Every layer's opening character passes `plain`, so every pass removes at least one opener
    // and it terminates.
    return { perLine, unclosed: stack.length > 0 ? stack[stack.length - 1].at : undefined }
}

// A name's continuation characters, again by the ECMAScript definition: whether what precedes
// `<` is the end of a name decides whether this continues an expression or starts a new one.
// Miss one character and the `<` in `Ä < b` becomes an expression start, turning a comparison
// into an opening tag.
const IDENT_PART = /[\p{ID_Continue}$\u200C\u200D]/u
const isWordChar = (c) => c !== undefined && IDENT_PART.test(c)

// Read the whole code point that **ends** at index `j`, on the same test as `typeStart`: ask
// about the halves of a surrogate pair separately and neither half is part of a name, so a name
// breaks in two here.
const codePointEndingAt = (text, j) => {
    if (j < 0 || j >= text.length) return undefined
    const unit = text.charCodeAt(j)
    if (unit >= 0xdc00 && unit <= 0xdfff && j > 0) {
        const high = text.charCodeAt(j - 1)
        if (high >= 0xd800 && high <= 0xdbff) return text.slice(j - 1, j + 1)
    }
    return text[j]
}

function countNewlines(text, from, to) {
    let count = 0
    for (let i = from; i < to; i++) if (text[i] === '\n') count++
    return count
}

// Returns the index after the closing quote; -1 when it does not pair. **Not pairing does not
// mean "the rest is a string"**: an unclosed quote is usually not the start of a string at all
// (an apostrophe in JSX text), and swallowing the rest of the file is this gate's worst failure
// mode — it exits 0 saying "nothing found".
function skipQuoted(text, start, quote, stopAtNewline) {
    let i = start + 1
    while (i < text.length) {
        const c = text[i]
        if (c === '\\') {
            i += 2
            continue
        }
        if (c === quote) return i + 1
        if (stopAtNewline && c === '\n') return -1
        i++
    }
    return -1
}

// Which characters an expression can start after: an operator, an opening bracket, a comma, or a
// keyword like `return`. After an identifier, a `)` or a `]`, an expression is being continued,
// not started.
// `>` is in the set too: `() => /re/` and `() => <div>` both follow the arrow, while `a > /re/`
// and `a > <div>` are not valid JS, so there is no second reading to tell apart.
const EXPR_PREV = new Set([...'([{,;:=!&|?+-*%~^<>'])
const EXPR_KEYWORDS = new Set([
    'return',
    'typeof',
    'instanceof',
    'in',
    'of',
    'case',
    'do',
    'else',
    'yield',
    'await',
    'new',
    'delete',
    'void',
    'throw',
])

// Only `/` and `<` ask this question: each has a binary-operator reading (division, less-than)
// that only position separates. A backtick does not ask — it has no second reading, and the
// reason is on the template branch of `scanOnce`.
function expressionStart(text, i, lastSig) {
    if (lastSig === undefined) return true
    if (EXPR_PREV.has(lastSig)) return true
    if (!isWordChar(lastSig)) return false
    let j = i - 1
    while (j >= 0 && /\s/.test(text[j])) j--
    const end = j + 1
    for (;;) {
        const ch = codePointEndingAt(text, j)
        if (!isWordChar(ch)) break
        j -= ch.length
    }
    return EXPR_KEYWORDS.has(text.slice(j + 1, end))
}

const regexPosition = expressionStart

// A JSX element is a primary expression, on the same test as a regex literal: it appears only at
// the start of an expression, while the `<` in `a < b` follows an operand.
const jsxPosition = expressionStart

// ── The other reading of `<` in `.tsx`: generic arrow type parameters ─────
// A `<Name …>` at the start of an expression has two readings in `.tsx`, and the two constructs
// hold different things: an opening tag holds attributes (`a`, `a="x"`, `{...p}`), a type
// parameter list holds type parameters (`T`, `T extends X`, `T = D`, separated by `,`, trailing
// comma allowed), and a parameter list is always followed by the `(` of the value parameter list.
// The test is therefore **what the angle brackets hold**, and it settles at the `<` itself.
//
// More than one shape reads both ways: a bare `<T>` is one, `<Foo extends bar = "x">` is
// another — as an attribute list it is the two attributes `extends` and `bar="x"`, as a parameter
// list it is one parameter with a constraint and a default. Wherever both readings work, the
// whole boundary follows TypeScript's own: in `.tsx`, `<T>` is JSX, `<T,>` and `<T = D>` are
// generic arrows, and `<T extends …>` turns on whether a type can start after `extends`. Sharing
// the source is what keeps the two readings from disagreeing on a file that compiles.
//
// Unrecognized means JSX, and that direction is chosen: missing a generic arrow costs at worst an
// unclosed layer and one more demotion rescan; judging a real element to be a type parameter
// list, the other way round, drops its child text back to code position, where a lone backtick
// can pair with some real template further on and the comments in between vanish silently. So
// whenever the parameter list grammar does not work out or does not finish (an unmatched quote,
// unbalanced brackets, end of file), it is handed back to the JSX path.
const TP_NAME = new RegExp(IDENT, 'uy')
// When a name continuation character sits directly against `extends`, it is the start of a name
// rather than the keyword: `extendsÄ` and `extendsA` are each one identifier.
const TP_EXTENDS = new RegExp(String.raw`extends(?!${ID_PART}|\\)`, 'uy')

// The characters a type can start with. A constraint and a default are both **types**, so where
// a type cannot start there is no constraint and no default, and the angle brackets do not hold a
// parameter list.
//
// This check is the test for the `extends` branch, for the reason given in `typeParameterList`:
// `extends` is itself a valid attribute name. After an attribute name, an attribute list accepts
// only `=`, `/`, `>` and the next attribute (an identifier or a `{` spread), and of those the last
// two can also start a type while the first three cannot — the boundary between the two readings
// therefore falls exactly on "can a type start here".
//
// The name branch follows the ECMAScript identifier, not ASCII: the constraint in `<T extends Ä>`
// is a type, no different from `<T extends A>`, and going by ASCII judges the former unable to
// start, so the angle brackets fall back to JSX — and once back, a `</T>` inside some string
// closes them and the comments go silent. `\` is in the set too: an escape spelling (`Ä`) is
// another spelling of the same name.
//
// An incomplete set means "cannot start", the same direction as the whole path: missing a generic
// arrow costs at worst an unclosed layer and one more demotion rescan. `+` is not in the set;
// only the minus sign is part of a literal type.
const TYPE_START = /[\p{ID_Start}$_\d"'`(\[{<|&\\-]/u
// One **whole code point** at a time. A name from a supplementary plane is a surrogate pair;
// split it into two code units and ask about each, and neither half belongs to any identifier
// category, so a type that can start is judged unable to — the angle brackets fall back to JSX,
// some later closing tag shuts them, and the comments in between go silent.
const typeStart = (text, i) =>
    i < text.length && TYPE_START.test(String.fromCodePoint(text.codePointAt(i)))

// What sits between tokens: whitespace (a parameter list spans lines) and comments
// (`<T /* why the constraint */ extends X>` is legal). Skipping them puts the test on the
// parameter list grammar rather than on whether somebody wrote a sentence in the middle — and a
// test that cannot settle falls back to JSX, where only a layer that does not close is caught.
//
// This only **looks ahead**, it does not consume: the main loop still walks character by character
// from after the `<`, so a comment written on a type parameter is still a real comment.
const triviaEnd = (text, i) => {
    for (;;) {
        while (i < text.length && /\s/.test(text[i])) i++
        if (text[i] !== '/') return i
        if (text[i + 1] === '/') {
            const end = text.indexOf('\n', i)
            if (end < 0) return text.length
            i = end + 1
            continue
        }
        if (text[i + 1] === '*') {
            const end = text.indexOf('*/', i + 2)
            if (end < 0) return text.length
            i = end + 2
            continue
        }
        return i
    }
}

export function genericArrowHead(text, at) {
    const end = typeParameterList(text, at)
    return end >= 0 && text[triviaEnd(text, end)] === '('
}

// Returns the index after the `>` closing the type parameter list; -1 when the shape is not
// that.
function typeParameterList(text, at) {
    let i = at + 1
    // The tokens that separate the two readings: `extends`, the `=` default, the `,` separator.
    // With none of the three it is the `<T>` that reads both ways, which follows TypeScript's line
    // to JSX.
    //
    // What is load-bearing is the "an attribute list cannot hold it" half, and that sentence does
    // not mean the same thing for all three. `=` and `,` land after the first name, which is the
    // tag name position: what an attribute list accepts there is an attribute name, a `{` spread,
    // `/` or `>`; `=` needs an attribute name first and `,` may not appear at all, so both rule
    // the attribute reading out on the spot. `extends` does not — it is a valid attribute name,
    // and `<Foo extends>`, `<Foo extends={x}>`, `<Foo extends="y">` and `<Foo extends/>` are all
    // elements. So only this branch looks once more at what follows: a constraint is a type, and
    // where a type cannot start it is not a parameter list (see `typeStart`).
    //
    // For an `=` met past that look, the `extends` branch has already settled the test: the
    // `bar="x"` of `<Foo extends bar = "x">` also holds as attributes, and a shape that holds both
    // ways follows TypeScript's line to the generic arrow. The same goes for what follows a `,`,
    // which has already ruled the attribute reading out.
    let marked = false
    for (;;) {
        i = triviaEnd(text, i)
        let name = matchAt(TP_NAME, text, i)
        if (!name) return -1
        // A `const` after `<` has only the modifier reading: a parameter cannot be named
        // `const`, which is a reserved word. So another name must follow, and without one this is
        // not a parameter list — while an element named `const` is writable, `<const>x</const>`
        // being an element.
        if (name[0] === 'const') {
            const after = triviaEnd(text, i + name[0].length)
            const next = matchAt(TP_NAME, text, after)
            if (!next) return -1
            i = after
            name = next
        }
        i = triviaEnd(text, i + name[0].length)
        const ext = matchAt(TP_EXTENDS, text, i)
        if (ext) {
            marked = true
            const constraint = triviaEnd(text, i + ext[0].length)
            if (!typeStart(text, constraint)) return -1
            i = typeEnd(text, constraint)
            if (i < 0) return -1
        }
        if (text[i] === '=' && text[i + 1] !== '=' && text[i + 1] !== '>') {
            marked = true
            i = typeEnd(text, i + 1)
            if (i < 0) return -1
        }
        if (text[i] === ',') {
            marked = true
            i = triviaEnd(text, i + 1)
            if (text[i] !== '>') continue
            return i + 1
        }
        if (text[i] === '>') return marked ? i + 1 : -1
        return -1
    }
}

// Skip a type and stop on this level's `,` / `=` / `>`. A constraint or a default can hold
// anything, so angle, round, square and curly brackets all count depth (the `>` of
// `Array<string>` is not this list's), strings and templates are skipped whole (the angle bracket
// inside `">"` is a literal), and `=>` is swallowed whole (a function type's arrow is not a
// default).
function typeEnd(text, i) {
    let depth = 0
    while (i < text.length) {
        const c = text[i]
        if (c === '"' || c === "'" || c === '`') {
            // Quotes other than backticks do not span lines inside a type: not pairing means
            // this is not a parameter list.
            const end = skipQuoted(text, i, c, c !== '`')
            if (end < 0) return -1
            i = end
            continue
        }
        if (c === '/' && (text[i + 1] === '/' || text[i + 1] === '*')) {
            // An angle bracket inside a comment is not a bracket: the list in
            // `<T extends object /* > */>` closes on the later `>`.
            i = triviaEnd(text, i)
            continue
        }
        if (c === '=' && text[i + 1] === '>') {
            i += 2
            continue
        }
        if (c === '<' || c === '(' || c === '[' || c === '{') {
            depth++
            i++
            continue
        }
        if (c === ')' || c === ']' || c === '}') {
            if (depth === 0) return -1
            depth--
            i++
            continue
        }
        if (c === '>') {
            if (depth === 0) return i
            depth--
            i++
            continue
        }
        if (depth === 0 && (c === ',' || c === '=')) return i
        i++
    }
    return -1
}

// Returns the index after the regex literal (flags included). Spanning a line means it is not a
// regex, so it is handled as division.
function skipRegex(text, start) {
    let i = start + 1
    let inClass = false
    while (i < text.length) {
        const c = text[i]
        if (c === '\\') {
            i += 2
            continue
        }
        if (c === '\n') return start + 1
        if (inClass) {
            if (c === ']') inClass = false
            i++
            continue
        }
        if (c === '[') {
            inClass = true
            i++
            continue
        }
        if (c === '/') {
            i++
            while (i < text.length && /[a-z]/i.test(text[i])) i++
            return i
        }
        i++
    }
    return start + 1
}

// ── Scanning ──────────────────────────────────────────────────────────────
export function scanFile(path, text, added) {
    const lang = EXTENSIONS.get(extensionOf(path))
    if (!lang) return []
    // CRLF is folded to LF uniformly: the line count is unchanged, but neither the `$` of the
    // exemption marker nor the newline checks in the lexer has to recognize `\r` on its own.
    // Missing it once makes the exemption fail entirely on a CRLF file — both directions break at
    // once.
    const normalized = text.includes('\r') ? text.replace(/\r\n/g, '\n') : text
    const comments = commentSegments(normalized, lang)
    const findings = []

    for (const lineNo of [...added].sort((a, b) => a - b)) {
        const comment = comments.get(lineNo)
        if (comment === undefined) continue
        const allow = ALLOW.exec(comment)
        if (allow) {
            if (!allow[1])
                findings.push({
                    path,
                    line: lineNo,
                    signal: 'allow-without-reason',
                    hit: 'comment-rule-allow',
                    why: 'an exemption must state its reason, or it is no different from a silent bypass',
                    fix: 'after the marker, write why this number or this piece of history has to stay in the comment',
                })
            continue
        }
        for (const signal of SIGNALS) {
            const hit = firstHit(signal, comment)
            if (hit === null) continue
            findings.push({
                path,
                line: lineNo,
                signal: signal.id,
                hit,
                why: signal.why,
                fix: signal.fix,
            })
            break
        }
    }
    return findings
}

function firstHit(signal, comment) {
    for (const re of signal.patterns) {
        re.lastIndex = 0
        const m = re.exec(comment)
        if (m) return m[0].trim()
    }
    return null
}

function main() {
    const argv = process.argv.slice(2)
    const baseArg = argv.includes('--base') ? argv[argv.indexOf('--base') + 1] : undefined
    const base = resolveBase(baseArg)
    const head = (git('rev-parse', 'HEAD') || '').trim()

    const findings = []
    let scanned = 0
    let self = 0
    for (const [path, added] of addedLines(base)) {
        if (SKIP_DIRS.test(path) || !EXTENSIONS.has(extensionOf(path))) continue
        if (isSelf(path)) {
            self++
            continue
        }
        let text
        try {
            text = readFileSync(path, 'utf8')
        } catch (err) {
            // `--diff-filter=ACMR` already excludes deleted files, so an unreadable file has
            // some other cause. Skipping it lets a file that should be checked through silently.
            die(`cannot read ${path}: ${err.message}`)
        }
        scanned++
        findings.push(...scanFile(path, text, added))
    }

    const skipped = self === 0 ? '' : `, plus ${self} files of the rule itself skipped as SELF`
    if (findings.length === 0) {
        console.log(
            base === head
                ? `HEAD is the base (${base.slice(0, 8)}); there are no added lines to look at.`
                : `scanned the added comment lines of ${scanned} files against ${base.slice(0, 8)}${skipped}; nothing found that will expire.`,
        )
        return
    }

    console.error(
        `${findings.length} things that will expire in the added comment lines` +
            ` (base ${base.slice(0, 8)}, ${scanned} files scanned${skipped}):\n`,
    )
    for (const f of findings) {
        console.error(`  ${f.path}:${f.line}  [${f.signal}]  ${JSON.stringify(f.hit)}`)
        console.error(`      why it fails: ${f.why}`)
        console.error(`      fix: ${f.fix}\n`)
    }
    console.error('the rule is in AGENTS.md, "Comments state invariants, not history".')
    console.error('the test for a sentence: on another machine, with this traversal rewritten and this pool reconfigured, is it still true and still useful?')
    console.error('for what genuinely must stay, write `comment-rule-allow: reason` on the same line.')
    process.exit(1)
}

// "Was this run directly" compares by **real path**: `import.meta.url` is the one node resolved
// symlinks on, while `argv[1]` is the one written verbatim on the command line. Without that
// conversion, any symlinked segment on the path makes the two disagree, `main()` never runs, and
// the process exits 0 without printing a word — a gate that silently does nothing is worse than
// no gate.
if (process.argv[1] && import.meta.url === pathToFileURL(realPath(process.argv[1])).href) main()
