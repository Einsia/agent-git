---
name: agit-privacy
description: Configure a local privacy allowlist, preview candidates, and authorize private session recovery.
---

# Privacy policy

## Guide the user through password operations

When continuing an encrypted cloned session requires private recovery, explain the requirement
to the user instead of only reporting that `unlock` needs an interactive terminal. The repository
viewing password unlocks the viewing key that decrypts the original session context. It is set
by the repository owner or administrator; it is not the user's Hub login password or API token.
Ask the user to obtain it from that owner or administrator through a private channel. Repository
read access is still required for private repositories, and the password grants no write or RC
device access.

Give the user a copyable command with the actual selected repository, ref and workspace:

```sh
agit privacy unlock OWNER/REPO@REF --workspace /absolute/workspace
```

Tell them to run it in their own interactive terminal on the machine that will execute the
agent, using the same AgentGit store (`AGIT_HOME`) as the intended session. For a remote RC
device, this means a terminal on that device, not merely on the browser's computer. Password
entry is local and does not echo. Do not ask for the password in chat, place it in tool calls,
or retry with `--yes` or a pseudo-terminal to bypass the handoff. If the CLI reports that
password input requires interaction, wait for the user to complete the command before
continuing work that depends on recovered context. Do not describe permission, network or
integrity failures as a missing password.

Explain this in the user's language, for example: "This repository is encrypted. To continue
from its original session context, obtain the repository viewing password from its owner and
run the following command in a terminal on the execution machine. Enter the password there,
not in this chat, and tell me when it succeeds so I can continue preparing the session."

CLI password initialization, password changes and key rotation likewise require user-operated
terminal input; give the corresponding command below and explain whether it creates a password,
changes the password for the same key, or creates a new key for future publications. Browser
initialization below is an alternative for setup only, not execution-machine recovery. An
explicitly saved, valid key can support `unlock --use-saved-key` without password entry; do not
enable key retention unless the user chooses it.

Cloning, inspecting the readable projection and starting the RC daemon do not themselves
require decryption. Unlock is needed to recover original private context for continuation.
After recovery, follow the selected session's resume/run rules and writer checks; successful
unlock does not itself start or attach a runtime. RC still protects outgoing content, so local
recovery does not promise an unredacted original in the web Workspace.

For agent-assisted onboarding, let the user set the viewing password in the browser:

1. Complete login and local initialization/import. Use `agit init NAME --auto-push=false`
   for initial setup; keep automatic publication disabled until setup is complete.
2. Run the following command, retaining this explicit Hub and repository on every retry:

   ```sh
   AGIT_HUB_URL=https://hub.example agit privacy init alice/agent --browser --json --yes
   ```

3. Read the existing JSON envelope's `result.value`. Exit code `8` with
   `status: setup_required` includes `setup_url`, `repository`, `hub`, and immutable
   `agent_id`. Show that link to the user and ask them to set the password there.
   Wait for their reply; do not request the password in chat or poll the service.
4. Rerun the same command after their reply. Proceed only after exit code `0` and
   `status: ready`. A repeated setup-required response means setup is still needed;
   authentication, permission, identity, mode, malformed-key, and network failures are errors.
5. Follow normal publication review and push, then return the session and invitation links.
   Enable `agit config --repo alice/agent push.auto true` afterward only if the user
   selected automatic publication.

Browser mode returns immediately, never prompts for a password, and never initializes, rewraps,
or rotates a key itself. `--yes` authorizes creation of a missing repository, privately by
default; `--public` explicitly changes that creation choice. Existing key readiness is checked
through the identity-bound publishing-key API. The CLI receives only the public key.
The website link selects initialization only and validates the expected immutable identity.
Before returning setup or ready, the CLI retains the verified Hub and repository ID in the local
repository identity pin. Retries refuse a replacement under the same name. If the remote already
exists but there is no local repository, initialize or clone it first; an unrelated local remote
cannot be rebound by browser setup.

These instructions ship with the supporting CLI. Before using an older installed CLI, check
`agit privacy init --help` for `--browser` and upgrade if absent. Website quickstart changes
that require this flag must follow the supporting CLI release.

Administrators can alternatively configure the repository viewing password in a terminal:

```sh
agit privacy init alice/agent
agit privacy change-password alice/agent
agit privacy rotate-key alice/agent
```

Passwords are entered locally without echo; new passwords require confirmation. `--yes` cannot
supply one. Initialization can create a missing repository after a separate confirmation, privately
by default (`--public` explicitly selects public visibility). Existing imported history is preserved.
Configuration uses the fetched immutable repository identity and version. A conflict requires a
fresh invocation and confirmation. Password changes retain the key; rotation creates a new key and
retains historical key records. Initialization configures the repository before content publication.

