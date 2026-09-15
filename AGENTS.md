

<!-- agit:begin -->
## Session version control (agit)

Use these rules when the user requests an AgentGit operation or `AGIT_SESSION` or
`AGIT_MERGE_TX` identifies the current managed session. Otherwise continue the
user's task without agit checks, transcript discovery, or session adoption.
The working directory and this file alone do not activate AgentGit.

- Inspect `agit status --json` when the requested operation needs session or workspace state. An adopted session still needs an explicit `<owner/repo>@<branch>` or `AGIT_SESSION`; directory bindings and native runtime IDs do not select it.
- Settle completed phases with `agit commit <owner/repo>@<branch> --milestone "<summary>"` (add `--code` when relevant). Importing a session does not set `AGIT_SESSION` in the calling process.
- If resumed as a merge agent, follow the `AGIT_MERGE_TX` protocol in the agit skill.
- Never rebase or force-push AgentGit history; remove context with `agit revert <owner/repo>@<branch>#n.k`. `@` requires `AGIT_SESSION`.
<!-- agit:end -->

## Everything here is written in English

This repository is mirrored publicly and read by people and agents who share no language with
whoever wrote a given line. A comment in a language the reader does not have is worse than no
comment at all: it still occupies the line where the explanation belongs, so the file reads as
if the explanation is there.

Rules:

- **Code and files are English.** Identifiers, comments, doc-comments, assertion messages,
  Markdown under `docs/`, skill documents, shell and CI scripts, and every string that reaches
  a terminal. Quote with `"..."`, never `「...」`, and use no other fullwidth punctuation —
  the em dash, `→`, `…` and `·` are this repository's own typography and stay.
- **Commit messages and merge request titles are English**, bodies included.

The exception is text that is data rather than prose: lexical tables the search index depends
on, character classes that match another script, fixtures that exist to exercise CJK width or
segmentation, and captured transcript evidence quoted verbatim. Those keep their original
language, and the comment beside them says which of these it is.

## Comments state invariants, not history

A comment outlives the situation that produced it. A measurement is about the machine that
produced it, and "the previous version missed this" means nothing once that version is
gone — but both keep being read as if still true, which is worse than no comment at all.

Rules for comments in this repository:

- State the rule and what breaks if it is broken, in the present tense and conditionally
  ("if the budget is one counter, links get billed at the dirent rate"), not as a report of
  something that already happened ("the first version wrote it as one counter").
- No measurements with units, no ratios like "7 of 8 runs", no counts of tests or of table
  entries. If a number is load-bearing, it belongs in a benchmark or an assertion, where it
  is checked; a number in prose is checked by nobody.
- No references to a previous revision, a review round, or a fix. Keep the failure *shape*
  when it explains the rule; drop the fact that it once occurred.
- A test's doc says which property it pins and how a plausible wrong implementation would
  escape it — not how the test was arrived at.
- **If a sentence is already in the commit message, it does not belong in a comment.**
  Commit messages are where the history goes; they are read against the diff that produced
  them, and they never drift.

The test to apply: would this sentence still be true and useful after the machine is
replaced, the traversal is rewritten, or the pool is reconfigured? If not, it is a lab note.

`scripts/check-comment-invariants.mjs` is available as an optional local review aid. It scans
the comment lines a branch adds relative to its merge base and flags numbers with units,
ratios, counts of tests or table entries, and phrasing that points at another version or
review round. It judges shapes; deciding what a sentence was trying to say, and rewriting it
as the invariant, is still yours. It is not a CI gate.

## Tests and CI stay focused

- Add a test only when it protects a user-visible behavior, a protocol contract, or a failure
  boundary that a plausible implementation could break.
- Prefer one representative end-to-end check over duplicated fixtures and broad combinations
  that exercise the same code path.
- Keep CI fast: run the smallest relevant test target, avoid redundant builds and matrices,
  and do not add nonessential checks to required jobs.
- A local exploratory check can remain outside CI when it does not protect a maintained
  contract.
