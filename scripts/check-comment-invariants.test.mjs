#!/usr/bin/env node
// Regression for `check-comment-invariants.mjs` itself.
//
// ── Why the gate needs a regression of its own ──────────────────────────────
// The worst way this gate fails is not an error, it is **still printing "nothing found" while
// what it claims to be watching has dropped out of its sight**. That failure turns no check red —
// it is what keeps the check green. So every assertion carries a proof: break the thing it
// protects and it must go red.
//
// **A case that stays green once the code it guards is deleted is not a case at all.** The lexing
// ones fall into that most easily: if an `r#"…"#` holds only paired quotes and no backslash, the
// ordinary-string path skips it too, so the "raw string" case tests nothing. Every lexing case
// below picks a shape **only one branch gets through**, and the shape itself is written in the
// comment on that case.
//
// All five shapes are below:
//   1. The signals themselves. Every signal has at least one sentence it must fire on and one of
//      a similar shape carrying no measurement and no history — only the first going red shows it
//      recognizes the thing rather than a keyword.
//   2. The trade. A number with a unit is a candidate whatever verb the sentence uses; a contract
//      number or an external hard cap stays by way of an exemption spelling out its reason on the
//      same line. Both ends are pinned: the shape must fire, and the exemption with a reason must
//      be allowed.
//   3. Comment boundaries. Text inside a string, a char literal, a Rust raw string or a JS regex
//      literal is not a comment; text inside a block comment and a comment inside a `${…}`
//      template interpolation are. One lexing mistake produces false positives and misses at
//      once, neither of them conspicuous. The other way round, an **unmatched** quote (an
//      apostrophe in JSX text, a lone backtick) must not hide the comments in the whole rest of
//      the file.
//   4. Added lines only. Edit a file full of pre-existing violations and only **the added line**
//      may turn it red — otherwise the gate is a wall the day it opens, and a wall ends up
//      switched off.
//   5. The rule's own prose. Those two files are exempt by path and the machine no longer judges
//      them, so "the prose carries no sentence pointing at another implementation" is left to one
//      regression.
//
// The end-to-end cases run in a temporary git repository: base resolution, diff parsing, exit
// codes and the file:line:signal in the report all go through real git rather than pure functions
// alone. The one with a non-ASCII filename is in there too: git quotes such a path, and failing
// to unquote it silently skips a file.
//
// The sample comments fed to the checker stay in Chinese: they are the fixtures for the Chinese
// character classes inside its patterns, which the checker keeps byte-identical. Everything the
// reader is meant to read — case names, comments, the free-text reason in an exemption — is
// English.

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { copyFileSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { spawnSync } from 'node:child_process'

import { commentSegments, genericArrowHead, scanFile, unquotePath } from './check-comment-invariants.mjs'

const HERE = dirname(fileURLToPath(import.meta.url))
const CHECKER = join(HERE, 'check-comment-invariants.mjs')

// The whole file counts as added, for the cases that test the signals and the lexer alone.
const allLines = (text) => new Set(text.split('\n').map((_, i) => i + 1))
const scan = (path, text) => scanFile(path, text, allLines(text))
const signals = (path, text) => scan(path, text).map((f) => f.signal)
const lines = (path, text) => scan(path, text).map((f) => f.line)

// ── 1. Signals: what must go red goes red, a near shape does not ──────────
//
// In each pair, `bad` is the kind of sentence the rule names and `good` is the same point written
// as an invariant. Both sides must hold: testing `bad` alone cannot prove the signal recognizes
// the thing rather than a word that happens to be there.
const PAIRS = [
    {
        signal: 'number-with-unit',
        bad: '/// 这一版把窗口压到 100ms。',
        good: '/// 窗口必须短于租约，否则续期赶不上围栏。',
    },
    {
        signal: 'number-with-unit',
        bad: '/// 开一个目录约 11.4 µs，解析一条链接约 38 µs。',
        good: '/// 解析一条链接比读一个目录项贵一到两个数量级。',
    },
    {
        signal: 'number-with-unit',
        bad: '/// 这条路上绑定要等满 20 秒，补偿撤绑再等 10 秒。',
        good: '/// 这条路上要等满两次调用的生产耐心，所以夹具把耐心压小。',
    },
    {
        signal: 'ratio',
        bad: '/// Under oversubscription the pair went from 60 of 192 runs red to 0 of 192.',
        good: '/// Registration is the barrier, so overlap is decided by causality.',
    },
    {
        signal: 'ratio',
        bad: '/// 这道门大约每八次全量跑红两次。',
        good: '/// 这道门在负载下会红，而红的原因不是它要钉住的性质。',
    },
    {
        signal: 'count',
        bad: '/// CONTROL_SURFACES 上二十一个名字里绝大多数是别人的工具。',
        good: '/// CONTROL_SURFACES 上的名字绝大多数是别人的工具。',
    },
    {
        signal: 'count',
        bad: '/// The suite has 54 tests covering this path.',
        good: '/// Every branch of this path is covered.',
    },
    {
        signal: 'history',
        bad: '/// The first version made a bet instead: poll until the row shows up.',
        good: '/// Polling until the row shows up is a bet on the clone still running.',
    },
    {
        signal: 'history',
        bad: '/// 上一版这里赌的是「第一次的克隆还没跑完」。',
        good: '/// 别改回轮询：那是在赌第一次的克隆还没跑完。',
    },
    {
        signal: 'history',
        bad: '/// 实测把它换成「否定也打戳」，这条回归照样绿。',
        good: '/// 「否定也打戳」会让这条回归照样绿，所以判据不能落在戳上。',
    },
    {
        signal: 'history',
        bad: '/// This used to be called `activation_cas_failure`, which said the wrong thing.',
        good: '/// The name says which fence survives, because that is what is pinned.',
    },
]

for (const [i, pair] of PAIRS.entries()) {
    test(`signal ${pair.signal} recognizes the wording that violates it #${i}`, () => {
        assert.deepEqual(signals('src/x.rs', pair.bad), [pair.signal], pair.bad)
    })
    test(`signal ${pair.signal} lets the same point written as an invariant through #${i}`, () => {
        assert.deepEqual(signals('src/x.rs', pair.good), [], pair.good)
    })
}

// Half the wordings the rule names are ordinary domain language in another context. A gate that
// mistakes them gets switched off, and once it is off the half it could catch is gone too — so
// these shapes must **not** fire.
const NOT_SIGNALS = [
    '// 这条 socket 返回 404s 和 401s 的时候不重试。',
    '// 一条测试钉住一件事；这两个用例分别钉住两端。',
    '// 同一次 start 的重试遇上自己上一轮留下的拒绝项，必须换一枚新 fence。',
    '// state 是当初进入这个页面时写的，之后谁也改不了。',
    '// 旧版 agitd 不发这个字段，所以写成可选。',
    '// A lock used to serialize writes across the pool.',
]

for (const [i, line] of NOT_SIGNALS.entries()) {
    test(`ambiguous wording is not a hit #${i}`, () => {
        assert.deepEqual(scan('src/x.rs', line), [], line)
    })
}

// ── 2. The trade: the shape fires, the reason allows ──────────────────────
//
// The machine cannot tell a number taken off a machine from one that is guaranteed: `a heartbeat
// every 15 seconds` and `one run, stalling every 15 seconds` have exactly the same shape.
// Guessing from the observation verb hands the test to the verb — the writer picks another verb
// and walks through. So the test is the shape and the way out is an exemption.
//
// Every sentence below is the kind of comment in real code that has to keep its number: a
// contract, the hard cap of an external system, a copy of a constant in the code. **They must
// fire** (a shape is a shape), and **a one-line reason must be able to keep them**. Only both
// together are the trade; pinning one end lets the other slip away.
const CONTRACT_NUMBERS = [
    ['/// 所以每次心跳问一次库。代价是每台机器每 15 秒一次主键查询。', 'the heartbeat contract of this system'],
    ['/// 「吊销最多 15 秒后一定生效，不管它连在哪个 Pod 上」。', 'same as above: that bound is the contract'],
    ['/// PostgreSQL 的 btree 索引行超过 ~2704 字节直接报错，而且是**确定性**的。', 'a hard cap from an external system'],
    ['/// TTL 是 3 秒，一个窗口内可以进来很多条。', 'a copy of `ENTRY_TTL`'],
    ['/// 相对超时 12 秒，比一次正常结算宽得多，又远短于一次 TCP 超时。', 'a copy of `RELATIVE_TIMEOUT`'],
    ['/// 真上限是 8 MiB；按比例缩到 1 字节，好在不灌 32 MiB 的前提下走到同一条分支。', 'the fixture scales `MAX_LINE_BYTES` down'],
    ['/// 观众侧每个操作 30 秒超时，而机器明明在线。', 'a copy of `VIEWER_OP_TIMEOUT`'],
]

for (const [i, [line]] of CONTRACT_NUMBERS.entries()) {
    test(`a contract number is still a candidate #${i}`, () => {
        assert.deepEqual(signals('src/x.rs', line), ['number-with-unit'], line)
    })
}

for (const [i, [line, reason]] of CONTRACT_NUMBERS.entries()) {
    test(`an exemption with a reason keeps the contract number #${i}`, () => {
        const allowed = `${line}comment-rule-allow: ${reason}`
        assert.deepEqual(scan('src/x.rs', allowed), [], allowed)
    })
}

// The other way round: these are **not** quantities and not history, and must never fire. The
// first three are shapes this gate catches by mistake: an estimated upper bound is not a count,
// `每一次…这一次` is not a ratio, and `几分钟` holds no number.
const MUST_PASS = [
    // "previous value" here is about this field, not about another implementation.
    [
        'src/features/agents/model.rs',
        '/// used to drain readers admitted before this change. `None` means identity, previous value,',
    ],
    [
        'src/features/agents/model.rs',
        '/// or lease authority changed while the route was waiting; nothing was mutated.',
    ],
    // Explains why an **absent** thing is absent, and constrains how whoever wants it back writes
    // it.
    ['src/features/rc/model.rs', '// 这里曾经有一个 `events_since`（「seq 大于 X 的前 N 条」）——零处调用，而且它的'],
    ['src/features/rc/model.rs', '// 人用的函数不值得留着当上膛的枪；要它的时候照 `events_tail` 的样子写。'],
    // An estimated upper bound is not a count: adding a case does not make it expire.
    ['src/x.rs', '/// `PUSHBACK_MAX_BYTES` 放行，一行能长出约八百万个条目。'],
    ['src/x.rs', '/// 生命周期帧远到不了这个量级；而 4096 个条目的容器只约束得了「小消息很多」。'],
    // `每一次…这一次` is not a ratio.
    ['src/x.rs', '// **每一次武装都推进代号，哪怕这一次没翻动那一位。**'],
    // `几` gives an order of magnitude, not a quantity: it holds no number to check, to move into
    // an assertion, or to expire.
    ['src/x.rs', '/// 连不上远端时会一直等到 TCP 超时，那是**几分钟**。'],
    ['src/x.rs', '/// 这趟扫描在全局锁里同步跑，一份大得离谱的转录会把所有 RPC 顿住几百毫秒。'],
    // Modulo has spaces on both sides; it is not a percentage.
    ['src/x.rs', '// 取模在语料里写成 `8192 % 8` / `step % 100`，两侧带空格。'],
    // Past tense with a domain meaning: the runtime state of a session, git history, the guarantee
    // after a degrade, the old value of a field.
    ['src/x.rs', '/// 恢复一段**曾经**无审批跑过的会话只给 owner。'],
    ['src/x.rs', '/// `agit revert` 于是在一份完全健康的历史上报错退出。'],
    ['src/x.rs', '/// 没有收据只是退回修复前的行为（可能丢一条通知），不打断结算。'],
    ['src/x.rs', '/// `Ok(true)` establishes a previously unknown active turn; `Ok(false)` repeats it.'],
    ['src/x.rs', '/// Decide whether a strict commit covers a new (or previously committed) settlement.'],
    // "we have **not measured** it" is a constraint on future changes, not an observation report.
    ['src/x.rs', '/// Multiple native effects have ordering semantics we have not measured.'],
]

for (const [i, [path, line]] of MUST_PASS.entries()) {
    test(`a sentence that is neither a quantity nor history is not a hit #${i}`, () => {
        assert.deepEqual(scan(path, line), [], line)
    })
}

// These must stay red. They are why this gate exists, and no "noise reduction" may carry them off.
// The first two are the shapes a verb-based test misses: the sentence holds no observation word
// at all, and the number is still a property of that one machine.
const MUST_FIRE = [
    ['number-with-unit', '/// 单次查询耗时约 2200ms。'],
    ['number-with-unit', '/// This path takes 13 seconds.'],
    ['number-with-unit', '/// 134 MB / 20 万行的转录扫一遍 30 ms。'],
    ['number-with-unit', '/// A real Codex app-server fallback took about 13 seconds before Bound.'],
    ['number-with-unit', '/// 压之前是 2200ms，这一版压到了 100ms。'],
    ['number-with-unit', '/// 拒绝率 92.3% 的查询要走完全部候选。'],
    ['history', '/// This used to be called `activation_cas_failure`, which said the wrong thing.'],
    ['history', '/// 实测 sqlx 的慢查询会把整条 socket 读死。'],
    ['history', '/// Measured: the handshake reports `permissionMode: default`.'],
    ['history', '/// 上一版这里写的是 `retain(|_, tx| !tx.is_closed())`。'],
    ['history', '/// 原先只看原串的第一个字节，于是花括号展开整个走空。'],
    ['ratio', '/// Under oversubscription the pair went from 60 of 192 runs red to 0 of 192.'],
    ['ratio', '/// This gate is best-of-5 on a loaded machine.'],
    ['ratio', '/// 这道门大约每八次全量跑红两次。'],
    ['count', '// 一次 main 合并带进 8 个这么写的文件，79 个用例集体红成这样。'],
    ['count', '// Node 25.6 上 11 个文件 111 个用例全红。'],
    ['count', '//! 「有内容的仓库不该是 0」。本机 54 条测试因此全红，而 CI 全绿。'],
]

for (const [i, [signal, line]] of MUST_FIRE.entries()) {
    test(`a sentence that expires must fire #${i}`, () => {
        assert.deepEqual(signals('src/x.rs', line), [signal], line)
    })
}

// ── 3. Comment boundaries ─────────────────────────────────────────────────
test('text that looks like a comment inside a string is not a comment', () => {
    const text = ['fn f() {', '    let s = "// 上一版这里睡了 2200ms";', '}'].join('\n')
    assert.deepEqual(scan('src/x.rs', text), [])
})

// The shape is load-bearing: the **lone** quote in `r#"a" … "b"#` ends the ordinary-string path
// at `a"`, so the `//` after it becomes a real comment. Delete the raw-string branch and this
// case goes red.
test('text inside a Rust raw string is not a comment', () => {
    const text = ['fn f() {', '    let s = r#"a" // 这条路 2200ms "b"#;', '}'].join('\n')
    assert.deepEqual(scan('src/x.rs', text), [])
})

// A raw string has **no escapes**: `r"C:\"` ends at that quote. Treated as an ordinary string,
// `\"` reads as an escape, the string runs on to the `"x"` on the next line, and the comment in
// between disappears — delete that branch and this goes red.
test('a backslash in a raw string is not an escape and does not swallow the comment after it', () => {
    const text = ['let p = r"C:\\";', '/// 这条路 2200ms。', 'let q = "x";'].join('\n')
    assert.deepEqual(lines('src/x.rs', text), [2])
})

// The shape is load-bearing: the signature holds an **odd** number of apostrophes and a real char
// literal follows it. Without the lifetime branch the lone third `'` pairs all the way to `'x'`
// and swallows the comment in between.
test('a lifetime does not swallow the comment after it', () => {
    const text = [
        "fn f<'a>(x: &'a str, y: &'a str) {}",
        '/// 这条路 2200ms。',
        "fn g() { let c = 'x'; }",
    ].join('\n')
    assert.deepEqual(lines('src/x.rs', text), [2])
})

// JSX text is not JS: `Couldn't` holds a single apostrophe. Read as the start of a string, every
// comment after it in the file disappears — exit 0, "nothing found". That is the worst way this
// gate fails.
test('an apostrophe in JSX text does not hide the comments in the rest of the file', () => {
    const text = [
        "export const A = () => <p>Couldn't load that period.</p>",
        '// 这条路 2200ms。',
    ].join('\n')
    assert.deepEqual(lines('website/src/A.tsx', text), [2])
})

test('an unmatched quote is an ordinary character and does not swallow the rest of the file', () => {
    const text = ['const s = `ok`', "const t = 'unterminated", '// 这条路 2200ms。'].join('\n')
    assert.deepEqual(lines('.ci/x.mjs', text), [3])
})

// A template is not one block of string: inside `${…}` it is JS code again and a `/* … */` there
// is a real comment. Skipping the whole template as a string loses this line's comment — and the
// file still exits 0 saying "nothing found".
test('a comment inside a template interpolation is a real comment', () => {
    const text = ['const s = `value: ${foo /* 上一版这里是别的写法 */}`', 'const t = `x`'].join('\n')
    assert.deepEqual(lines('.ci/x.mjs', text), [1])
})

// Every line picks a shape **only one branch gets through**:
//   1. Braces are counted: the `}` in `f({…})` does not close the interpolation. Uncounted, the
//      interpolation closes there, the block comment after it becomes template text, and this
//      line's hit disappears.
//   2. An interpolation can nest another template: the inner template's **literal** holds no
//      comment. Without pushing it on the stack that backtick is an ordinary character, `//`
//      opens a fake comment in code position, and this line is over-reported.
//   3. A string inside an interpolation is still not a comment. Without skipping strings, one
//      more line is over-reported.
//   4. After those three lines the lexer has not run off: this real comment is still here.
test('nested templates, a string inside an interpolation, braces inside an interpolation', () => {
    const text = [
        'const s = `${ f({ a: 1 }) /* 上一版这里是别的写法 */ }`',
        'const t = `a${ `// 上一版这里是别的写法` }b`',
        'const u = `${ "// 上一版这里是别的写法" }`',
        '// 上一版这里是别的写法。',
    ].join('\n')
    assert.deepEqual(lines('.ci/x.mjs', text), [1, 4])
})

// A line continuation inside a template (a backslash right before the newline) still takes up a
// line. Skipping both escaped characters without counting that line files every comment after it
// under a line number that is too early, so both directions break at once: a real comment is
// looked up against a line nobody touched and the gate exits 0 saying "nothing found", while the
// line it names holds no comment at all. Two continuations pin that every one of them is counted;
// an implementation counting only the first fails here as well.
test('a line continuation inside a template does not eat a line number', () => {
    const text = ['const banner = `agit rc \\', 'a \\', 'b`', '// 这条路 2200ms。'].join('\n')
    assert.deepEqual(lines('.ci/x.mjs', text), [4])
    // The load-bearing half: a wrong line number drops this hit when filtering by added lines.
    const onRealLine = scanFile('.ci/x.mjs', text, new Set([4]))
    assert.deepEqual(onRealLine.map((f) => f.line), [4])
    assert.deepEqual(scanFile('.ci/x.mjs', text, new Set([3])), [])
})

// A backtick that never closes opens no template. Without this check it swallows the rest of the
// file as a template — again "exit 0, nothing found".
test('a lone backtick does not swallow the rest of the file', () => {
    const text = ['const t = `unterminated', '// 上一版这里是别的写法。'].join('\n')
    assert.deepEqual(lines('.ci/x.mjs', text), [2])
})

// ── JSX: child text has no lexing ─────────────────────────────────────────
//
// Every case below has all three parts at once: a backtick in JSX text first, a comment in the
// middle, and **a genuine template last**. That last template is load-bearing — without it a
// misaligned template swallows everything to the end of the file and is then caught by "still
// open at the end of the file means degrade and rescan", so an implementation guessing from
// neighbouring characters stays green. With it, the misaligned one finds its pair further down,
// the comment in between turns into template body, and the file exits 0 saying "nothing found".
const jsxSwallow = (jsx) =>
    [jsx, '// 上一版这里是别的写法。', 'export const B = () => `${x}`'].join('\n')

// What sits left of the backtick is an **operator**: in JS, `:`, `+` and `(` are all followed by
// the start of an expression, where a template can open — in JSX text they are punctuation. The
// test is not the backtick's neighbour, it is whether the stretch it landed in is child text.
test('a backtick in JSX text is literal text whatever sits left of it', () => {
    for (const jsx of [
        'export const A = () => <p>Press: ` to continue</p>',
        'export const A = () => <p>Press + ` to continue</p>',
        'export const A = () => <p>Press (` to continue</p>',
        'export const A = () => <p>Press ` to continue</p>',
        "export const A = () => <p>Couldn't press ` here</p>",
        'export const A = () => <>Press: ` in a fragment</>',
    ]) {
        assert.deepEqual(lines('website/src/A.tsx', jsxSwallow(jsx)), [2], jsx)
    }
})

// Both backticks sit in JSX text with the comment between them: this case needs no template after
// it, the two backticks pair with each other. Judged by whether this one closes, they are a legal
// template, the comment in between disappears, and `scanFile` comes back empty-handed.
test('backticks in two stretches of JSX text do not pair into a template across a comment', () => {
    const text = [
        'export const A = () => <p>Press` one</p>',
        '// 上一版这里是别的写法。',
        'export const B = () => <p>Press` two</p>',
    ].join('\n')
    assert.deepEqual(lines('website/src/A.tsx', text), [2])
})

// A closing tag closes only its own level. If `</b>` cleared the whole stack, the child text
// after it would fall back into code position, where the backtick following `Press: ` can open a
// template and pair all the way to the real one at the end. A self-closing `<br />` is the same:
// it closes itself, not the level around it.
test('a closing tag and a self-closing tag close only their own level', () => {
    for (const jsx of [
        'export const A = () => <div><b>x</b>Press: ` here</div>',
        'export const A = () => <div><br />Press: ` here</div>',
    ]) {
        assert.deepEqual(lines('website/src/A.tsx', jsxSwallow(jsx)), [2], jsx)
    }
})

// Inside a `{…}` container it is JS code again, so `{/* … */}` is a real comment — without that
// level it becomes child text and this line's hit disappears. The backtick in the text at the end
// of the same line is the other half: once the container closes the lexer must return to child
// text, and failing to return drops it into code position and swallows the comment after it.
test('a comment in a JSX container is a real comment, and child text resumes once it closes', () => {
    const text = [
        'export const A = () => <p>{/* 上一版这里是别的写法。 */}Press: ` here</p>',
        '// 这条路 2200ms。',
        'export const B = () => `${x}`',
    ].join('\n')
    assert.deepEqual(lines('website/src/A.tsx', text), [1, 2])
})

// Between attributes is a **position where a comment can appear**. Without comments inside a tag
// the element is degraded whole to ordinary characters for a wrong shape, the comment written on
// an attribute turns into the outer element's child text and disappears — and the file exits 0
// all the same. The `<header>` around it is load-bearing: without it the rescan after the degrade
// reads this line again as a comment in code position, and a broken implementation stays green.
test('a comment inside a JSX opening tag is a real comment', () => {
    for (const attr of ['// 上一版这里是别的写法。', '/* 上一版这里是别的写法。 */']) {
        const text = [
            'export const A = () => (',
            '  <header>',
            '    <Row',
            `      ${attr}`,
            '      token={t}',
            '    />',
            '  </header>',
            ')',
        ].join('\n')
        assert.deepEqual(lines('website/src/A.tsx', text), [4], attr)
    }
})

// An attribute value is a literal, not code: a `//` inside a URL opens no comment there. Without
// skipping the value whole, the `//` in `href` becomes a comment and this line grows a hit out of
// nowhere — clean code judged red, and that kind of red ends with the whole gate switched off. An
// attribute value also spans lines (this is JSX, not a JS string), so the line count follows
// while skipping it: count wrong and the hit is filed under a line nobody touched, which the
// added-line filter then drops.
test('a JSX attribute value is not code: the // inside opens no comment, line numbers still count', () => {
    const text = [
        'export const A = () => (',
        '  <a',
        '    href="https://example.com/?t=2200ms"',
        '    title="one',
        'two"',
        '  >',
        '    Press: ` here',
        '  </a>',
        ')',
        '// 上一版这里是别的写法。',
        'export const B = () => `${x}`',
    ].join('\n')
    assert.deepEqual(lines('website/src/A.tsx', text), [10])
    // The load-bearing half: a wrong line number drops this hit when filtering by added lines.
    assert.deepEqual(scanFile('website/src/A.tsx', text, new Set([10])).map((f) => f.line), [10])
    assert.deepEqual(scanFile('website/src/A.tsx', text, new Set([9])), [])
})

// When it cannot tell, the backstop is not one more shape test but **whether it can be taken
// back**: reading it as JSX swallows everything to the end of the file, and a level still open at
// the end of the file is not the thing it claimed to be, so it degrades to ordinary characters
// and rescans. Without that, every comment after it disappears.
//
// The two cases each pick a way of looking like an opening tag without being one: a `<T>` that
// reads both ways in a `.tsx` and goes to JSX along the TypeScript line, and a closing tag whose
// name does not match, which closes no level.
test('something that looks like an opening tag without being one is caught by the degrade and rescan', () => {
    for (const code of [
        'export const id = <T>(x: T) => x',
        'export const A = () => <a>x</b>',
    ]) {
        const text = [code, '// 上一版这里是别的写法。'].join('\n')
        assert.deepEqual(lines('website/src/A.tsx', text), [2], code)
    }
})

// ── Another reading of `<` in `.tsx`: the generic arrow ───────────────────
//
// A `<Name …>` at the start of an expression reads two ways in a `.tsx`, and the test is **what
// the angle brackets hold**: an opening tag holds attributes, a type parameter list holds type
// parameters, and what follows a parameter list is the `(` of the value parameter list.
//
// Every case below has all three parts at once: a generic arrow, a comment inside the value
// parameter list, and **a closer that shuts the misreading down** — the closing tag inside the
// string. That last part is load-bearing: a misreading that cannot be taken back is caught by the
// degrade and rescan, so an implementation resting on the degrade alone stays green without it.
// With it, the misreading closes where it stands, the comment in between vanishes silently, and
// `scanFile` comes back empty-handed.
//
// The closing name follows **the one the misreading would take**, the first identifier after `<`:
// `<const T,>` misreads as an element named `const`, which `</T>` cannot close, so that level
// cannot be taken back and this case falls back to testing the degrade alone.
const genericArrow = (head) => {
    const name = /<[ \t\r\n]*([A-Za-z_$][\w$]*)/.exec(head)?.[1] ?? 'T'
    return [head, '  // 上一版这里是别的写法。', '  x: T', `) => "</${name}>"`].join('\n')
}

// A constraint or a default can hold anything, so angle brackets are counted by depth
// (`Array<string>`), `=>` is not a default (`() => void`), and a string is skipped whole (the
// angle bracket in `">"` is a literal).
//
// For the comma cases and the one with a newline right after `<`, the misreading cannot happen at
// all: `JSX_OPEN` requires whitespace, `/`, `>` or `{` after the tag name, and neither `,` nor an
// empty name can follow. What they pin is **do not loosen that** — loosened, these shapes turn
// into misreadings that close.
test('a generic arrow is not an opening tag: the comment in the value parameter list stays one', () => {
    for (const head of [
        'const f = <T extends object>(',
        'const f = <T = string>(',
        'const f = <T, U>(',
        'const f = <T,>(',
        'const f = <const T,>(',
        'const f = <T extends Array<string>>(',
        'const f = <T extends Map<string, number>>(',
        'const f = <T extends () => void>(',
        'const f = <T extends ">">(',
        'const f = <T extends `a${string}`>(',
        'const f = <T extends { a: string; b: number }, U = [T, T]>(',
        // A parameter list spans lines, and the value parameter list can start on a new line.
        'const f = <\n  T extends object,\n>(',
        'const f = <T extends object>\n  (',
        // A comment can sit between tokens and hold angle brackets: the test is the syntax of the
        // parameter list, not whether somebody wrote a sentence in the middle.
        'const f = <T /* 为什么这么约束 */ extends object>(',
        'const f = <T extends object /* > */>(',
        // Only with every way a constraint can start recognized does the `extends` gate block the
        // attribute list alone. Miss one way a type starts and the element here swallows the
        // value parameter list, comment included, as child text.
        'const f = <T extends -1>(',
        'const f = <T extends 1 | 2>(',
        'const f = <T extends | 1 | 2>(',
        'const f = <T extends & { a: string }>(',
        'const f = <T extends <U>(u: U) => U>(',
        'const f = <T extends [1, 2]>(',
        // The kind that reads both ways: as an attribute list it is the two attributes `extends`
        // and `bar="x"`, as a parameter list it is one parameter with a constraint and a default.
        // TypeScript takes the parameter list here.
        'const f = <Foo extends bar = "x">(',
        // Names are recognized as ECMAScript identifiers, not as ASCII: a parameter name and a
        // constraint may both be non-ASCII, and an escape is another spelling of the same name.
        // Recognized as ASCII, the constraint here is judged unable to start and the angle
        // brackets fall back to JSX — where the closing tag in the string at the end closes them
        // exactly.
        'const f = <T extends Ä>(',
        'const f = <T extends 変数>(',
        'const f = <Ä extends object>(',
        'const f = <Ä,>(',
        'const f = <T extends \\u00C4>(',
        'const f = <\\u0041,>(',
        // A name in a supplementary plane is a pair of surrogates. Asked per code unit, neither
        // half belongs to any identifier category, so this is judged unable to start — the angle
        // brackets fall back to JSX and the closing tag at the end closes them exactly.
        'const f = <T extends \u{10400}>(',
        'const f = <T extends \u{1D49C}>(',
        'const f = <\u{1D49C},>(',
    ]) {
        const text = genericArrow(head)
        const commentLine = head.split('\n').length + 1
        assert.deepEqual(lines('website/src/A.tsx', text), [commentLine], head)
    }
})

// The other half, and the more load-bearing one: missing a generic arrow costs at worst one more
// pass of the degrade and rescan, while judging a real element to be a type parameter list drops
// its child text back into code position, where a lone backtick pairs with the real template at
// the end and the comment in between vanishes silently. So these cases all pick real elements
// that look most like a parameter list: child text starting with `(`, a space between the
// attribute name and `=`, and the `<T>` that reads both ways.
//
// The `extends` group is the other half of the same thing: `extends` is a writable attribute
// name, so the word alone does not separate the two readings. What follows `extends` in each case
// below is something an attribute list accepts and a type cannot start with — `>`, `=`, `/` —
// and they are all elements, which `tsc --jsx preserve` compiles on a `.tsx` all the same.
test('a real element that looks like a type parameter list is still an element', () => {
    for (const jsx of [
        'export const A = () => <p>(optional) ` here</p>',
        'export const A = () => <Row bar = "x">(a) ` b</Row>',
        'export const A = () => <T>(x) ` y</T>',
        'export const A = () => <div {...p}>(a) ` b</div>',
        'export const A = () => <Foo.Bar>(x) ` y</Foo.Bar>',
        'export const A = () => <Row extends>(a) ` b</Row>',
        'export const A = () => <Row extends >(a) ` b</Row>',
        'export const A = () => <Row extends={x}>(a) ` b</Row>',
        'export const A = () => <Row extends = {x}>(a) ` b</Row>',
        'export const A = () => <Row extends="y">(a) ` b</Row>',
        'export const A = () => <Row extends /* 为什么这么传 */>(a) ` b</Row>',
        // A tag name is recognized as an ECMAScript identifier too: a non-ASCII name is a legal
        // element name, and unrecognized it is no level at all, so the child text falls back into
        // code position.
        'export const A = () => <Ä extends>(a) ` b</Ä>',
        'export const A = () => <変数>(a) ` b</変数>',
    ]) {
        assert.deepEqual(lines('website/src/A.tsx', jsxSwallow(jsx)), [2], jsx)
    }
})

// The other face of the same misreading: judged a parameter list, the child text falls back into
// code position, where the `//` becomes a line comment — a miss traded for a false positive, and
// this face does not wait for a backtick further down to pair with, it goes red on the spot. So
// these cases expect nothing: JSX text holds no comment to report.
//
// An empty expectation is easy to write as a case that tests nothing; these are not that. Take
// the `extends` gate away and they report line 1.
test('in an element that looks like a type parameter list, the `//` in child text is not a comment', () => {
    for (const jsx of [
        'export const el = <Foo extends={x}>(参见 // 上一版的写法)</Foo>',
        'export const el = <Foo extends>(参见 // 上一版的写法)</Foo>',
        'export const el = <Foo extends="y">(参见 // 上一版的写法)</Foo>',
        'export const el = <Ä extends={x}>(参见 // 上一版的写法)</Ä>',
        'export const el = <変数>(参见 // 上一版的写法)</変数>',
    ]) {
        assert.deepEqual(lines('website/src/A.tsx', jsx), [], jsx)
    }
})

// The dividing line follows the one TypeScript itself draws, so this table is the answer
// `tsc --jsx preserve` gives on a `.tsx`. More than one shape reads both ways: a bare `<T>` is
// one, `<Foo extends bar = "x">` is another — as an attribute list it is the two attributes
// `extends` and `bar="x"`, as a parameter list it is one parameter with a constraint and a
// default. Sharing the source buys that the two readings never disagree on a file that compiles.
test('shapes that read both ways are split along the TypeScript line', () => {
    for (const [want, code] of [
        [true, '<T extends object>(x: T) => x'],
        [true, '<T = string>(x: T) => x'],
        [true, '<T,>(x: T) => x'],
        [true, '<const T,>(x: T) => x'],
        [false, '<T>(x: T) => x'],
        [false, '<const T>(x: T) => x'],
        [false, '<div>'],
        [false, '<Foo bar>(a)'],
        [false, '<Foo bar = "x">(a)'],
        [false, '<Foo.Bar>(a)'],
        [false, '<>'],
        // A parameter list is always followed by a value parameter list: when it is not, it goes
        // back to the JSX path and the degrade and rescan catches it.
        [false, '<T extends object> x'],
        // `extends` is a legal attribute name, so the word alone does not separate them; the next
        // token does. After an attribute name an attribute list accepts `=`, `/`, `>` and the
        // next attribute, and only the next attribute can also start a type.
        [false, '<Foo extends>(a)'],
        [false, '<Foo extends >(a)'],
        [false, '<Foo extends={x}>(a)'],
        [false, '<Foo extends = {x}>(a)'],
        [false, '<Foo extends="y">(a)'],
        [false, '<Foo extends/>(a)'],
        [false, '<Foo extends /* 为什么这么传 */>(a)'],
        [true, '<Foo extends bar>(a) => a'],
        [true, '<Foo extends bar = "x">(a) => a'],
        [true, '<Foo extends-1>(a) => a'],
        // `=` and `,` are different: they land right after the tag name, a position where an
        // attribute list accepts an attribute name, a `{` spread, `/` or `>` — `=` needs an
        // attribute name before it and `,` may not appear at all. So these two tokens rule out
        // the attribute reading by themselves, whatever follows.
        [true, '<Foo = string>(a) => a'],
        [true, '<Foo ,>(a) => a'],
        // A parameter cannot be named `const`, which is a reserved word, so a `const` after `<`
        // reads only as the modifier and another name must follow it. An element named `const` is
        // writable.
        [false, '<const,>(a)'],
        [false, '<const, U>(a)'],
        [false, '<const = D>(a)'],
        [false, '<const>(a)'],
        // Names are recognized as ECMAScript identifiers, not as ASCII — in both positions: the
        // name in the constraint, and the parameter name itself. An escape is another spelling of
        // the same name.
        [true, '<T extends Ä>(x: T) => x'],
        [true, '<T extends 変数>(x: T) => x'],
        [true, '<Ä extends object>(x) => x'],
        [true, '<Ä,>(x) => x'],
        [true, '<T extends \\u00C4>(x: T) => x'],
        [true, '<\\u0041,>(x) => x'],
        // The same test on a supplementary plane: a whole code point is recognized at once, not
        // split per code unit.
        [true, '<T extends \u{10400}>(x: T) => x'],
        [true, '<T extends \u{1D49C}>(x: T) => x'],
        [true, '<\u{1D49C},>(x) => x'],
        [false, '<\u{1D49C}>(x) => x'],
        [false, '<Ä>(x) => x'],
        [false, '<Ä extends>(a)'],
        [false, '<Ä extends={x}>(a)'],
        // An identifier-continue character right after `extends` makes it the start of a name
        // rather than the keyword.
        [false, '<T extendsÄ>(x)'],
        [false, '<T extends$>(x)'],
    ]) {
        assert.equal(genericArrowHead(code, 0), want, code)
    }
})

// An opening tag with an empty name has one spelling only, `<>`. If `< ` counted as a fragment
// too, `a < b` in child text would open a fake fragment level, the `>` after it would turn that
// into child text and `</>` would close it exactly — so the real `<>` loses a closer and swallows
// everything to the end of the file. The degrade can only take `<>` down, its child text falls
// back into code position, the backtick there pairs with the real template at the end, and the
// comment in between disappears.
//
// The load-bearing half is that **the fake fragment can be taken back**: the ones that cannot are
// caught by the degrade and rescan, so this case wants a `>` and a `</>` after the `< `.
test('whitespace after `<` is not a fragment', () => {
    for (const jsx of [
        'export const A = () => <>Press ` when a < b > c</>',
        'export const A = () => <p>a < b and c ` d</p>',
    ]) {
        assert.deepEqual(lines('website/src/A.tsx', jsxSwallow(jsx)), [2], jsx)
    }
})

// The degrade takes down the **innermost** level. An outer level opened at a position that was
// judged, while a `<` or `{` in child text enters on shape alone — the innermost level rests on
// the least. The two directions of getting it wrong do not cost the same: degrading the outer JSX
// drops its child text back into code position, where a lone backtick pairs with the real
// template at the end and the comment in between vanishes silently. In every case below it is
// the **inner** level that cannot be taken back: a fake tag with a space after the name, and a
// container that never closes.
test('the degrade starts from the innermost level', () => {
    for (const jsx of [
        'export const A = () => <p>x <b and ` c</p>',
        'export const A = () => <p>{ ` }</p>',
        'export const A = () => <p>{<b and ` c</p>',
    ]) {
        assert.deepEqual(lines('website/src/A.tsx', jsxSwallow(jsx)), [2], jsx)
    }
})

// The other half: a template must still be recognized, or the rule "a backtick never opens a
// template" would turn the case above green as well, and the `//` inside a template body would
// become a comment — a miss traded for a false positive, and the gate is untrustworthy either
// way. Three shapes, each with a body **that goes unreported only if the template is
// recognized**: right after a tag, after `=>`, and inside a JSX attribute.
test('a template body is not code: tagged template, after an arrow, inside a JSX attribute', () => {
    for (const line of [
        'const q = gql`{ a // 这条路 2200ms }`',
        'const f = () => `x // 这条路 2200ms`',
        'const A = <div t={`x // 这条路 2200ms`} />',
    ]) {
        const text = [line, '// 上一版这里是别的写法。'].join('\n')
        assert.deepEqual(lines('website/src/A.tsx', text), [2], line)
    }
})

test('a regex literal containing quotes does not swallow the comment after it', () => {
    const text = ['const RE = /["\']/', '// 上一版这里是别的写法。'].join('\n')
    assert.deepEqual(lines('.ci/x.mjs', text), [2])
})

test('a block comment spans lines and each line is located on its own', () => {
    const text = ['/*', ' * 干净的一行。', ' * 上一版这里是别的写法。', ' */'].join('\n')
    assert.deepEqual(lines('src/x.rs', text), [3])
})

test('commentSegments hands back comment text only', () => {
    const text = 'let s = "code"; // a comment'
    assert.deepEqual([...commentSegments(text, 'rust').values()], ['// a comment'])
})

// ── 4. The exemption marker ───────────────────────────────────────────────
test('an exemption with a reason is allowed', () => {
    const line = '/// 这条路 2200ms。comment-rule-allow: this number is an external tool hard cap'
    assert.deepEqual(scan('src/x.rs', line), [])
})

test('an exemption without a reason is itself a failure', () => {
    const line = '/// 这条路 2200ms。comment-rule-allow:'
    assert.deepEqual(signals('src/x.rs', line), ['allow-without-reason'])
})

// Matched against the raw line, the ` */` at the end of a block comment becomes the "reason", so
// the line reports neither the violation nor the missing reason — a silent bypass worse than the
// exemption it means to catch.
test('an exemption without a reason inside a block comment is a failure too', () => {
    for (const line of [
        '/* 这条路 2200ms。comment-rule-allow: */',
        '/** 这条路 2200ms comment-rule-allow: */',
    ]) {
        assert.deepEqual(signals('src/x.rs', line), ['allow-without-reason'], line)
    }
})

test('an exemption with a reason inside a block comment is allowed too', () => {
    const line = '/* 这条路 2200ms。comment-rule-allow: an external tool hard cap */'
    assert.deepEqual(scan('src/x.rs', line), [])
})

// On CRLF both directions break at once: an exemption stops exempting (clean code is judged red
// with no way to silence it), and an exemption without a reason is no longer named.
test('exemptions keep working on a CRLF file', () => {
    const withReason = '/// 这条路 2200ms。comment-rule-allow: an external tool hard cap\r\npub fn f() {}\r\n'
    assert.deepEqual(scan('src/crlf.rs', withReason), [])
    const without = '/// 这条路 2200ms。comment-rule-allow:\r\npub fn f() {}\r\n'
    assert.deepEqual(signals('src/crlf.rs', without), ['allow-without-reason'])
})

test('CRLF does not affect the line number of an ordinary hit', () => {
    const text = 'pub fn f() {}\r\n/// 这条路 2200ms。\r\n'
    assert.deepEqual(lines('src/crlf.rs', text), [2])
})

// ── 5. The rule's own prose ───────────────────────────────────────────────
//
// These two files are exempt by path and the machine no longer judges them — so "the prose keeps
// the rule itself" is left to this regression alone. What the exemption buys is **the right to
// give examples**: a specification that says what counts as a ratio and what counts as a count
// has to be able to write those shapes down. What it does not buy is telling, in its own voice,
// what another implementation did: such a sentence is stopped by this gate everywhere else and is
// no more allowed here; it belongs in the commit message.
test('the prose of the rule points at no other implementation', () => {
    for (const path of [CHECKER, fileURLToPath(import.meta.url)]) {
        const text = readFileSync(path, 'utf8')
        const found = scanFile(path, text, allLines(text)).filter((f) => f.signal === 'history')
        assert.deepEqual(
            found.map((f) => `${f.line}: ${f.hit}`),
            [],
            path,
        )
    }
})

// ── 6. Quoted paths from git ──────────────────────────────────────────────
test('unquotePath decodes the C-style quoting of git', () => {
    assert.equal(unquotePath('"b/src/\\346\\265\\213\\350\\257\\225.rs"'), 'b/src/测试.rs')
    assert.equal(unquotePath('"b/a\\"b.rs"'), 'b/a"b.rs')
    assert.equal(unquotePath('"b/a\\\\b.rs"'), 'b/a\\b.rs')
    assert.equal(unquotePath('b/src/plain.rs'), 'b/src/plain.rs')
})

// ── 7. End-to-end: only comment lines added relative to the merge base ────
const gitInit = (dir) => {
    const run = (...args) => {
        const r = spawnSync('git', args, { cwd: dir, encoding: 'utf8' })
        assert.equal(r.status, 0, `git ${args.join(' ')}\n${r.stderr}`)
    }
    run('init', '--initial-branch=main', '--quiet')
    run('config', 'user.email', 'gate@example.invalid')
    run('config', 'user.name', 'gate')
    run('config', 'commit.gpgsign', 'false')
    // true is the default; setting it explicitly keeps this case independent of the git config on
    // the machine running the tests.
    run('config', 'core.quotePath', 'true')
    return run
}

// The checker runs in a **temporary repository**, so the outer MR environment has to be cleared
// first: `CI_MERGE_REQUEST_DIFF_BASE_SHA` names the base of the MR carrying this CI run, that
// commit is not in the temporary repository, and the checker then exits 2 — while these cases
// assert 0 and 1. A gate whose own regression is green locally and red in CI, for a reason
// unrelated to the code under review, is no different from a broken gate. A case that tests this
// variable itself sets it in `env`.
const runChecker = (dir, { checker = CHECKER, args = [], env = {} } = {}) => {
    const outer = { ...process.env }
    delete outer.CI_MERGE_REQUEST_DIFF_BASE_SHA
    return spawnSync('node', [checker, ...args], {
        cwd: dir,
        encoding: 'utf8',
        env: { ...outer, ...env },
    })
}

// A pre-existing file full of violations, then one line changed. The gate must report that line
// alone — this is what keeps it from being a wall the day it opens.
test('pre-existing violations do not count, the added line does', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    mkdirSync(join(dir, 'src'))

    const legacy = [
        '/// 这条路 2200ms，每八次全量跑红两次。',
        '/// 二十一个名字里绝大多数是别人的工具。',
        'pub fn f() {}',
    ].join('\n')
    writeFileSync(join(dir, 'src/legacy.rs'), `${legacy}\n`)
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')
    run('checkout', '-b', 'work', '--quiet')

    const clean = runChecker(dir)
    assert.equal(clean.status, 0, `${clean.stdout}${clean.stderr}`)

    writeFileSync(join(dir, 'src/legacy.rs'), `${legacy}\n/// 第一版在这里赌了一把。\n`)
    run('add', '-A')
    run('commit', '-m', 'work', '--quiet')

    const dirty = runChecker(dir)
    assert.equal(dirty.status, 1)
    assert.match(dirty.stderr, /src\/legacy\.rs:4 {2}\[history\]/)
    // Not one of the pre-existing lines may appear, or the gate is a wall the day it opens.
    assert.doesNotMatch(dirty.stderr, /src\/legacy\.rs:[123]\b/)
})

