# RFC: Hubs as remotes

Status: draft for review, revised for ledger 119 = A (plain publication only) and the approved
#174 restatement. Approver: the coordinator. Owners: privacy ("What each Hub holds"), agit-ux
("What people type and see"). Targets CLI 0.2.36.

## Summary

A folder's repository can be published on more than one Hub, the way a Git repository has more
than one remote. Pushing to a Hub the repository has never been published on registers it there
from scratch: a new repository, new registrations, content projected again from the local
history under the rules that apply to that Hub. Nothing that only means something on one Hub
(account ids, registrations, accepted publications, receipts) is sent to another.

Only the person's own sessions cross Hubs. A session reaches another Hub only if it was born
offline, or born under the account this device is signed in to on its birth Hub. Every other
session stays where it started, with every branch that depends on it, and the push says so. A
copy on another Hub is
outside the reach of the author's withdrawal and erase on the first Hub, so only the author may
make one.

Today a checkout is pinned to one destination (`agit.remoteIdentity`), and `agit push --to-hub`
(!520) delivers to a second Hub as a one-off: it refuses automatic and supervised push, gives
the destination its own session ids, and refuses any session whose creator it cannot map to the
signed-in account, offline births included. This RFC replaces the pin with state kept per
destination, admits exactly the sessions above, and defines how a commit that names one Hub is
published on another.

## What each Hub holds

### One local repository, one publication per Hub

The folder's repository and its history are local and belong to no Hub. A Hub holds a
*publication* of that history: its own repository record, and the commits the projection
produced for it. A destination is identified by `RemoteIdentity { hub, agent_id }`, as now.
Every piece of publication state is keyed by that identity, never by the owner/name a Hub shows,
which differs between Hubs and can collide across them.

| State | Scope today | Scope in this model |
| --- | --- | --- |
| Checkout pin (`agit.remoteIdentity`) | one per checkout | one per remote |
| Accepted ledger (`agit/privacy-accepted/<digest>`) | per destination identity | unchanged |
| Publication mode choices (`privacy_git::modes`) | per Hub and session | unchanged |
| Candidate cache session aliases | per repository id | unchanged |
| Secret-scan exclusions | per destination identity | unchanged |
| Session registration on the Hub | one, on the birth Hub | one per Hub the session is published on |
| Account rule journal and dictionary sync | per Hub and account | unchanged |
| Local protection rules | the device-wide union of every journal | unchanged |
| Repository allowances | per local repository | per destination identity (see "Allowances") |
| Repository blocks | per local repository | unchanged |

Most of the publication state is already keyed by destination. What has to change is the
checkout pin, the session registration and the commits that carry one Hub's identities.

### What never crosses

A push to Hub D never sends:

- another Hub's URL, account ids or usernames, in any object, metadata field or request;
- another Hub's registrations, accepted-ledger entries or receipts;
- a session that is not the person's own (see "Whose sessions cross"), or any commit of it;
- encrypted legacy history (see "Encrypted legacy history");
- the values of rules registered under another Hub's account. Those rules protect the push
  locally, as every rule on the device does, but their journal syncs only to its own Hub.

The link between publications of the same repository on different Hubs is recorded only on
this device, in the per-remote pins and ledgers. No Hub is told that another Hub holds a
publication.

### Whose sessions cross

A session may be published on D only if:

- (a) its birth record names no Hub (it was born offline), or
- (b) it was born on a Hub H under the account this device is signed in to on H, as the
  delivery's mapped-account check establishes from the device's own credential for H.

Everything else is never sent, and neither is any branch whose history contains its commits.
That covers sessions born under another account, sessions whose source account is missing,
changed or unverifiable, and anything that could only be matched by a username, a Git signature
or the account on D. No guess stands in for (b).

