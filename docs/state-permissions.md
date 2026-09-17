# Local state permissions and recovery

Agit's state-directory helper uses Unix mode `0700`; new Link and migration
locks use mode `0600`. The caller's umask can further restrict those modes; the
helpers do not change the process umask or widen existing permissions. Link
replacement files and private archive publications also receive safe modes at
creation.

Ordinary authority carriers must belong to the effective user and satisfy
`mode & 0022 == 0`. Private archive carriers additionally satisfy
`mode & 0077 == 0`. Type, symlink, ancestor, content and session-identity checks
remain independent requirements.

## Required paths

These paths are relative to `AGIT_HOME` (normally `~/.agit`). The strict native
selection and archive paths have different requirements; the entire state tree
is not a private archive.

| Path | Use and existing protection |
| --- | --- |
| `store`, `store/<runtime>` | Strict native-Link selection checks ordinary directory authority and ancestry. Creation uses the private directory helper. |
| `store/<runtime>/<session-id>.json` | Selection checks a bounded regular Link image and native identity. Archive transition publication additionally checks ordinary carrier permissions, then publishes a private replacement. Ordinary writes already use `NamedTempFile`. |
| `store/<runtime>/<session-id>.json.lock` | Stable Link exclusion inode; created privately. The lock is not itself passed to the archive permission validator. |
| `store/.locks/branches/<digest>.lock`, `store/.locks/repositories/<digest>.lock` | Branch/repository exclusion; their directories and new lock files are private. They are not archive journals. |
| `repos/<owner>/<repo>/.git` | The common Git directory is ordinary authority when probing or settling archive state. Repository preparation uses protected parent directories. |
| `repos/<owner>/<repo>/.git/AGIT_MERGE_ARCHIVES` | Private archive namespace. |
| `AGIT_MERGE_ARCHIVES/<generation>.json`, `.recovery`, `.control` inside that Git directory | Private journal, recovery receipt and stable control inode. Their existing creation/publication paths enforce private modes. |
| `AGIT_MERGE_TX`, `AGIT_MERGE_TX.control` inside that Git directory | Transaction state and exclusion. Ordinary activation state is checked before private transition publication. |
| `AGIT_MERGE_TX.landed-<generation>.json`, `AGIT_MERGE_TX.aborted-<generation>.json` inside that Git directory | Private completion evidence. |
| `layout-v1.lock` | Startup exclusion. Startup recovery directory/evidence and migration spool locks also use permission-safe creation. |

`AGIT_HOME`, `repos`, owner/repository directories and other ancestors must prevent
untrusted replacement. External ancestors may belong to the effective user or
root; the validator permits sticky directories such as the system temporary
directory. Startup update-cache, configuration and migration preparation use the
same protected home-directory creation helper, so a first command cannot leave
an unsafe ancestor for later settlement.

## Repair a legacy path

If settlement reports an owner-controlled-state or ancestor-permission error,
use the diagnostic's path with the explicit Unix recovery command:

```sh
agit doctor --repair-permissions "$HOME/.agit/store/codex/<session-id>.json"
```

With a custom home, use that same configuration for repair and for the retry:

```sh
AGIT_HOME=/path/to/state agit doctor --repair-permissions /path/to/state/store
```

The command accepts only the named authority paths in the table and their
managed directory prefixes. It repairs the selected carrier and its ancestors
inside `AGIT_HOME`; selecting a directory does not visit its children. Repeat for
another reported carrier when needed, then retry the original import or commit.
`doctor --repair-permissions` cannot be combined with other doctor checks.

Recovery first verifies the complete selected path's scope, type and ownership.
It opens each component relative to a retained directory descriptor, refuses
symlinks and multiply-linked files, and rechecks inode identity before changing
permissions through the descriptor. Only forbidden permission bits are removed:
an ordinary `0664` Link becomes `0644`, while a private `0640` journal becomes
`0600`. Stricter valid permissions stay stricter. Bytes, inode identity and lock
exclusion are preserved. Repeating repair is harmless.

Unrecognized paths, project files, native-runtime files and external ancestors
are outside the repair scope. If an external ancestor is unsafe, its owner must
secure it separately. A concurrent substitution causes refusal; already secured
managed ancestors stay secured, and the command can be retried after resolving
the replacement.

Permission repair is not an integrity verdict on previously writable content.
The retried operation still performs its normal content, archive, session and
repository identity validation. It does not migrate data or change settlement
ordering, storage formats, commits or refs.