test('an untracked new file counts as added in full', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    mkdirSync(join(dir, 'src'))
    writeFileSync(join(dir, 'src/a.rs'), 'pub fn f() {}\n')
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')

    writeFileSync(join(dir, 'src/new.rs'), '/// 上一版这里是别的写法。\npub fn g() {}\n')
    const r = runChecker(dir)
    assert.equal(r.status, 1)
    assert.match(r.stderr, /src\/new\.rs:1 {2}\[history\]/)
})

// git quotes a non-ASCII path by default. An unrecognized extension skips the file, which passes
// it silently — and doing nothing without a sound is what this gate exists to prevent.
test('a non-ASCII path is not skipped silently', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    mkdirSync(join(dir, 'src'))
    writeFileSync(join(dir, 'src/a.rs'), 'pub fn f() {}\n')
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')
    run('checkout', '-b', 'work', '--quiet')

    writeFileSync(join(dir, 'src/测试.rs'), '/// 上一版这里是别的写法。\npub fn g() {}\n')
    run('add', '-A')
    run('commit', '-m', 'work', '--quiet')

    const r = runChecker(dir)
    assert.equal(r.status, 1, `${r.stdout}${r.stderr}`)
    assert.match(r.stderr, /测试\.rs:1 {2}\[history\]/)
})