How a session that stays is reported depends on whether the person selected it. One they named,
or that a branch they named contains, makes the push partial. `--all` selects only what this
device can tell may go, so a session it leaves unselected is counted and listed (in `--json`,
in the first push's question, by `--dry-run`) without making the push partial. Whenever a
selected session was left out, the exit status reports a partial result, and a push in which no
selected session may go publishes nothing. What `--all` selects, and what the person reads, is
in "What people type and see".

### Projection for a destination

Publication is already a projection: the plain path keeps a commit's oid only when its
metadata and parents are unchanged, and otherwise writes a new commit that the ledger maps to
its source. This RFC adds one rule that applies to every destination, the birth Hub included:

**A commit's metadata may name only the destination Hub.** Fields that name D are published as
they are. Fields that name another Hub are mapped to D or dropped:

| Field | Names D | Names another Hub H |
| --- | --- | --- |
| `committer_account` | kept | replaced by this device's account on D when it is this device's signed-in account on H; dropped otherwise |
| `session_privacy` (`hub`, `creator`, `owner`) | kept | replaced by D's registration (see "Registration on a new Hub") |
| `fork_origin` (the whole record) | kept | dropped, never mapped: it binds H's repository, publication and history |
| `source_ref`, `source_commit` | kept | dropped, never mapped |

An origin URL is dropped wherever it names a repository on a Hub other than D, whichever Hub the
commit was settled for. That covers the origin in `code` (`<origin>@<sha>`, whose short sha is
kept) and `cwd_state.origin`, including the same field under its legacy name `code_state`. A
Hub here is any host this device holds a Hub credential, pin or remote for. An origin on any
other host is kept, as on any plain publication.

A commit whose metadata names no other Hub keeps its oid when its parents do, as on any plain
publication. In particular, code-only history that was never settled under a signed-in account
has the same oids on every Hub, as in Git. A commit that names H gets a new oid on D, and so do
its descendants.

The rule is symmetric. History fetched from D carries D's identities, and a later push of it to
H maps them in the same way. There is no special "birth" Hub, only the destination of the
current push.

Content is projected from the local commits, never from what another Hub published, under the
rules described in "Rules that protect a push". This is the same input and the same projection
as on the birth Hub, so the first push to a new Hub is a full publication: nothing in D's ledger
is accepted, every selected commit is introduced, and the scan covers all of it.

### Session ids

A session's id (`agit-` + a digest of its working directory and transcript) is a local content
identity, like a commit id, and plain publication uses it as the public id on every Hub.

Minting a fresh public id per Hub was considered and is not proposed. Two plain publications of
the same session carry the same turns, so anyone who can read both can link them by content.
A fresh id would not prevent that, and it would force a rewrite of every session commit on
every additional Hub.

`--to-hub` (!520) gave each destination its own ids and kept the mapping on the device.
Migrating those mappings, and the objects already accepted under such ids, is separate
follow-up work. It is never done by silently rewriting a Hub's accepted history.

### Registration on a new Hub

A session is registered on D the first time a push introduces it there, as on any Hub: the
scheme is `plain`, the Hub is D, and D records the pushing account as the session's creator and
owner, since the first publisher in a repository is a session's creator there. Because only the
person's own sessions cross, that record never names anyone the creator of someone else's
session. No other Hub's creator, owner or registration is sent.

There is no creatorless registration (`owner_source: first_committer`). A session that would
need one is not the person's own, and is left out.

Where D shows who brought a session or a turn there (`commit_pushes`), it says "published by"
or "pushed by", never "created by".

### Encrypted legacy history

Publication is plain only. History that an earlier version published encrypted is never
delivered to another Hub: not a commit whose local object is ciphertext, and not a session
registered encrypted on any Hub. Until #212 defines its handling, it stays, with the branches
that depend on it, and is reported as "Whose sessions cross" describes.

### Rules that protect a push

A push to D is protected by:

1. every rule on this device, from every journal, under any Hub or account (the device-wide
   union);
2. the rules of the account signed in on D, synced from D;
3. D's repository policy.

Rules registered under H's account protect a push to D because they are on this device. They
are not uploaded to D: each account journal syncs only with its own Hub. A device that cannot
read one of its journals withholds publication as it does now, naming the account.

### Allowances

An allowance lets a value through unreplaced to an audience, and audiences differ between
destinations. So an allowance is scoped to the destination it was made for, its
`RemoteIdentity { hub, agent_id }`, the key that secret-scan exclusions already use. It never
applies to another repository, on another Hub or on the same one. On a push to any other
destination, a value allowed elsewhere is a finding again, exactly as if it had never been
allowed: it is replaced unless the person allows it for that destination too. The first push to
a destination states how many such values it found again and how to review them for that
remote; later pushes show only that destination's own allowances. An allowance made before the
repository had a destination belongs to the first destination it is published to, and that
first push names it. Existing allowances belong to the destination the checkout is pinned to
today.

Blocks stay scoped to the local repository: a block only ever replaces more, so it applies on
every destination.

A destination whose own visibility changes later, from private to public, keeps the allowances
made for it. That is today's behaviour with one destination and is not changed here; making
such a change ask again belongs with the repository's visibility confirmation, not with
remotes.

### What a first push to a new Hub creates

On D:

- the repository, with the visibility the user confirms;
- one plain registration per session the push introduces;
- the projected commits and their refs.

Locally:

- the pin for that remote;
- D's ledger, mapping each source commit to the commit published on D;
- D's candidate cache entries and publication mode choices, if any.

Nothing changes on H, in H's ledger, or in the local commits.

### Status per remote

Whether a local commit is published on D, and how far a branch is ahead of D, is read from D's
ledger: a commit is published on D when D's ledger maps it to a public commit that D's refs
contain. Comparing local oids with D's oids directly is wrong as soon as one commit was rewritten
for D, which is the normal case on a non-birth Hub. Remote-tracking refs for D hold D's public
oids. Commits fetched from D that no ledger entry maps are new content, as in Git.

### Local store keys

Privacy state never uses owner/name as a key. A remote is added to the folder's existing
repository. Its owner/name on D are D's coordinates and need not match those on H. A collision
between two different repositories with the same owner/name on two Hubs can therefore only occur
in the local store layout used by `clone`. How the store distinguishes them is part of
"What people type and see".

### Changes in the code

This builds on !615, which applies rules (a) and (b) on the ordinary push path.

- `hub::identity`: a pin per remote in place of the single `agit.remoteIdentity`; reading an
  existing pin keeps working and names the remote it was created with.
- `audited_push`: the "pinned remote" and "destination differs from the pinned repository
  identity" checks apply to the selected remote's pin.
- `privacy_git::delivery`: admit a session under (a) or (b) only. An offline birth is admitted,
  where today it is refused. Every other session is left out with its dependent branches,
  instead of refusing the whole push. Plain ids are kept rather than given per destination. The
  metadata rules above apply: `committer_account` is mapped only for the device's own account;
  `fork_origin`, `source_ref` and `source_commit` are dropped; Hub origins are dropped from
  `code` and `cwd_state` (legacy `code_state` included).
- `privacy_git::sessions`: on the plain path, rewrite the metadata when a field names another
  Hub, not only when the scheme is not plain.
- `session_privacy::SessionPrivacyConfig::verify_registration`: accept D's stored registration
  for a session this push admitted.
- `secret_filter`: an allowance records the destination identity it was made for, and the scan
  for a push honours only the destination's own allowances. The first-push confirmation states
  how many values allowed elsewhere were found again.

### Tests (contract level)

With two fake Hubs (the `privacy_push` fixture):

1. A repository pushed to H and then to D: D's objects, metadata and requests contain no string
   from H (URL, account id, username). Code-only commits keep their oids, and commits that named
   H do not. H's refs and ledger are unchanged.
2. A value registered under H's account is replaced in D's publication, and D receives no
   request carrying that rule.
3. A push of three sessions to D, one born offline, one born on H under this device's account
   and one born on H under another account, publishes the first two and leaves the third out,
   with every branch built on it. No object, ref or registration of it reaches D. Named in the
   push, the third makes the exit status partial. Under `--all` it is not selected: the push is
   not partial, and `--json` lists it as not selected.
4. A session born on H under this device's account, pushed after the device's credential for H
   was switched to another account, is left out as unverifiable.
5. A `code` or `cwd_state.origin` (or legacy `code_state`) value that names a repository on H is
   absent from D's metadata. One that names a host that is not a Hub is kept.
6. History fetched from D and pushed to H carries no D identity on H.
7. After a push to D and a new local turn, the status reports the branch one turn ahead of D,
   computed through D's ledger.
8. A value allowed for H's repository is published unreplaced on H and replaced on D, and also
   replaced on a second repository on H. D's first-push confirmation counts it as found again.
   Allowing it for D publishes it unreplaced on D, without changing what H's allowance covers.

### Settled in review

- Allowances are scoped per destination identity, not per local repository or per Hub, so an
  allowance never widens silently to another audience; blocks stay per local repository.

- A creatorless registration (`owner_source: first_committer`) for another account's session is
  not built. Such a session is not the person's own and is left out (#174).
- `committer_account` is mapped to the account on D only when it is this device's signed-in
  account on H, and is dropped otherwise (#174).
- A session born offline is published on D under rule (a), with the pushing account as creator,
  as on any Hub.
- Sessions that are not the person's own stay with their dependent branches, and the rest of
  the push goes ahead. Only a selected session that stays makes the push partial, so `--all` in a
  team repository is not partial because of teammates' sessions. `--to-hub`'s whole-push refusal
  ends when the remote commands replace it.
- `fork_origin`, `source_ref` and `source_commit` are dropped on another Hub, never mapped
  through the ledger.

## What people type and see

The commands follow the accepted direction in `docs/rfc-git-aligned-commands.md`: Git's names
where Git has the concept, one question before publishing, local verbs that stay local, and an
error that ends with the command to run.

### The model, in Git's terms

| Git | agit | Where people see it |
| --- | --- | --- |
| remote | a Hub that holds the folder's repository, under a name | `agit remote -v` |
| `origin` | the Hub the repository was created on or cloned from | `agit remote -v` |
| remote URL | the repository's address on that Hub, `https://hub.corp.com/acme/app` | `agit remote -v` |
| upstream | the remote that automatic upload goes to; `origin` unless moved | `agit status` |
| `origin/main`, ahead and behind | what each Hub last accepted of a conversation, and how many turns it lacks | `agit status`, `agit show`, `agit log` |

A remote belongs to the folder's repository, as in Git; it is not a machine-wide list. Signing in
stays per Hub, as it is now: one sign-in serves every repository with a remote on that Hub.

One thing differs from Git: only your own conversations go to another Hub. A conversation is
yours to take elsewhere when it started under the account this computer is signed in to on the
Hub where it started, or while this computer was signed in nowhere. A teammate's conversation
that you fetched stays on the Hub where they published it, even in a repository you share.

### One step for each common goal

| Goal | Command |
| --- | --- |
| Also keep your conversations in this folder on the company Hub | `agit push https://hub.corp.com --all` |
| Move your work in this folder to the company Hub, new conversations included | `agit push -u https://hub.corp.com --all` |
| Bring the company Hub up to date later | `agit push corp --all`, which `agit status` prints when it is behind |
| Give colleagues on the company Hub a link | `agit share --remote corp` |
| Open a colleague's repository from the company Hub | `agit clone https://hub.corp.com/acme/app` |
| Use the company Hub for new folders on this machine | `agit login https://hub.corp.com`, then Enter at "Use it for new folders?" |

Pushing to an address adds it as a remote named after its host (`hub.corp.com`), so the next push
can name it; `agit remote rename hub.corp.com corp` shortens it. A first push to a Hub this machine
is not signed in to signs in as its first step at a terminal (the browser opens, as with
`agit login`); without a terminal it stops and prints `agit login <remote>`.

### Commands

- `agit remote [-v]` lists the remotes: name, address, the account signed in there (or the
  command to sign in), and which one takes automatic upload.
- `agit remote add <name> <address>`, `remove <name>`, `rename <old> <new>`, `set-url <name>
  <address>`. These change nothing on any Hub and say so. `remove` forgets the remote here; the
  repository on that Hub stays, and the message names `agit repo delete --remote <name>` for
  deleting it. Removing the upstream stops automatic upload until another remote takes it, and
  the message says that too.
- `agit push [<remote>|<address>] [<target>]`. Without a remote it pushes to the upstream, as
  now. An address starts with `https://` or `http://`. A remote name never contains `/` or `@`,
  and a target in its usual form (`owner/repo@branch`) always does. The one spelling both could
  use is the old bare repository name (`agit push payments`): a bare word is a remote when the
  repository has a remote of that name, and a repository name otherwise, as now. When it names
  both, nothing is pushed and the message shows how to write each. `-u` makes the remote the
  upstream after the push succeeds. `--all` pushes every conversation of the folder's repository
  that may go to that Hub (see "Conversations that stay where they started").
- `agit fetch [<remote>]` and `agit pull [<remote>]` read from the named Hub; the default is the
  upstream. Their `owner/repo` argument keeps its meaning, and a bare word is read as for `push`.
- `agit clone <address>` accepts a full repository address; `origin` is that Hub. The local
  repository takes the owner/name it has on that Hub. When this computer already holds a
  different repository under that name from another Hub, the clone adds the Hub's host to the
  name (`acme/app-hub-corp-com`) and prints it; targets then use that name.
- `agit login [<remote>|<address>]` signs in to the Hub behind a remote; with no argument it keeps
  today's order (`AGIT_HUB_URL`, then `hub.url`, then the public Hub). `--hub <url>` keeps working.
- `agit share --remote <name>` and `agit repo … --remote <name>` act on that Hub's copy, and
  `agit secrets review --remote <name>` on the values allowed for that remote's repository.
  Without `--remote` they act on the upstream, as now.

### The first push to a Hub

The first push of the repository to a remote asks once, with Enter required (RFC rule 3), and
shows what "What each Hub holds" says the person must decide on:

```
Publish alice/app to hub.corp.com as acme/app?
  Who can see it there: members of acme
  Conversations: 12, checked for secrets on this computer first
  Allowed for origin (agentgit.dev), checked again here: 2 values (review them: agit secrets review --remote corp)
  Left out: 2
    fix-login-review  continues fix-login, which another account started
    old-spike         published encrypted on agentgit.dev by an earlier agit
  Other accounts' conversations, staying on agentgit.dev: 5
    fix-login, docs-pass, ci-flake, bump-deps, perf-probe
    (to bring yours: sign in to agentgit.dev with that account)
  Stays on agentgit.dev in any case: share links, invitations, pull requests, Overflow posts
Press Enter to publish, or Ctrl-C to cancel. Nothing is sent until you press Enter.
```

The allowed-elsewhere, left-out and other-accounts lines appear only when there are any. The
allowed-elsewhere line names each remote the values were allowed for, since two remotes can share
a Hub. A value allowed for another remote is found again here, as if it had never been allowed
(see "Allowances"); pushes after the first show only this remote's own allowances.

The repository on the new Hub is chosen by the destination rules approved for `agit on` (join a
writable team repository of the same name, otherwise one under your account), applied on that
Hub. Pushes after the first are not asked about again. Agents get the same preview in JSON with
`next.human_action` set, and pass `--yes` only after the person agrees.

### Conversations that stay where they started

A conversation that is not yours to take elsewhere is never sent, and every conversation that
continues it stays with it, since sending that one would send its turns. The push names each
one with its reason and goes ahead with the rest:

| Why it stays | What the person reads |
| --- | --- |
| Started under an account this computer is not signed in to on its Hub: another person's, or one you used there before | `started on agentgit.dev under another account` |
| Its Hub is not signed in on this computer, or the sign-in cannot be read | `started on agentgit.dev, which this computer is not signed in to (sign in to bring it: agit login origin)` |
| Its Hub's record does not say who started it | `agentgit.dev does not record who started it` |
| It continues one of the above | `continues fix-login, which another account started` |
| It was published encrypted by an earlier agit (#212) | `published encrypted on agentgit.dev by an earlier agit; it stays there for now` |

The next step names the remote when that Hub is one (`agit login origin`), and its host
otherwise. If a conversation another account started is in fact yours under an account you have
since left, the way to bring it is to sign in to that Hub with it; nothing else stands in for the
sign-in.

What counts as left out depends on what the person asked for. A target or `-b` selects what it
names. `--all` selects every conversation this computer can tell is yours to take elsewhere, rule
(a) or (b) in "Whose sessions cross":

- **Selected, and staying.** A conversation the person named that stays for any reason in the
  table, or one `--all` selected that stays anyway (it continues one that may not go, or an
  earlier agit published it encrypted), is left out, and the push is partial. It prints
  `Published 12 to corp. Left out 2: fix-login-review, old-spike (each says why above)` and
  exits with 7, the policy code a privacy refusal uses, so a script never reads it as complete.
  A push in which nothing selected may go stops before asking and exits with 7, and so does an
  `--all` that selects nothing, with the line below: there is nothing of the person's to publish.
- **Not selected by `--all`.** A conversation started under another account, including one you
  used on that Hub before, or one this computer cannot tell is yours (its Hub is not signed in
  here, or the Hub's record does not say who started it), is not selected, so it does not make
  the push partial. When at least one stays, every push prints one line per Hub that names the
  Hub, counts them and points at the way out, after everything else it prints, so it is the
  last thing the person reads:
  `5 other accounts' conversations stay on agentgit.dev (to bring yours: sign in to agentgit.dev with that account)`,
  or, when that Hub is not signed in here,
  `7 conversations stay on agentgit.dev, which this computer is not signed in to (to bring yours: agit login origin)`.
  The first push's question names them, and so does `agit push corp --all --dry-run` on any
  later push. Otherwise every `--all` in a team repository would end partial and name the whole
  team's work.

`agit status` does not count a conversation as behind on a Hub it may never go to, and `agit show`
on one says where it stays and why.

### Where a conversation is published

`agit status` lists the folder's remotes and, for each, whether it has every saved turn:

```
Repository  alice/app
  origin  agentgit.dev/alice/app   automatic upload · up to date
  corp    hub.corp.com/acme/app    3 conversations behind · agit push corp --all
```

`agit show` names each Hub's page for the conversation, or the command that puts it there:

```
Published  origin  https://agentgit.dev/alice/app/…
           corp    not yet · agit push corp
```

`agit log` marks the last turn each Hub has, the way `git log` shows `origin/main`. All three
read each Hub's record on this computer of what it accepted ("Status per remote"), so a turn
whose published copy differs on a Hub still counts as published there. They change nothing;
`--fetch` asks the Hubs first (RFC rule 2).

### What never appears

Hub ids, repository ids, account ids, session ids on a Hub, registrations and keys stay out of
human output, as the RFC's other internal names do. People see a remote's name, its address and
the account signed in there. A link printed for one Hub is never shown as the link on another.
Where agit says who brought a conversation or a turn to a Hub, it says "published by" or "pushed
by", never "created by", as the Hub's pages do.

### Errors and their next step

| Situation | What the person reads |
| --- | --- |
| Unknown remote | `no remote named corp here. Remotes: origin. Add one: agit remote add corp <address>` |
| A bare word names both a remote and a repository | `corp is both a remote and the repository alice/corp. Push the repository: agit push alice/corp. Push to the remote: agit push corp alice/app@<branch>` |
| Not signed in to the remote's Hub, no terminal | `not signed in to hub.corp.com (remote corp). Sign in: agit login corp` |
| Nothing selected may go to that Hub | each conversation with its reason, then `nothing to publish to corp` |
| `AGIT_HUB_URL` names a Hub that is not a remote of this repository | `this repository is on agentgit.dev, not hub.corp.com. Publish it there too: agit push https://hub.corp.com` |
| A remote's Hub answers at a new address (until #105) | both addresses, and `agit remote set-url origin <new address>` |
| The repository name is taken on that Hub by someone else | the name it would use instead, as `agit on` does |

### Agents, the skill and MCP

`agit status --json` gains `remotes`: name, address, signed-in account, `upstream`, and the
conversations behind. `agit push --json` names the remote it pushed to and, on every push,
lists each conversation it left out: its reason as a fixed code, `selected` (true when the
person's selection included it, which is what makes the push partial; false for one `--all`
does not select), and the command that is the next step, if any. The codes keep apart what a
script may treat differently: `another_account`, `hub_signed_out`, `starter_unknown`,
`continues_left_out` and `encrypted_legacy`. A script reads both kinds, and can fail on
`hub_signed_out` alone, without parsing the printed lines. MCP tools that publish take an
optional `remote`. The skill mentions remotes only when the person asks about another Hub;
everyday work keeps using the upstream without naming it. It tells agents that a conversation
left out is reported to the person, never copied into a new one to get it across.

### Compatibility

- Every existing repository gets `origin` from the Hub it is pinned to. Nothing changes for people
  who use one Hub, and nothing is uploaded anywhere until someone pushes to a new remote.
- `AGIT_HUB_URL` and `hub.url` keep choosing the Hub for new folders and for `agit login` without
  an argument. A command on an existing repository uses its remotes; a variable naming another Hub
  is refused as above, as it is refused now, with a next step added.
- `agit push --to <owner/repo>` publishes to another repository on the Hub being pushed to, as it
  does on the one Hub now. `--to` for a device-local remote-control repository is unchanged.
- `agit push --to-hub <url>` keeps working until step 3 ships. It then does what
  `agit push -u <address>` does and prints that form; the ids it gave each destination are
  migrated as "Session ids" says.
- The JSON contract (schema v2) only gains fields.

### Steps

1. `agit remote` and `agit remote -v`, with `origin` read from the existing pin. Read-only; no
   model change. This is !619, taken over or redone.
2. Published state per remote in `status`, `show` and `log`, once each Hub's accepted turns are
   recorded per remote (privacy's part).
3. `agit remote add` and the first push to another Hub, on the delivery #174 builds (the
   own-sessions rule and per-Hub registration), with the left-out lines and exit status above.
4. `-u`, pushing to an address, cloning by address, `share --remote`, `login <remote>`, and
   `--to-hub` as `-u`.
5. Skill and Hub docs.

Each step is one merge request, verified with real Claude Code and Codex sessions against two Hubs
on the Sim Hub host. Step 1 can ship before #174; steps 3 and 4 follow it.

### Open questions for review (commands)

2. Automatic upload goes to one upstream only. Keeping a second Hub current takes
   `agit push corp --all`, which `agit status` prints. Upload to every remote is left out until
   someone asks for it.
3. A remote added by pushing to an address is named after the host (`hub.corp.com`), and a
   cloned repository whose name is taken locally gets the host added to its name. A shorter
   default is possible but would need a rule people can predict.
