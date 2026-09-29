# Context for entropy discovery

Entropy is evidence that a value may be opaque, not proof that it is a credential.
Source identifiers and document references can have the same character distribution
as credentials. Automatic protection must preserve those roles without exempting
whole files or allowing the same bytes everywhere.

The shared scanner collects byte ranges with structural evidence before applying
bare entropy discovery:

- `secrets::syntax` recognizes bounded identifier roles, including import bindings,
  declarations, qualified call targets and inheritance names. It does not depend
  on a particular symbol spelling, filename or capitalization convention.
- `secrets::paths` recognizes resource locations in links, rooted paths and named
  path fields. This applies to text carriers generally, not just `README.md`.
- Media and repository identity handling retain their existing evidence checks.

The source classifier examines original text before escape normalization. Literal
contents and comments cannot supply executable syntax. Valid JSON carriers are
also inspected after decoding, so a source snippet embedded in a transcript uses
the same classifier as a standalone source file.

Call evidence requires a contiguous callee followed by balanced parentheses, a
simple argument list and a statement boundary. Prose surrounding a call-shaped
value, whitespace before its parentheses and incomplete expressions do not supply
that evidence. An apostrophe followed by an identifier without an immediate
closing quote is ambiguous with a lifetime or label unless it forms a plain
quoted phrase; otherwise the classifier withholds syntax exemptions for that
carrier instead of guessing its lexical state.

Only entropy-only findings are excluded by these ranges. Provider patterns,
explicitly registered secrets and credential-field discovery remain independent.
A declaration name can be preserved while a credential in its initializer is
still protected. There is no exemption for all CamelCase names, all strings with
slashes, all source files or already-published history.

This is a conservative lexical classifier, not a complete parser for every
language. Unsupported or ambiguous syntax retains entropy inspection. The token
budget bounds classification work; it must never bound secret inspection.

## Verification

`cargo test --lib domain::secrets` checks scanner boundaries, including code-like
values, credential fields, provider matches, registered values and JSON carriers.

`cargo test --test file_line_commit_fresh_home` exercises `file add` and
`file commit`, then reads canonical Git objects directly. Reading the stored
bytes prevents local dictionary hydration from concealing artifact corruption.

These rules affect new classification. They do not rewrite historical objects or
remove entries from an existing secret dictionary. Previously misclassified
values require review through the normal explicit allowance workflow.