// The exemption is derived from `import.meta.url` and covers **the checker that is running** and
// the test file beside it. A static table of canonical paths cannot do that: this file is
// verbatim identical in two repositories, so the table would have to list the paths on both
// sides, and every copy would then issue exemption tickets for **the other repository** — create
// a file with the same name over there and it is skipped whole. The checker running here is not
// in this temporary repository, so no canonical path from either side may benefit, and the line
// about skipping as SELF must not appear either.
const CANONICAL = [
    '.ci/check-comment-invariants.mjs',
    '.ci/check-comment-invariants.test.mjs',
    'scripts/check-comment-invariants.mjs',
    'scripts/check-comment-invariants.test.mjs',
]

test('a canonical path from another repository does not count as SELF', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    mkdirSync(join(dir, 'src'))
    writeFileSync(join(dir, 'src/a.rs'), 'pub fn f() {}\n')
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')
    run('checkout', '-b', 'work', '--quiet')

    const violation = '// 上一版这里是别的写法。\n'
    mkdirSync(join(dir, '.ci'))
    mkdirSync(join(dir, 'scripts'))
    const written = [...CANONICAL, 'src/check-comment-invariants.mjs', '.ci/other.mjs']
    for (const path of written) writeFileSync(join(dir, path), violation)
    run('add', '-A')
    run('commit', '-m', 'work', '--quiet')

    const r = runChecker(dir)
    assert.equal(r.status, 1, `${r.stdout}${r.stderr}`)
    for (const path of written) {
        assert.match(r.stderr, new RegExp(`${path.replace(/\./g, '\\.')}:1 {2}\\[history\\]`), path)
    }
    assert.doesNotMatch(r.stderr, /files of the rule itself/)
})