Password operations require an encrypted repository. `privacy init` cannot change
an existing ordinary repository's mode, including an empty one; create a different
repository for encrypted publication. When creating a missing repository, init
uses its local creation intent or the global `privacy.encryption` default. Use
`--encryption=true` to select an encrypted new repository explicitly. A disabled
selection is refused before creation; use `agit repo create --encryption=false`
to create an ordinary repository without a viewing password.

`agit privacy` manages the policy used before a session, export, share, or push enters the
publication pipeline. The policy is stored below the local AgentGit repository's Git common
directory and is never committed to the AgentGit tree.

The default policy is conservative: no workspace is authorized until one is configured, and the
default include list covers common source, documentation, test, example, and project metadata
paths. Sensitive names such as `.env`, SSH keys, and private credential files remain excluded even
when an include pattern matches them. Memory stays private until an explicit memory pattern is
added.

System/device mandatory exclusions override repository includes and branch settings. The CLI
loads the platform's system policy and the absolute source paths listed in
`$AGIT_HOME/privacy-policy-sources.json`. `privacy policy show` reports the effective rules, while
repository edits save only local authorization. Source versions and contents affect the policy
digest and require renewed publication consent when changed. Missing configured sources block
processing. See `docs/privacy-envelope.md` for paths and schemas; cloned policy text does not
install mandatory sources or authorize source-machine directories.

```bash
agit privacy policy set-workspace /work/project --repo alice/agent
agit privacy policy include 'src/**' --repo alice/agent
agit privacy policy allow-memory 'team.md' --repo alice/agent
agit privacy policy show --repo alice/agent
agit privacy preview --repo alice/agent --branch work src/main.rs --memory team.md
```

`privacy preview` evaluates only the paths supplied to the command. It does not recursively walk
an authorized directory. Every report includes a policy digest so a later publication step can
require a new preview when the strategy changes.

The publication path layer keeps a device-local alias table beside the policy file. `privacy preview`
records aliases as it evaluates candidates, so repeated previews use the same names. Authorized
roots use stable logical names such as `<workspace>/src/main.rs` and `<contracts>/contract.yaml`;
excluded or outside-root files use stable `<private-file-N>` placeholders. The reverse mapping is
never part of tracked repository content or a public preview.

The shared projection path aliases paths, applies configured replacements, and scans the rewritten
result before public bytes are produced. An excluded candidate produces no bytes, and an allowed
candidate can be sealed in the versioned private envelope used by later publication integrations.

Recover an encrypted snapshot after binding a workspace:

```sh
agit privacy unlock alice/agent@work --workspace /work/project
```

The command reads the recipient from the selected local envelope, obtains the matching accepted
publication key record from the source Hub, and prompts locally for the repository viewing
password without echo. Public repositories permit anonymous lookup; private repositories retain
their read ACL. Passwords and plaintext keys are never sent to the Hub. The exact repository ID,
commit, session and recipient must match before decryption.

Recovered session files are stored under the Git common directory's
`agit/privacy-recovery/<commit>`, outside tracked content, with the selected workspace. Continue
with `agit resume alice/agent@work`; `--no-launch` prepares the runtime without starting it.
Resume verifies the recovery manifest and installs the original private VIEW using ordinary
resume rules, preserving native messages, models and tool records within the same runtime.
Cross-runtime conversion uses its ordinary loss warnings. Settlement appends new events to the
original private LOG and VIEW without returning LOG-only conversation to context. Untouched legacy
quoted sessions can be rebuilt; those with new conversation must be committed first. Anonymous
resume does not supply an author identity: ordinary `commit` requires login, and push separately
requires destination write access.

Add `--remember-for 24` to store the key in the OS credential store for that many hours (1-720).
`--use-saved-key` reuses an unexpired key only after online read-access and publication checks.
These options are mutually exclusive. The scope is canonical Hub, immutable repository ID,
recipient and public key. Logout and repository renames do not change the scope; key rotation
or name reuse cannot select another key. Same-key password rewrapping retains the saved key.

`agit privacy forget-unlock alice/agent` removes the repository's saved-key slot locally, including
after logout. It uses the local immutable identity and takes no account selector. Forgetting or
expiry does not remove already recovered session files or restored native history.

For direct editing, `agit privacy policy file --repo alice/agent` prints the JSON path. The file
must retain the current policy version and is validated before it is used.
Its `metadata` setting defaults to `"minimal"`; `"project"` additionally publishes the sanitized
project origin and worktree summary. Push retains these processed fields in `metadata.privacy`
in both the public envelope and Git metadata. They are presentation facts, not local permissions
or workspace bindings.
