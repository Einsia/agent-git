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

## Native transcript identifiers

Native record schemas identify linkage fields before local projection. Their
bounded identifier values are exempt from entropy-only detection. Provider
credential patterns and explicit blocks still apply. The same value in a tool
argument, result, or message text remains ordinary content.

Encoded carriers also honor explicit literals in their serialized form. A block
inside an unescaped string value is handled during decoded field scanning. A block
crossing JSON syntax or escape bytes protects the complete carrier as a recoverable
value, so repeated projection stays opaque and hydration restores call references
and JSON quoting together. When a carrier wraps existing placeholders, hydration
opens that mapping first and restores its fields after parsing JSON. Unavailable
field mappings and cyclic carrier mappings remain unresolved.

| Format | Recognized linkage |
| --- | --- |
| Codex | Session and fork IDs, response item IDs, tool `call_id`, turn and client IDs, and IDs inside compacted `replacement_history` |
| Claude Code / Desktop | Message IDs, message-chain IDs, tool-use IDs and tool-result references |
| OpenCode | Session, message and part IDs, parent and compaction references, tool `callID` |
| Hermes | Session and message IDs, `tool_calls[].id` and `tool_call_id`, including encoded tool-call arrays |
| OpenClaw | Event-chain and leaf references, tool-call IDs and tool-result references |
| WorkBuddy | Record and parent IDs, tool `callId` |

Publication grants these entropy exemptions only to verified saved events. An
ordinary file containing a native-looking object cannot establish that trust.

When restoring history, protocol fields containing secret placeholders receive
consistent short aliases before best-effort dictionary hydration. This preserves
pairing even if hydration stops between a call and its result. Codex call IDs
exceeding its length limit receive the same treatment. Alias allocation reserves
existing IDs, leaves tool data untouched, and changes only the materialized copy.
An untouched prepared session with invalid IDs is materialized again; appended
unsettled work retains the ordinary replacement safeguards.

## Verification

`cargo test --lib domain::secrets` checks scanner boundaries, including code-like
values, credential fields, provider matches, registered values and JSON carriers.

`cargo test --test file_line_commit_fresh_home` exercises `file add` and
`file commit`, then reads canonical Git objects directly. Reading the stored
bytes prevents local dictionary hydration from concealing artifact corruption.

These rules do not rewrite historical objects or remove entries from an existing
secret dictionary. Previously misclassified content values require review through
the normal explicit allowance workflow; protocol identifiers use the restoration
aliases described above.