// The other half: the copy that is running, together with the test file beside it, must really
// be skipped, and the skip goes by **where it sits**, not by a blessed directory name. Here the
// checker is copied into `gate/` (a directory neither repository has) while the other
// repository's canonical paths stay in place — they must be reported. What was copied is the real
// checker, whose own prose carries the shapes this rule forbids, so the skip is load-bearing:
// without it, it is reported.
test('the running pair is skipped by where it sits; same-named files elsewhere are not', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    mkdirSync(join(dir, 'src'))
    writeFileSync(join(dir, 'src/a.rs'), 'pub fn f() {}\n')
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')
    run('checkout', '-b', 'work', '--quiet')

    const violation = '// 上一版这里是别的写法。\n'
    mkdirSync(join(dir, 'gate'))
    mkdirSync(join(dir, '.ci'))
    const copy = join(dir, 'gate/check-comment-invariants.mjs')
    copyFileSync(CHECKER, copy)
    writeFileSync(join(dir, 'gate/check-comment-invariants.test.mjs'), violation)
    writeFileSync(join(dir, '.ci/check-comment-invariants.mjs'), violation)
    writeFileSync(join(dir, 'src/check-comment-invariants.mjs'), violation)
    run('add', '-A')
    run('commit', '-m', 'work', '--quiet')

    const r = runChecker(dir, { checker: copy })
    assert.equal(r.status, 1, `${r.stdout}${r.stderr}`)
    assert.doesNotMatch(r.stderr, /gate\/check-comment-invariants/)
    assert.match(r.stderr, /\.ci\/check-comment-invariants\.mjs:1 {2}\[history\]/)
    assert.match(r.stderr, /src\/check-comment-invariants\.mjs:1 {2}\[history\]/)
    assert.match(r.stderr, /2 files of the rule itself/)
})

