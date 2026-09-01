## Session version control (agit)

This project's agent sessions are managed by agit. Rules:

- Settle when a phase completes: `agit commit --milestone "<summary>"` (add `--code` when relevant).
- `agit status` at session start; if you were resumed as a merge agent, follow the `AGIT_MERGE_TX` protocol (see the agit skill).
- Never rebase / force-push; remove context with `agit revert @#n.k`.