// A base that cannot be found must **raise an error**, not exit 0 silently as if there were no
// added lines.
test('an unresolvable base exits with an error rather than passing silently', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    gitInit(dir)
    writeFileSync(join(dir, 'a.rs'), '/// 上一版这里是别的写法。\n')
    const r = runChecker(dir, { env: { CI_DEFAULT_BRANCH: '' } })
    assert.equal(r.status, 2)
    assert.match(r.stderr, /merge base/)
})

test('an unresolvable CI_MERGE_REQUEST_DIFF_BASE_SHA errors instead of falling back to a guess', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    writeFileSync(join(dir, 'a.rs'), 'pub fn f() {}\n')
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')
    const r = runChecker(dir, { env: { CI_MERGE_REQUEST_DIFF_BASE_SHA: 'f'.repeat(40) } })
    assert.equal(r.status, 2)
    assert.match(r.stderr, /GIT_DEPTH/)
})

// The case above only proves that an **explicit** unresolvable base errors. In CI that variable
// is not set by a case, the MR job environment carries it already: the temporary repository holds
// no base for the outer MR, so the end-to-end cases all exit 2 while they assert 0 and 1. So the
// variable goes into the **environment** the way an MR job has it, and the same end-to-end case
// runs again: 0 when clean, 1 with one violating line added.
test('the outer base in an MR environment must not leak into the temporary repository', (t) => {
    const dir = mkdtempSync(join(tmpdir(), 'comment-rule-'))
    t.after(() => rmSync(dir, { recursive: true, force: true }))
    const run = gitInit(dir)
    mkdirSync(join(dir, 'src'))
    writeFileSync(join(dir, 'src/a.rs'), 'pub fn f() {}\n')
    run('add', '-A')
    run('commit', '-m', 'base', '--quiet')
    run('checkout', '-b', 'work', '--quiet')

    const outer = process.env.CI_MERGE_REQUEST_DIFF_BASE_SHA
    t.after(() => {
        if (outer === undefined) delete process.env.CI_MERGE_REQUEST_DIFF_BASE_SHA
        else process.env.CI_MERGE_REQUEST_DIFF_BASE_SHA = outer
    })
    process.env.CI_MERGE_REQUEST_DIFF_BASE_SHA = 'f'.repeat(40)

    const clean = runChecker(dir)
    assert.equal(clean.status, 0, `${clean.stdout}${clean.stderr}`)

    writeFileSync(join(dir, 'src/a.rs'), 'pub fn f() {}\n/// 上一版这里是别的写法。\n')
    run('add', '-A')
    run('commit', '-m', 'work', '--quiet')

    const dirty = runChecker(dir)
    assert.equal(dirty.status, 1, `${dirty.stdout}${dirty.stderr}`)
    assert.match(dirty.stderr, /src\/a\.rs:2 {2}\[history\]/)
})
