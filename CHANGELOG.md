# Changelog

Every notable change to agit, the AgentGit CLI, by release. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/). A version's section here is the body of
its [GitHub Release](https://github.com/Einsia/agent-git/releases), and the
`@einsia/agent-git` npm package ships this file.

## [Unreleased]

## [0.2.48] - 2026-11-10

### Changed

- Files in an agit repository other than conversation records, such as files added with `agit file`, memory and
  skills, are now pushed as they are, the way git pushes files, with no privacy check. So are session branch names. A
  push no longer stops because one of them holds a value the check would replace, or is too large to check.
  Conversation records are still checked when they're saved, as before.

### Fixed

- A conversation in which the privacy check had left part of a record out could stop saving at its next turn. It now
  goes on saving, and the part stays left out.
- agit now reads the requests in a Codex conversation that Codex imported from another agent, which it missed when
  Codex kept them only as event records, and dates the imported turns by when they started instead of when they were
  imported.

## [0.2.47] - 2026-11-09

### Changed

- `agit project list --json` says why a folder that should upload isn't, which runtimes still need setting up there, and
  which records it couldn't read; `agit project sync` reports every session, and a session still in its first turn or
  left out is no longer a failure.

### Fixed

- `agit on`, setup and doctor no longer wait without limit on a busy Codex, leave nothing running from it, and no longer
  end silently when Codex exits without answering; doctor says when Codex didn't answer in time.
- A value under a field whose name ends in `password`, `passwd`, `passphrase` or `pwd` is now replaced before anything is
  shared even when it reads like ordinary words, and so is the credential in an HTTP Basic `Authorization` or
  `Proxy-Authorization` header. Documentation placeholders such as `YOUR_PASSWORD`, and a password field set without
  quotes to a Python class defined in the same file, stay as they are.
- With `privacy.entropy = replace`, a full-length hash written after `sha1:`, `sha256:`, `sha384:` or `sha512:` is no
  longer replaced for looking random. A Terraform `password = "…"` now has only the value inside the quotes replaced,
  not the quotes. Values under credential fields, and your own blocks, are still replaced as before.
- A conversation saved with `privacy.entropy = replace` could stop saving with "the native transcript does not continue
  this branch's settled content". It happened when a name in one of its records, such as Claude Code's
  `cacheCreationInputTokens`, had been replaced and this computer couldn't restore it. Such a conversation now
  continues, and what was already saved stays as it is.
- With `privacy.entropy = replace`, a JSON member name made only of letters, underscores or hyphens, such as
  `cacheCreationInputTokens`, is no longer replaced as if it were a credential. What it holds is still checked, and your
  own blocks still apply to the name.

## [0.2.46] - 2026-11-09

### Added

- `agit push <repo>@<branch> --keep-both` keeps both sides of a branch that diverged from the Hub's copy in one step: it
  publishes this checkout's line as `<branch>-local` beside the Hub's, which stays as it is. At a terminal, the refused
  push asks the same question, and `--yes` doesn't answer it.
- A message you type into a Claude Code turn that agit runs for remote control now reaches the model at its next tool
  step instead of after the turn. One that can't reach the running turn is sent as the next turn or reported as not
  delivered. On the Hub it appears as the message you typed.

### Changed

- Remote control shows sessions as they are, live and in their history, to you and the colleagues in your workspace:
  nothing is held back for a privacy check any more. What `agit save`, a turn's end and `agit push` publish to a
  repository is checked as before.
- A colleague's message in a shared conversation now runs like your own: in any mode, in Codex, in your home folder,
  with approvals anyone in the workspace can answer. Permission mode and model stay yours to change.
- `agit merge` says the merge agent will take over the terminal, names `--manual`, and asks before starting it; `-y`
  skips the question.

### Fixed

- When another computer added a turn to a plain session on top of what this computer uploaded, resume, run and pull
  bring that turn over as the Hub holds it and continue from it, instead of refusing the session as continued in two
  places.
- A conversation another session continues, as `agit resume` does, now says so at its start and at each turn's end: its
  turns there stay on this computer and aren't uploaded, and `agit resume <repo>@<branch>` continues the branch.
- `agit repo rename` and `agit clone --mine` move a repository's worktrees with it. A worktree an older agit left in
  another repository's place is named by `agit doctor`, and `agit doctor --repair-worktrees <owner/repo>` moves it home.
- In a folder whose repository has automatic upload off, a new conversation's start line, `agit status` and the agent's
  note say its turns are saved on this computer and that `agit push` publishes them, instead of saying they upload at
  the end of each turn. That start also no longer asks the Hub who can read the repository.
- When an automatic upload didn't go through, the line that says so names the exact `agit push <repo>@<branch>` that
  goes on from it, as a bare `agit push` can't act on a conversation that has ended. A conversation whose folder doesn't
  upload it by itself is told it's saved on this computer and not uploaded, rather than that its upload failed. For an
  upload that failed with no named reason, `agit status` shows the first line the push reported.
- `agit save` names a conversation that remote control keeps for a folder enrolled with the Hub by the Hub repository it
  is for, or says it has none yet, instead of the internal desktop-…/project-… repository name.
- Stopping or restarting remote control right after a conversation's turn was saved no longer leaves a lock file that
  stops later turns from being saved; agit clears a lock it can tell nothing still holds, and otherwise says which file
  it is, when it is safe to delete, and the command.
- `agit status` keeps a row for every local repository, however many branches another has, and its "incomplete" warning
  names how many branches of which repository were left out, with the `agit branch --repo <owner/repo> --all` that lists
  them.
- A memory file you allow with `agit privacy policy allow-memory` is collected as it stands by the next sync or turn
  end, even if you edited it before allowing it; the command names the files it covers.
- After `agit memory distill`, the hint also names the session branch's push when that branch isn't on the Hub yet, not
  only main's.
- `npx create-agit` no longer says your shell selects another agit when the only other one is npx's own temporary copy.
- On Windows, reading agit's output no longer waits on a helper that outlives the command.

## [0.2.45] - 2026-11-08

### Added

- `agit repair merges <owner/repo>@<branch>` copies a session branch whose earlier merges recorded its history again to
  a new branch (`<branch>-repaired`, or `-b <name>`) without those repeats, keeping every turn's dates. `--dry-run` lists
  what it would leave out. The original branch is unchanged and nothing is uploaded. A share refused because of such a
  repeat points at the dry run.
- `agit project list --json` now carries `uploads_not_through`, the same list of automatic uploads that didn't go through
  that `agit status --json` shows.

### Fixed

- A Codex, Claude Code or WorkBuddy turn on a computer that isn't signed in to agit now says the turn isn't saved and
  that `agit login` signs in, instead of ending silently.
- With automatic upload off for a repository, agit no longer contacts the Hub for that repository on its own, and no
  longer reports each turn as a failed upload: `agit status` says the turns are saved here and that `agit push`
  publishes them. What you ask for still uploads, through the same checks as `agit push`: `agit save` (the conversation
  you name, or this folder's unsaved sessions; `--all` and `--today` leave that repository out), `agit project sync` and
  the menu bar's Retry. None of them answers a create-or-make-public question for you.
- An automatic upload that didn't go through now says so at the next turn's end or start and in `agit status`,
  including when the conversation also continued on another computer.
- An automatic upload refused because the Hub moved on with nothing new on this computer now says so, and a failure with
  no known cause no longer promises that the next turn tries again.
- OpenCode conversations kept in its newer (V2) storage are now found: `agit save` saves them, with their tool results
  and attachments, and `agit resume` restores them. Attachments other than images and PDFs are left out of what's
  published. As before, no folder uploads OpenCode conversations by itself.
- A Codex conversation continued on a second computer saves its turns there; a turn that can't be saved now says so at
  the end of the turn.
- `agit new`, `agit run` and `agit resume` with `--as codex` hold their own branch and give Codex the team memory index
  even while Codex's shared background server is running.
- `-y` no longer adopts personal memory (CLAUDE.md, AGENTS.md, skills, memory files) into a new repository's main. agit
  names the files it found; you choose each one at a terminal, or add it yourself, and no hint suggests `-y` for it.
- A runtime that `agit new`, `agit run`, `agit resume`, `agit fork` or a merge launches no longer inherits that
  command's `-y`, `-q` or `--tui`, so the agent's own agit commands ask as usual.
- The local daemon that runs remote-control sessions no longer keeps the `-y`, `-q` or `--tui` of the agit command that
  started it, so its sessions' agents are asked as usual.
- A Codex conversation that is running now appears in the remote-control session list even while Codex is busy writing
  its own index, and `agit rc sources list` says why a list can't be read.
- While a conversation's live view waits for the privacy check on that computer, agit now tells the Hub why the check is
  still waiting, naming only the checker's own state (busy, restarting or not responding), and history errors shown to
  workspace members no longer say anything about a record's contents or the owner's privacy rules.
- Stopping agit's background service while a conversation is being archived no longer leaves that archive stuck; if a Git
  lock is left behind anyway, `agit doctor` names the file and says how to clear it.
- Base64 in a session record that names a text type, such as a file attached to a message or a data URI a tool prints,
  is now decoded and checked like the rest of the record before the session is published, and left out if it can't be
  read as text. Base64 of a binary file, or with no type beside it, is published as it is.
- `agit merge` no longer records turns a second time when the source already holds turns that are the target's own (a
  shared start, or the Hub's published copy, including one from another repository). The merge says how many new turns
  from the source it recorded, or that there is nothing to merge.
- `agit push <repo>@main` after `agit memory distill` is no longer refused for the account agit records in main's own
  metadata; a push refused for a value in a file names the commit in your checkout and how to remove the value.
- When `agit push` finds nothing to send, it says the branch is already on the Hub, so a push retried after one that
  failed, such as a 502 for a push the Hub took, no longer leaves open whether it got there.
- When `agit push <repo>@<branch>` finds nothing new to upload, it says the branch is on the Hub and names the session
  it holds, by its opening words and when its last turn ran, so a look-alike branch pushed by mistake is easy to spot.
  If that branch isn't the conversation running in this terminal, the push says so, and so does `agit status` for the
  session it's set to.
- `agit scan <repo>@<branch> --secrets` checks what `agit push` of that branch would send, main included when the push
  carries it, instead of every branch, and says which it scanned. `--all` or no branch scans the whole repository.
- `agit push` no longer tells a plain session that its published copy is encrypted when another computer has added a
  turn on the Hub; it names the other computer's turn and points to a separate checkout of the Hub's copy.
- When `agit push` finds that main has diverged from the Hub's, it points to `agit pull` (which merges the files) and
  says when both sides hold the same files, instead of offering only a fork or a merge by hand.
- Turning agit on in a folder for a repository that already exists on the Hub now starts from the Hub's main, instead
  of creating a separate main that `agit status` shows as diverged.
- A main this computer started on its own, because its first save couldn't use the Hub's, can now take the Hub's main in
  with `agit pull <repo>@main`. The pull names the files the person added at its start and who can read the repository,
  and leaves their upload to `agit push`.
- The note after a push that a folder doesn't upload by itself speaks to the person when they run agit at their own
  terminal, and lists each git worktree on its own line. It and the notes at a conversation's start and resume say who
  can read the destination in the same words, including the people granted access to an organization's repository.
- `agit memory distill <file>` says whether the file was never collected onto the branch, with what keeps it on this
  computer, or is unchanged against main, instead of "already matches main, or never existed"; hints that name
  `agit privacy policy allow-memory` say that the file is then saved to the repository and uploaded with the branch.
- `agit distill` names the repository whose main it carries memory into, in each question and its result, and its manual
  says it reaches only the session's own repository's main.
- `agit save` names a conversation it can't read, and says why, instead of failing with "No such file or directory"; a
  step that can't run in a conversation's folder says which folder.
- In a folder turned on before agit recorded which OpenCode conversations it already had, a plain `agit save` left every
  OpenCode conversation out. The next `agit save` or `agit on` there now looks once. The OpenCode conversations from
  before then stay out until you name one, a plain `agit save` takes the ones started after, and `agit save` finds
  OpenCode conversations by their titles.
- `agit save` and the `agit` overview now find Claude Code, Codex and OpenCode conversations when the runtime's command
  isn't on your shell's PATH, as when an app or an editor runs it.
- `agit save <id>` for a conversation whose folder was removed says where it ran and that it isn't saved from here,
  instead of "no session matches".
- `agit save` says a conversation already saved in another repository or branch is saved there, instead of reporting a
  failed upload, and offers a retry only where one can help; a conversation whose transcript no longer continues its
  branch names the import that keeps both.
- When Git can't inspect one of your agit repositories, `agit status` names the repository, the worktree and what Git
  said, and goes on if the folder you're in doesn't use it. `agit doctor` reports it as a finding instead of refusing to
  run.
- A conversation in a folder agit isn't saving now says so once, at its start, with the command that turns the folder
  on; `agit config notice.unsaved false` keeps it quiet. Hints about a folder's earlier sessions name `agit save <id>`
  for one, and tell an agent to ask the person instead of running `agit save --all`.
- `agit status` says a conversation isn't saved when nothing asked agit to save its folder's conversations, and names
  `agit import <id>`, instead of "incomplete claim; process unverified"; in a folder whose project saves it, it says the
  conversation is saved when its turn ends.

## [0.2.44] - 2026-11-07

### Added

- When `agit pull` stops on a file that changed on both sides, the terminal shows each conflicting place with both
  versions and who last changed each, and one `agit merge --into <target> --continue` settles every file with
  `--keep-mine`, `--take-theirs` or `--resolved` (after `--edit <path>` opens it in your editor), instead of editing
  agit's checkout by hand.

### Changed

- Commits in a repository agit made for a project on this computer, as remote control does, now carry that repository's
  own agit identity instead of your account's name and email. Commits made before the upgrade keep the name they had.

### Fixed

- Some branches with a Codex conversation that an agit before 0.2.43 linked hold their uploads until you confirm that
  conversation with `agit branch confirm` (or fork at your own turn). `agit status` names them.
- The request inside a WorkBuddy message's `<user_query>` block is published, as it is for Cursor, and a session
  published earlier shows its requests from its next push.
- On a computer that continued a session it took from the Hub with `agit run`, `agit run` and `agit resume` no longer
  refuse the branch as having tracking commits that aren't integrated right after its own upload.
- `agit config --repo <repo> push.auto false` now works before the repository's checkout exists on this computer, and
  the off holds when the first saved conversation creates it.
- Resuming a session in a changed folder as another runtime asks once; choosing to continue no longer meets a second
  question whose default cancels.

## [0.2.43] - 2026-11-06

### Changed

- Sessions you published encrypted now continue as plain at their next push: new turns upload the way a plain session's
  do, earlier turns show no more than they did before, files the session changed while encrypted are not uploaded, and
  the originals stay on this computer.
- A session whose history includes an encrypted snapshot publishes what was already published, or is refused with
  nothing uploaded, instead of always being refused.
- `agit privacy mode … encrypted` and `agit privacy access grant` are retired; they say why and change nothing. Access
  you already gave to originals stays for earlier snapshots, and `agit privacy access show` and `revoke` keep working.

### Fixed

- OpenCode conversations that `agit save` takes are no longer all withheld as "agit cannot verify where the
  conversation started".
- An automatic upload refused because the repository's publication rules changed while it ran is tried once more from
  the start; if it's refused again, agit says so, and the turn uploads with the next one or `agit save`.
- When a conversation's automatic upload stops, the message at the end of the turn now says why accurately: it points at
  `agit status` only when status shows the privacy check it is waiting for, and says so when this computer keeps the
  conversation from uploading.
- A one-turn `claude -p` conversation in a folder set up with `agit on` is now saved and uploaded when its turn ends,
  instead of only when another conversation starts there.
- `agit resume` and `agit run` no longer refuse a session as "continued in two places" right after its own upload, when
  the upload left records out.
- `agit status` lists the current folder's sessions first and never cuts them from the page, and says how to see the
  rest when the list is longer.
- A message from a colleague to a conversation running in a terminal names its sender once.
- Switching a fully saved Claude Code conversation to another agent with `agit resume --as` or `agit run --as` no longer
  refuses it as unsettled when `agit commit` has nothing to save.
- `agit status` no longer keeps listing an upload as waiting for the privacy check after it has landed on the Hub under
  another repository name.
- `agit run --as codex` and `agit new --as codex` no longer record other Codex conversations into their branch; a branch
  an earlier version may have mixed stays on this computer until you fork it at your own last turn or lift the hold.
- A computer signed in to more than one Hub now uploads each relayed turn to the Hub its workspace belongs to, instead of
  retrying "publication Hub changed" forever.

## [0.2.42] - 2026-11-06

### Added

- Published Hermes and OpenClaw sessions show the prompts the person typed: in Hermes, those from their own console; in
  OpenClaw, those it marks as the owner's.

### Changed

- Published OpenCode sessions show the prompts the person typed.
- A push stops before uploading anything when a text file it adds holds a value your privacy rules replace, or is too
  large to check, and names the file so you can move it or keep the branch local.
- `agit scan --secrets` says which parts a push replaces and which it sends as they are, advises not to publish a branch
  whose Agent history holds a secret until the findings are checked, to have its owner revoke and rotate a real
  credential, and to continue in a new session; it suggests allowing a value only when it's yours and not a credential.
- `agit tag -m` is refused on a session turn: a tag there is published without a message, and its name is held to the
  check a branch name meets.
- An owner typed in uppercase is accepted as the lowercase owner, and a repository name typed in another case reaches
  the existing repository and its checkout.

### Fixed

- A turn whose transcript ends with a stop-hook summary now finishes archiving and uploads, instead of retrying without
  end.
- A turn whose archive met a slow privacy check is tried again within seconds.
- When the Hub is slow to answer a sign-in renewal, agit tries again instead of signing the device out, keeps saving
  conversations on this computer while signed out, and says once that `agit login` is needed when the Hub has really
  ended the sign-in.
- After an agit upgrade, the running daemon keeps settling and archiving conversations with its own version instead of
  failing or running the new binary's helpers; a privacy check that cannot run is retried instead of leaving turns
  unsettled.
- An agit upgrade no longer waits for ever behind a conversation left idle; after 30 minutes idle it is paused for the
  update and continues with your next message.
- Stopping the agit daemon, including with SIGTERM, interrupts a running turn the way a person's interrupt does instead
  of cutting it off mid-tool; the conversation shows as interrupted and continues with the next message.
- When the agit daemon crashes or is killed during a turn, the next start closes that turn so the conversation can
  continue, and archives what was recorded; resuming a conversation left with an unfinished tool call no longer fails.
- Of two terminals open on one conversation, the first stays its writer; the second says what holds it and waits.
- A new conversation whose first message quotes a placeholder from somewhere else, such as pasted output, now uploads;
  only its first message is checked against its branch name.
- A push run right after a push that stopped early no longer fails with "privacy check pending (store_busy)".
- `agit commit --milestone` (and `--tag`) given while the turn is still running goes on that turn when it is saved; with
  nothing new to save, it names the last saved turn.
- A tag on a session turn reaches the Hub on the commit the turn was published as; `agit push` names any local tag it
  doesn't upload and why, and doesn't report "already up to date" while one is missing.
- When an import prints the command to run next and that import also moves the session's claim from another branch, the
  printed command now works as printed: the report says the claim moves, and the command carries the -y that confirms
  it.

## [0.2.41] - 2026-11-06

### Added

- When a session that `agit run`, `agit resume` or `agit fork` started ends, agit says in one line how many turns it
  saved and whether they were uploaded, are still uploading, or why not.
- The Hub can ask a computer to make and bind a new ~/AgentGit/<organization>/<repository> folder for a project.
- `agit rc start --here` can add the folder as a project after one question, so sessions started there from then on
  upload to a new private repository; `agit off` now also stops the uploads of a folder added on the website.

### Changed

- When an installed agit waits for the running remote-control daemon to hand over, `agit rc local status` and the menu
  bar say what it waits for.
- Automatic upload into a public repository can be turned on with `--yes`, by an agent too; the menu bar then tells you
  once that it was turned on and where to turn it off.
- Commands no longer stop to ask about an update. They show a one-line hint and run; `agit upgrade` installs it.
- Commands you granted with `agit rc grant` stop applying to operators once an operator approves a file change in that
  workspace; granting again restores them. In a workspace bound at or above the home folder, only the owner can preview
  files, see local sessions or send messages, and its approvals go to the owner.
- `agit push` protects each record and commit message it adds the same way saving a turn does today, leaves what the Hub
  already holds as it is, and `agit status` shows an upload the privacy check is holding back.
- When a command makes a repository public, agit says that a public repository may appear on the Hub homepage and in
  recommendations.

### Fixed

- In the menu bar, an action that fails stays in view with agit's full reason until you dismiss it or it succeeds, and
  is notified when the menu is closed; Add a folder stays open until the folder is on.
- Recording a session no longer fails on filesystems that refuse to rename an open file, such as BeeGFS.
- Claude Code no longer loses a prompt when agit can't get an answer on whether another process writes the conversation.
- A saved turn's commit message now uses the same protected text as the turn itself, for its subject and for its
  milestone line.
- The background service no longer exits without a trace when a client or child process goes away while it's writing to
  them.
- When agit cannot read a repository's branch list from the Hub, the error now says why (for example that it did not
  answer in time) instead of reporting broken publication history, and a slow answer is retried rather than stopping the
  upload.
- `agit distill` says before asking, not after, when a memory file stays on this computer because your allowlist doesn't
  include it, and both it and `agit memory status` name the command that allows it.
- A merge or runtime switch of a Codex session whose next turn has just started is no longer refused; a record type agit
  doesn't know is named, and the refusal says the turns stay saved on this computer and gives an upgrade step that works
  in this install.
- `agit resume` checks the Hub for the branch before continuing it, so a teammate's pushed turns are included and a
  branch that exists only on the Hub can be continued if you can write to it; when the Hub can't be reached it says so
  and continues from this computer's copy.
- A new conversation whose first save waited for a slow privacy check is saved normally on the next try.
- Values that look like credentials stay replaced in more places a session is published from, and source references such
  as file paths and variable names are no longer replaced by mistake.
- `agit distill` no longer puts back a file the session never changed over a teammate's newer version; changes to
  different lines are merged, and changes to the same line stop with both versions shown until you choose
  (`--take-mine`).

## [0.2.40] - 2026-11-05

### Added

- `agit repair placeholders` puts back values an older agit replaced in a repository's files; real credentials stay
  replaced, and nothing is published until you push.
- `agit setup --remove-skill --runtime <tool>` turns agit's Skill off for one tool and keeps it off through later setups
  and upgrades.

### Changed

- `agit setup` now sets up only the AI tools installed on this computer; run it again after installing another, or use
  `agit setup --all` for every tool agit supports.
- `agit upgrade` now sets up an AI tool installed since the last setup (agit's Skill, capture hooks and MCP) and says
  which; that tool's conversations are then saved on this computer, and uploaded only in folders set to upload.
- Refusals to edit or merge an encrypted publication now point only to continuing the session natively, and a fork of
  an encrypted snapshot can be continued on this computer but isn't published.

### Fixed

- A session that includes another session published encrypted is no longer published as a plain session; such a push is
  refused and nothing is uploaded.
- A push that is refused or fails its destination checks no longer contacts the Hub to sync the privacy dictionary.
- A long session with many records no longer fails its first upload with "local Git inspection exceeds its output limit".
- When a repository `agit push` meant to create already exists by the time it asks, the push stops and asks again instead
  of continuing into it.
- The background service no longer rewrites its session catalog every few seconds while idle, which wore the disk and
  used CPU on computers with many sessions.

## [0.2.39] - 2026-11-04

### Changed

- `agit rc start` warns when AGIT_HUB_URL names another Hub than the one you signed in to, and turns Cloud access on only
  for a Hub you're signed in to.

### Fixed

- `agit merge`, `agit resume` and switching runtimes no longer refuse Claude Code 2.1.311 to 2.1.314 sessions over the
  records it writes after a turn and at /exit. A record of a type agit does not know yet is refused by its type, with
  `agit upgrade` as the fix.
- The menu bar turns on uploads for a folder with one confirmation, including into a public repository after a question
  that says it is public; a refusal shows on the folder you clicked, and times in the menu stay current.
- A computer with many Claude Code commands or Skills no longer shows offline in remote control; the command list it
  reports is shortened instead.
- Pushing many large sessions at once no longer fails because too many of them go to the Hub in one publication.
- Remote viewers no longer see a replay gap when nothing was missed. The background service now records each session
  subscription's sequence numbers in its local diagnostics log (no content, nothing uploaded).

## [0.2.38] - 2026-11-04

### Added

- After you push by hand in a folder that doesn't upload by itself, your agent may ask once whether new conversations
  there should upload automatically, naming where they would go and who can read them; `agit off` declines, and agit
  won't suggest it there again.
- The upload summary says how many records in the upload have parts the privacy check left out.

### Changed

- `agit scan` mentions low-confidence values only with `--low-confidence`, which now lists just those in what push would
  publish, grouped by shape, and `--json` carries `low_confidence` only then.
- `agit status` reads repositories and session checkouts several at a time, so it finishes several times faster on a
  computer with many sessions.
- When the website can't bind a project to a folder on this computer, it can now say why (signed in to another Hub or
  account, a repository that isn't private or isn't yours, no write access, or a timeout).

### Fixed

- A conversation with a record too large for the privacy check to divide keeps saving; that record's largest parts are
  left out of what agit saves and uploads, and agit says so.
- Reopening a conversation in the terminal after the page's reply has ended no longer says a web turn is still running.
- When the Hub turns a publication down because its preview ran out or the repository's settings changed, agit says so
  plainly, says nothing was updated, and previews again once where that helps.
- `agit privacy status` on a version of a branch that isn't published yet says so and names `agit push`, instead of
  suggesting you may lack access.
- The daemon recovers by itself when its installed agit is replaced while it runs.
- A remote-control message that your computer has taken is confirmed at once, and one its agent never confirms is
  reported as unconfirmed after a minute instead of leaving the sender with no answer.
- When `agit push` won't upload a conversation the folder keeps out, it says why (it ran before agit was on, you dropped
  it, or it's a delegated session) and, where one exists, names the `agit save` command that uploads it.

## [0.2.37] - 2026-11-04

### Added

- `agit rc start --here` waits until this computer is online on the Hub you signed in to and says which Hub it is online
  on if not; `agit rc status` shows each Hub's Cloud connection.
- `agit doctor` names the background service's log, which now records each turn's start and outcome, session shutdowns
  and the runtime's start and exit, by ids and timings only.

### Fixed

- An old conversation larger than 32 MiB uploads again; since 0.2.36 its first upload failed.
- A Claude Code conversation an older agit renamed "agit: unnamed" gets its own title back when you reopen it.
- A remote-control message sent while a Claude Code conversation in a terminal is busy is confirmed at once and starts
  the next turn, instead of waiting for the running turn to end and being reported as not sent.

## [0.2.36] - 2026-11-03

### Changed

- Published Codex sessions show their token usage and model.
- Published sessions carry the conversation itself; the runtime's setup and context records stay on this computer.
- OpenCode, Hermes and OpenClaw sessions show no user prompts until those runtimes mark typed messages.
- Cursor and WorkBuddy sessions show only the typed request.
- Published sessions show a tool result's text and images only.
- Repositories published by this version show as diverged in agit 0.2.35 and older; update agit to see their real
  status.

### Fixed

- A new session's branch name is made from the published request.
- A Codex conversation is saved by itself only after Codex has run AgentGit's hooks for it, as `agit on` says; until
  then `agit save SESSION-ID` uploads it.
- Watching a conversation from the Hub never saves or uploads a conversation you left out or a folder you turned off.
- `agit secrets review` says what kind each secret is and whether a detector found it or you added it.

## [0.2.35] - 2026-11-03

### Added

- `agit month draft <org> <YYYY-MM>` gathers what you asked and what came back in the sessions the
  Hub lists for your month in an organization into a private, protected bundle for your own agent
  to write a summary from. `agit month publish` puts the sections you wrote on the Hub as your own
  text after a secret scan and your confirmation at a terminal; it stays yours alone until you
  share the month on the website. `agit month clean` deletes the bundle. Against a Hub that doesn't
  name the days it keeps of each session, `agit month draft` says so and exits without reading
  anything.
- `agit remote` lists the Hubs that hold a repository (`-v` adds its address there, the account
  signed in, and the upstream), for the folder's repository by default.
- An agit that was never set up, for example because npm 12 blocked its install script, says so
  when you start or save a session. A terminal you answer lists what setup writes and asks once,
  default No; agents and scripts get one line a day telling them to ask you first. `agit doctor`
  reports the setup state and Claude Code's capture hooks.
- Remote Control can stop a live session from the web, and its owner can close it, which also stops
  this machine collecting its transcript.
- A page can follow a conversation on a device without starting it there, and a resume says
  whether it started the conversation's runtime.

### Changed

- Someone other than a conversation's owner can send to it again from the Hub once the device the
  conversation runs on has this version. Claude Code and OpenCode conversations accept their
  messages again, and the device refuses only work that would outlast their turn.
- Others can no longer send messages into a Codex conversation on this device; only the workspace
  owner can, until a later update.
- Only the workspace owner can run a conversation's runtime commands on the device.
- Importing a session starts one Git process per turn instead of about four.

### Fixed

- A Claude Code conversation shared between a terminal and the web has a single writer: reopening
  it in a terminal stops the web's headless process at its turn boundary instead of forking the
  conversation, and the web is told who holds it. A terminal opened while a web turn still runs
  takes the conversation over when that turn ends, and keeps holding what is typed there until
  then.
- `agit push` no longer fails when someone else pushes to the same repository while a publication
  is being checked. It previews the publication again once, and asks again if what it would
  publish changed.
- A conversation you drop or exclude while a long `agit push` is still running is no longer
  uploaded by a later group of branches or by `--keep-going` publishing branches one at a time.
- A conversation larger than the Hub accepts is no longer uploaded again after every turn.
  Automatic uploads leave it out until it shrinks or you push it yourself, and a push the Hub
  turned down before changing anything no longer says that some ref updates are unconfirmed.
- A conversation that builds on a session withdrawn on the Hub is no longer uploaded again after
  every turn; automatic uploads leave it out and say why once, and pushing it yourself publishes it
  once that session is restored.
- When the Hub refuses an upload, `agit save` and `agit` show the Hub's reason instead of Git's
  "failed to push some refs", and no longer suggest saving again when the same refusal would come
  back.
- `agit import` no longer imports nothing when you press Enter at its history choice: no option is
  preselected, Esc or q cancels, and a cancel says that nothing was imported.
- Every command `agit`, `agit new` and `agit resume` suggest runs as printed: the overview names
  `agit resume` instead of a command that doesn't exist, and a fork suggestion names the
  repository and a free branch name.
- `agit status` now says when a conversation's completed turns stay on this computer because
  automatic publication is off for them, and which setting keeps them there.
- The "last automatic upload failed" line after a turn is about that conversation only, and says
  when the next turn will try again.
- Resuming a conversation from before the folder was on, or from before agit covered its git
  worktree, now tells you it is not uploaded, where it would go and who can read it there, and how
  to upload it.
- Memory a conversation writes in a folder that uploads automatically is collected onto its
  branch; its memory baseline is recorded when the conversation starts, so its own edits are no
  longer counted as personal memory that was already there.
- Upgrading removes the old agit section that told agents to run `agit status` at every session
  start from your home folder's instruction files, also where an early setup doubled its markers;
  `agit doctor` and `agit` name any project that still has it, with
  `agit setup --remove-agents-md DIR`, which removes only that section.
- Signing out now also clears live sessions from Team activity for a device started with
  `agit rc start`, and says so when it can't.
- Team activity no longer keeps showing a terminal Claude Code session as "Needs approval" after
  you answer the approval in the terminal.
- Stopping a headless Claude Code turn with a turn-specific stop is now refused with a clear reason
  (fence_unsupported) instead of an unexplained error; a plain stop still stops the turn.
- When the device refuses a Cloud connection, its log now says which check refused it.
- A privacy publication no longer leaves the next push or status check reporting "privacy
  publication cache is being updated; retry" when another agit thread was starting a process at
  the moment the publication finished.
- After `agit memory distill`, the hint to publish main now names the repository
  (`agit push <owner/repo>@main`), so it works from any folder.
- `npx create-agit` no longer exits silently with status 13 when its check of the registry for a
  newer installer cannot finish; it installs its own version instead.
- Native task identities and binary content are kept when a session is captured, and delegated
  sessions get meaningful titles.

## [0.2.34] - 2026-11-01

### Added

- Team activity shows live status, including "Needs approval", for members' terminal Claude Code
  sessions in folders bound with `agit project bind`, on Hubs that support it.

### Fixed

- 0.2.33 could upload a conversation you had dropped with `agit project drop`: after `agit on --repo`
  moved its folder to another repository, or `agit project bind --rebind` bound the folder to one,
  `agit save --all` or `agit save --today` could upload it there. A dropped conversation now stays
  dropped.
- A conversation you dropped stays dropped when a subfolder of a folder that is on gets its own
  setting with `agit project bind`; in 0.2.33, `agit save --all` in the subfolder, or
  `agit save --today`, could upload it.
- A folder that is on now also saves and uploads the sessions in the git worktrees of its checkout,
  from when it covers each one; earlier sessions there wait for `agit save --all`, and `agit off` in
  a worktree stops only that worktree. A conversation in a git worktree that was already running
  when you upgraded isn't uploaded until you save it: `agit save` in that conversation, or
  `agit save --all` in the worktree.
- A failed first clone no longer leaves `agit status` and `agit doctor` refusing to run, and a
  leftover recovery note now says whether another agit command is still running or an earlier one
  stopped and `agit config --list` finishes it.
- A folder that uploads automatically now says at session start when agit is signed out, and the
  next turn shows why an automatic upload failed. It no longer promises uploads that never happen.
- A failing agit hook no longer exits with code 2, which Claude Code treats as an instruction to the
  agent.
- `agit push` finds a commit's privacy envelope whatever type of entry it is stored as, including a
  submodule entry with an older system Git, and refuses such a commit like any other unpublished
  encrypted snapshot.
- `agit setup --hooks` adds its hooks to a runtime's existing hook file (Claude Code's
  `settings.json`, Codex's and WorkBuddy's hooks files) without reordering keys or reflowing the
  rest of the file, and leaves a file it cannot edit that way, such as one with a key given twice,
  unchanged.
- A device that has just connected to the Hub for the first time no longer drops its connections a
  few seconds later, so Remote Control requests sent at that moment no longer fail with "executor
  attachment changed".
- A Claude Code session driven by an SDK host saves each turn at its own Stop, including the
  session's last turn, instead of one turn late and never the last one.

### Changed

- A first publication to a new repository, and every publication preview, start a fixed number of
  Git processes instead of one or more per commit.

## [0.2.33] - 2026-10-31

### Added

- `agit on` turns a folder on once so that every Claude Code or Codex conversation there saves and
  uploads itself after each turn, to a private repository it names; `agit off` stops that without
  deleting anything, and plain `agit` shows the folder's state and next step.
- `agit save` uploads now with no other argument: this conversation inside an agent, what the
  folder still owes the Hub in a terminal, `--all` for earlier sessions and `--today` for the day's
  sessions everywhere. `--all` and `--today` never upload a conversation you stopped uploading with
  `agit project drop`; only naming it does. A first save turns the folder on privately, and the
  agent then asks once whether to keep uploading there.
- `agit push --to owner/repo` publishes to another repository on the same Hub that your account can
  write to; that repository is checked right before anything is written, and the checkout keeps its
  own repository as the default destination.
- `agit push` publishes a session whose new history is more than one publication can carry in
  parts: one confirmation, then each part is prepared, checked and sent in turn with its own
  privacy preview file, never past the commits you confirmed. Dropping the project or turning
  automatic upload off stops the parts not yet sent, and rerunning after an interruption continues
  from what the Hub holds.
- Team activity shows live status, including "Needs approval", for members' Claude Code sessions in
  Workspace projects, also ones started in a terminal.
- The owner of a device can share one of its own conversations with a workspace over remote control
  without starting it, and the workspace then lists it as one of its sessions for its members.
- Each GitHub release now includes the corresponding source code of the GPL- and LGPL-licensed parts
  of the bundled Git runtime, including the MSYS2 and Git for Windows components that the Windows
  build's MinGit carries, with their checksums listed in the release notes and in
  `SOURCES-SHA256SUMS`.

### Changed

- One user turn can now land as several commits under the same turn number. When the agent keeps
  working after it replies, without a new prompt, a further commit is added on top when it
  finishes, and history is never amended; `agit import`, project capture, or an `agit commit` that
  collects several finished turns lands one commit per turn. `agit log` and the Timeline show the
  first commit of a turn as `#n` and later ones as `+n` (`"continuation": true` in `--json`);
  `<ref>#n` resolves to the turn's last commit and `#n.k` numbers the events of all its commits, so
  existing addresses keep their meaning.
- Publishing a session sends its branch and its new tags in one upload when they fit, saving a
  privacy preview, a confirmation and an upload on each push.
- A turn you interrupt with Esc in Claude Code is no longer uploaded right away, whether you stopped
  it before any output or the transcript records the interruption. It is saved and uploaded once a
  later turn in that session completes, or when you run `agit push` yourself.

### Fixed

- A Claude Code turn that keeps working after it replies, woken by a background task's
  notification, a monitor event or a finished workflow, is saved and pushed when it next finishes,
  as a further commit of the same turn, instead of staying local until the next prompt.
- Codex turns, including goal turns that continue without a new prompt, are saved when they finish
  instead of one turn late, and no turn leaves a later commit holding only its completion record.
- Saving a Claude Code or Codex session stops at the last point the runtime recorded as complete,
  so a turn still in progress is never committed in part; an interrupted turn, an API error, a
  refusal, "No response requested." or a manual `/compact` is still saved.
- Session metadata larger than a Hub accepts is refused when it is saved, with the size and the
  limit, instead of being saved and refused only at push, and `agit activity` and
  `agit search --local` no longer fail or skip versions whose metadata is between 64 KiB and that
  limit.
- `agit push` no longer fails every time on a branch whose session was withdrawn on the Hub: it
  names the refused branches, keeps them in the checkout and leaves them out of later pushes, and
  naming such a branch asks the Hub again once the withdrawal is restored.
- When agit's local record of what a checkout already published does not match its commits,
  `agit push` now names the repository and branch, says that nothing was sent and local commits are
  kept, and asks you to report the problem with `agit status` output instead of suggesting recovery
  steps that don't exist.
- Enrolling a folder for publication from the Hub no longer turns a repository's automatic upload
  back on when it was turned off on the device; the device refuses the enrolment with that reason,
  and only `agit config --repo OWNER/NAME push.auto true` or the menu bar app lifts it.
- The Remote Control daemon no longer keeps retrying uploads on a timer while a session's privacy
  check is pending or automatic upload is off for its destination, and no longer re-reads unchanged
  transcripts every few seconds.
- A folder whose name maps to the same Claude Code project directory as another, such as
  `/work/a-b` and `/work/a/b`, no longer lists the other folder's conversations.
- `agit import` reads OpenCode sessions saved by OpenCode's desktop and web apps, and says plainly
  when an OpenCode session uses a format agit cannot read yet.
- `agit setup` no longer moves approved Codex hooks to another copy of agit, and setup,
  `agit doctor` and `agit project status` now say when Codex has not approved AgentGit's hooks, so
  nothing is being saved.
- `agit on` no longer says every Codex turn saves while Codex holds AgentGit's hooks for review; it
  says nothing is saved yet, which hooks are held, and how to approve them.
- Codex side chats are no longer picked up, announced as uploaded, or captured by a connected
  project.
- `agit log` says when its entry limit hid earlier entries ("… N earlier of M entries not shown"),
  and `agit --json log` reports `total` and `truncated`.
- `agit search --local` finds saved turns of ordinary-sized sessions instead of stopping at its work
  budget, and an incomplete result now says what was skipped and what to do.

## [0.2.32] - 2026-10-31

### Changed

- `agit push` splits a publication larger than the Hub's published per-request limits into
  fast-forward requests within them instead of being refused, and a single commit too large for any
  request is refused before anything is sent, naming the setting that raises the limit.
- Session-start hooks no longer add anything to an agent's context; in a folder that uploads, the
  person is still told where each conversation goes at every start, resume included.
- The Git bundled in the Linux packages uses curl 8.23.0, brotli 1.2.1 and musl 1.2.6-r3.

### Fixed

- `agit setup --mcp` registers the agit MCP server with Claude Code at user scope, so its tools work
  in every folder, and `agit doctor` names any folder where a different local entry still overrides
  it.
- `agit pr create --branch <owner>/<repo>@<branch>` names the pull request's source, so a pull
  request can be opened from a cloned repository without a session.
- `agit project bind --auto-upload` now says plainly that automatic upload is turned on for this
  computer only, not for teammates or the Hub.
- A remote-control turn that stops without anyone choosing it, such as a failed turn or a Claude
  Code harness that exits mid-turn, is now saved as a turn of its own instead of waiting for the
  next completed turn. A turn you interrupt still stays out of automatic upload until a later turn
  completes. A conversation's last turn is also no longer queued for archiving forever behind the
  records Claude Code writes after a reply.
- Workspace conversation titles and history no longer stay "privacy check pending" while the privacy
  check is only busy; a withheld title now says why, and a conversation is never named by the
  pending marker.
- `agit push` sends a tag added to an already published session branch instead of reporting the
  branch up to date.
- A session converted from another runtime now opens in Codex when Codex uses a provider other than
  `openai`.
- `agit setup` and the installer no longer write an AGENTS.md section into whatever folder they run
  in. `--agents-md` writes it only at a Git repository's top level and says where.
- A skill install removes an agit section left in a home-level instruction file (`~/AGENTS.md`,
  `~/CLAUDE.md`, the Codex and OpenCode ones), which reached every conversation, and `agit doctor`
  reports one.

## [0.2.31] - 2026-10-30

This version was tagged but not published: its Linux builds could not download the packages the
bundled Git pinned, which Alpine had replaced. Every change listed for it ships in 0.2.32.

## [0.2.30] - 2026-10-30

### Added

- `agit project list` shows every folder this machine saves conversations for: whether it uploads
  automatically and where, its last upload result, its newest Claude Code and Codex conversations,
  and, as blocked, a folder whose repository has `push.auto` off on this machine; `--json` gives the
  same offline for tools that poll it.
- With statistics on, agit reports once per installation when it first uploads a session and when it
  first shares one, carrying no content, path, repository, session or link.

### Changed

- The agit skill tells agents to follow an error's hint instead of guessing, how to take an upload
  down, and how to invite someone or search the team's past work.
- `agit push` of a session that was published before sends only the turns added since, so each push
  costs what it adds rather than the whole session, and a plain session longer than one publication
  keeps publishing its new turns.
- Publishing to a public organization repository you write only as a member of the organization,
  through its base permission, asks you first, at the terminal or with `--yes` for that one push; an
  automatic upload never publishes there, and the Hub refuses such a push unless agit says you
  confirmed it.
- The notice that a conversation is uploaded automatically names every member of the organization as
  readers of a private organization repository while its base permission lets members read, instead
  of only its owners and granted teams.
- Before `agit setup` rewrites a hook file, it keeps the file as it was before agit first changed it
  beside it, readable only by you, with `.agit-backup` added to the name.
- New placeholders are short and say what they replaced, for example `‹secret:github-
  pat:k3m7x2q6w4ab›`, instead of a long `{{AGIT_SECRET_V2:…}}` token; existing tokens keep restoring
  on every device of the account, and a value replaced before under the old spelling gets the new
  one the next time it is captured.
- Continuing a published encrypted session publishes only the turns it adds and keeps the public
  session identity it was accepted under.
- The upload notice and the `agit import` refusal for organization repositories name the people
  granted access and the base permission instead of teams, which the Hub no longer has.
- `agit push` of many branches publishes them together, paying the publication's fixed cost once,
  and leaves branches the Hub already holds out of the upload; `--keep-going` publishes a failing
  group one branch at a time so only the failing branch is held back.

### Fixed

- `agit push` publishes a session that another account registered first in the repository, or that a
  different local choice recorded, under the configuration the Hub holds for it instead of stopping
  with "session publication mode is already registered and differs from the local choice". A session
  first published on another Hub publishes as a new plain session of your account only when this
  device is signed in there as the account that created it, or when the session names no Hub;
  otherwise `agit push` stops and names it instead of stopping with "session creator belongs to
  another Hub".
- A session started with agit run in a folder that's switched off, or with upload off, now saves its
  turns locally instead of losing them.
- A fork made with agit fork or run records where it came from, so its Hub page shows 'forked from';
  a merge's opening names the merged branch, or says 'another conversation' when that one isn't
  public.
- In a Codex app conversation each prompt is read as the request you typed, without the in-app
  browser state and the request marker the app wraps around it, so its titles and search results
  show what you asked.
- `agit search "stall author:sim-lin since:2026-10-24"` no longer finds nothing: `author:`, `since:`
  and `before:` written into the query are read as `--author`, `--since` and `--before`, with a hint
  naming the flag; in a batch they are refused with the flag to pass instead.
- Answering a Claude Code permission request from AgentGit works again where Claude Code's built-in
  security default is in force (Team and Enterprise organizations, computers with managed settings):
  `agit setup` adds a PermissionRequest hook that offers the request to shared control while the
  terminal's dialog stays up, and answers only with what a person chose there; run `agit setup`
  again to add it.
- Installing @einsia/agent-git as a project dependency or with `npm install --prefix` no longer runs
  `agit setup` or records an installation; only a global install (`npm install -g`) or `npx create-
  agit` sets up hooks, Skills and MCP, so deleting a local install can no longer break every agit
  hook on the machine.
- `agit rc start`, `stop` and `status` work again after an agit home is copied, restored or moved: a
  leftover control socket that nothing can still hold is removed with a one-line note when the
  daemon that published it is gone, and a leftover elsewhere (such as on macOS) still needs `agit rc
  local recover --confirm-stopped`.
- Old publication records are reclaimed however long a session grows, and a reclaim that cannot run
  is reported instead of stopping silently.
- The last Claude Code turn before /exit is now saved with its reply and uploaded, instead of
  staying unsettled.
- Publishing a new branch that reuses already-published history from another branch no longer
  mistakes an inherited identity boundary with omitted ancestors for an inconsistent publication
  mapping.
- The device daemon lists and watches a runtime's local conversations whether or not its program is
  on the daemon's PATH, as when a desktop app or service manager starts it, and installs Claude
  Code's native integration when Claude Code is found in its installers' directories.
- `agit push` no longer fails every session with "unsupported session candidate cache" or "invalid
  session candidate cache" when another agit version on the same computer wrote that cache; agit
  says once that it is rebuilding it and keeps the sessions' published identities. An agit older
  than 0.2.26 still refuses the cache a newer one wrote, so update every agit copy on a computer,
  including any `npx` copy.
- `agit status` and `agit doctor` answer sooner on a computer with many conversations, reading each
  repository's checkouts at once and ruling out branches kept from the storage migration in one
  batched read.
- `agit push` on a branch that moved on the Hub prints why on its own line and the next commands
  (`agit pull` when only the Hub moved, `agit fork … -b …` or `agit fetch` + `agit merge … --into …`
  when both sides did), and a failed automatic push after `agit commit` shows the same reason.
- `agit init` for a repository that already exists binds the current directory to it instead of
  refusing with "already exists", keeping the repository's settings and naming the options that
  apply only to a new repository.
- `agit doctor` no longer reports a large session that agit commits and pushes as unreadable
  storage; it reads committed sessions with agit's own limits and lists one past them as not
  inspected.
- `agit push` publishes a session first published on another Hub as yours only when this device is
  signed in to that Hub as the account that created it, and the published session no longer names
  that Hub's registration or accounts. Otherwise it stops and names the session, instead of
  registering someone else's session as yours.
- Publishing a fork of a private session into another repository no longer names the private source
  in the fork's commit messages or milestone.
- agit run and resume continue a branch at its published tip when the local copy is behind, and stop
  with the exact next step when it has diverged, instead of silently continuing an older head.
- An agit rc daemon no longer spends a CPU core for about a minute on every archive-recovery retry:
  a settlement that proved a session's committed history records that proof, and moving a claim to
  another branch forgets the proof it held for the old one.
- A checkout bound to an old Hub address explains how to keep its work, with a delivery command that
  keeps the recorded source Hub, and that freeing its local name is only for disposable work and
  needs `--local`; `agit doctor` reports the mismatch without contacting the Hub.

### Security

- `agit push --to` no longer names a private fork source (repository, branch, commit or fork origin)
  when it publishes a fork into another repository that can't already see that source.

### Known issues

- `agit push` can report a branch as up to date without sending a tag added to its
  already-published history since the last push; `agit push --verify` sends the tag, and so does
  the next push that publishes new turns on that branch.

## [0.2.29] - 2026-10-29

### Added

- `agit doctor` names the skill folders Claude Code or Codex would skip without saying so, for
  a conversation started where it runs: a folder holding skills one level too deep (Claude Code
  reads only `<skills>/<name>/SKILL.md`), a `SKILL.md` without the frontmatter Codex requires,
  and a link whose target is gone. Each row prints the fix, such as one `mv` per nested skill,
  and moves nothing.
- `agit setup --hooks --installed-only` repairs the agit hooks your agent runtimes already have
  but cannot start, such as one written by an older agit whose path the shell split at a space,
  and installs none. Only the command text of those hooks changes; the rest of the file stays as
  it was, and the original is kept beside it with `.agit-backup` added to its name. With `--json`
  it reports `hook_files_repaired`.

- `agit push` reports preparation, preview, receipt and upload progress with snapshot counts and
  elapsed time. Redirected output retains periodic progress on stderr; quiet and JSON output
  omit these routine notices. Each upload batch reports completion only after acknowledgement.
- `agit push <owner/repo@branch> --to-hub <url>` explicitly migrates a repository publication to
  another Hub using credentials already saved for that Hub. The destination is reviewed before
  the local immutable Hub binding and `origin` are updated; ordinary pushes remain bound to the
  selected Hub. Delivery verifies the source session creator, maps only the signed-in source
  account, and leaves collaborator commits unattributed. Sessions with another or an unverified
  creator are refused until the Hub supports creatorless registration.
- In a folder that uploads its conversations automatically, each new conversation in Claude Code
  or Codex now opens with one line saying where it will be uploaded and who can see that
  repository, before its first turn is uploaded, and how to stop it: say so, or run
  `agit project drop`.

### Fixed

- Uploading the current conversation from an agent takes two commands instead of six to ten.
  The agit skill no longer has the agent run status, sign-in and dry-run checks before
  pushing: push checks the sign-in and prints the destination, count and visibility itself. The
  agent pushes only when the person asked to upload, and runs a dry-run preview only when they
  ask to see one first.
- A conversation holding a record or memory file larger than one privacy check takes is no longer
  held back. The record is checked in parts with the text around each part, images and other
  base64 media pass through unchanged, and a single string too large to check is published as
  "[content omitted: too large for the privacy check]" rather than unchecked. A record that still
  cannot be divided, because it holds only short strings, is named with its size in the
  settlement output and in `agit status`, with the command that stops recording the
  conversation, instead of a "pending" that never ends.
- The privacy publication store kept for each published repository no longer grows without
  bound: a push compacts it once it holds many packs or loose objects and its disk has about the
  store's size free, keeping every object it holds. `agit doctor` reports a store over 1 GiB with
  its size and the command that compacts it by hand.
- Hooks installed by `agit setup` run when agit's path contains a space, such as an app's
  `Application Support` folder or a Windows user folder with a space in its name. The hook
  command wrote that path unquoted, so the shell split it, the hook failed on every turn, and no
  conversation was saved or uploaded automatically. Running `agit setup` again rewrites such a
  hook in place.
- `agit push` works again in a checkout that has fetched the version tags of more turns than one
  publication lists (about 4,000, counted across every session of the repository); it failed
  with "audit publication reference limit exceeded" even for a short new session. Version tags
  no longer count against that limit; other branches and tags still do.
- An encrypted session whose publication envelope is larger than 1 MiB can be unlocked and
  resumed again: `agit privacy unlock` and resume read the committed envelope up to the size an
  envelope may have, instead of failing with "local Git inspection exceeds its output limit", and
  a large encrypted session's original is no longer refused by the local recovery cache.
  Delivering such a session to another Hub, or pushing it to a new repository, is refused for its
  encryption, as for any encrypted session, instead of for the envelope's size.
- After a hard stop (power loss, a killed machine) left empty Git object files in a local
  repository, `agit doctor` names the repository and its damaged branches, and `fetch`, `pull`,
  `run` and commits stop with the repair to run instead of Git's own errors.
  `agit doctor --repair-objects OWNER/REPO` restores what the Hub has, lists the commits only this
  machine had, and with confirmation sets an unreadable branch aside.
- On macOS, `agit push` (and `--to-hub` above all) no longer hangs for good after the Hub
  answers. A privacy worker started at the same moment as a Git command could inherit that
  command's output pipe and keep it open, so the push waited for the end of output while the
  worker waited for the push. The worker now closes every descriptor it inherited before it
  starts, and does not start where it cannot list them (no `close_range`, `/dev/fd` or
  `/proc`), so a publication there stops with a privacy error instead of hanging.
- Push reuses committed history facts within the same frozen selection, reads retained encrypted
  envelopes once per reconstruction, and prepares the JSON and expanded terminal preview together.
  Metadata-only projection changes retain validated private session content while checking changed
  metadata; deserialized content still receives complete validation.
- A Slack token whose numeric groups are longer than thirteen digits is now replaced in what agit
  publishes; such a token was published unchanged. So is the value curl echoes in "Enter host
  password for user '…'", which is the credential itself when a command passes it to `curl -u`
  without a colon, and the user part git prints in "Password for 'https://…@host'" when a command
  put a credential there.
- `agit setup` configures Hermes 0.22's hooks and MCP server again, and a conversation can be
  restored into Hermes 0.22. Hermes 0.22 starts through a shell launcher whose Python needs the
  launcher's own bootstrap, so looking for a `python` beside the `hermes` command found none and
  setup stopped at "install Hermes before configuring its integration". agit now asks the launcher
  for its runtime command; an install that names its Python directly is unchanged.
- `agit activity` and its MCP tool read a Hub that answers activity contract 3 as well as one
  that answers 4, both when they query the Hub and on a local clone, where 0.2.28 refused the
  reply. A Hub that answers 3 selects by save time: the result then says
  `applied_filters.select_by: saved`, and each message's `sent_at` is its save time with
  `sent_basis: saved`. agit names the contract it reads in the `x-agentgit-activity-schema`
  request header, and its requests to the Hub name the release in their User-Agent
  (`agit/<version>`).
- A credential is replaced to its end in what agit publishes. A GitLab token longer than the
  20 characters its rule names had only its start replaced, and the rest of the token was
  published after the placeholder; every rule that names a token by its length now replaces the
  whole token. Model provider keys in the `sk-` format, `hrk_` and `hsk_` tokens, and a JSON Web
  Token written after a vendor prefix (`hsk_eyJ…`) are now replaced wherever they appear, also
  without a credential name in front. Sessions already published keep what they published:
  rotate a token that appears in one.
- Remote Control never asks anyone to approve a command they cannot see. An approval request whose
  privacy check had not finished went out as "[privacy check pending: text withheld]" and could be
  allowed blind. The device now holds the request, says that an approval is waiting for its
  privacy check, and shows the card with its command once the check passes; until then the
  approval cannot be answered. A request whose check cannot pass, such as content too large to
  check, is shown without its command and can only be denied; in a session running in a terminal
  on the computer, it is answered there.
- agit no longer renames conversations in Claude Code: a new conversation keeps the title Claude
  Code gives it. A conversation an earlier agit renamed "agit: unnamed" or
  "agit owner/repo@branch" gets Claude Code's own title back when it is opened again, or its first
  prompt when it never had a title of its own.
- On Windows, attaching to the device daemon no longer fails with "All pipe instances are busy"
  (os error 231) when another client connects at the same moment, which closed the bridge
  before it answered; it waits for the daemon's next pipe instance instead.
- `agit commit <org>/wiki@main -m … -- <paths>` commits the named pages also when the wiki's
  `main` was started on the Hub or written with plain Git and carries no AgentGit metadata.
- Codex keeps trusting AgentGit's hooks after an update. `agit setup` wrote the file an installer's
  link resolves to into the hook commands, and Codex trusts a hook by its exact command, so every
  update made Codex ask to review the hooks again, and they saved nothing until someone did.
  Hooks and MCP entries now run agit by its link or launcher script on `PATH`, and `agit setup`
  leaves a hook that already runs this agit exactly as it is, also when it runs for another
  AGIT_HOME. The first `agit setup` after updating rewrites them once.
- AgentGit no longer tells an agent to push a saved session on its own. The context a saved
  session starts with, `agit status`, the skill and the `AGENTS.md` section now say to publish
  only when the person asks; the session context and status also leave saving new turns to the
  person's request. A forked Codex session had pushed mid-turn because the context told it to.
- `agit rc` rebuilds its index of this device's conversations when that index cannot be opened,
  for example after a power loss damages one of its files, instead of failing every conversation
  list with an internal error. The index holds only what `agit rc` read from Claude Code, Codex
  and OpenCode, so nothing is lost: the damaged copy is moved aside next to it for inspection and
  removed after seven days. It rebuilds once per start; if the index still cannot be opened, the
  list reports `catalog_unreadable` and asks the device owner to restart `agit rc`, and the
  daemon log names the underlying error.
- A repository whose secret dictionary from before account sync holds more than 16,384 values
  saves and uploads again. Every privacy check there stopped as incomplete, so turns stayed
  unsettled and pushes were refused, without saying why. The dictionary is now read whole and
  carried into your account's dictionary once. If it still cannot be read, the pending check is
  reported as `legacy_rules`.
- A yes/no question takes its answer when you press Enter, so a quick `y` followed by Enter no
  longer answers the next question too. An `agit run` that you cancel no longer leaves an empty
  fork branch behind.
- An agent can no longer publish to a public repository on its own: without a terminal, publishing
  there needs an explicit `--yes` that the person has confirmed.
- Turning on automatic upload for a public repository now needs a person at a terminal, whichever
  way it is switched on: `agit config push.auto`, `agit init --auto-push`,
  `agit project bind --auto-upload` or the clone menu. `--yes` and agents cannot confirm it, and
  cancelling the menu changes nothing.
- Disconnecting a folder with `agit project unbind` now turns automatic push back off for the
  repository if binding it had turned it on, and binding says plainly that automatic push applies
  to the whole repository.

## [0.2.28] - 2026-10-27

### Added

- `agit project drop [OWNER/REPO@BRANCH]` stops uploading a conversation from this computer; its
  local history stays. Without a target it drops the conversation the command runs in. The
  conversation stops recording into that branch and the project no longer captures it.

### Changed

- `agit activity` and its MCP tool select messages by when they were sent: the time Claude Code,
  Codex or OpenCode recorded for each message, or its save time when none was recorded. A
  conversation saved or pushed later still counts on the day it happened. Each message carries
  `sent_at` and `sent_basis`. Encrypted publications are still selected by acceptance time.
  `agit activity` and the MCP tool do not work against agent-git.com in this release, on a local
  clone either: this release needs activity contract 4, and the Hub answers it with contract 3.
  A later agit release reads both.
- The privacy check no longer counts its own start-up against a command's time budget, and
  saving, importing, pushing and collecting memory allow seconds instead of a fraction of a
  second, so large transcripts and large allowlists are protected instead of passed through.
- `agit scan --secrets` lists only high-confidence findings (provider signatures, private keys,
  credential assignments), each with the record id that `agit secrets allow` accepts, and states
  how many there are. Entropy-only values such as runtime ids, hashes and encrypted carriers are
  counted, and listed with `--low-confidence`. The report no longer suggests rewriting Agent
  history or repeating a push with `--allow-secrets`.

### Fixed

- `agit import -y` run outside the session's folder could add that folder's `CLAUDE.md`,
  `AGENTS.md` and skills to the repository it created, on the `main` line a push publishes. A
  repository created by an import now takes them from the session's own folder.

- `npx create-agit` served from npx's cache, or a `create-agit` installed globally, no longer
  installs an old agit: when the registry publishes a newer installer, the old one hands the
  install to it once, and when the registry does not answer it installs its own version. The
  install instructions and the upgrade fallback name `npx -y create-agit@latest`.
- A project never publishes a conversation that started outside its folder, or a session
  excluded from it, whatever its claim says: automatic upload and `agit push` both refuse the
  branch. Conversations captured that way by an earlier version are dropped on the first run, and
  agit says once which ones; their local history stays. A copy already on the Hub is not removed.
- A PEM private key is replaced as one block from BEGIN through END in memory files and in
  remote-control text, instead of only on its BEGIN line. A memory file whose key block lacks its
  BEGIN or END line, or that is not UTF-8 text, stays off the branch with a warning.
- Content the privacy check did not protect in full is never committed, published or shown to a
  remote-control viewer. Settlement commits the turns that passed and leaves the rest for the
  next settlement; `agit status` shows a session whose settlement waits, and why. A viewer sees
  "[privacy check pending: …]" in place of withheld text, and the text arrives once it passes.
- Remote-control session lists protect each session's opening prompt and title before they leave
  the device.
- A value registered with `agit secrets add` or `agit secrets block add` before `agit login` is
  replaced from the first command after signing in; until now it was not replaced until agit had
  synchronized your rules in the background. Registered values also stay replaced after you sign
  out or switch to another account or Hub on the same computer.
- AgentGit's own credentials (personal access, device and controller tokens, and sign-in codes)
  and bearer tokens written outside a `curl` command are replaced before a conversation is
  published, in addition to the access and refresh tokens already covered.
- When rules this computer registered under another account or Hub cannot be read, for example
  because that account's key is no longer cached here, agit keeps new content back and says which
  account to sign in to once (in settlement, `agit status` and `agit doctor`), instead of a generic
  "privacy check pending".
- `agit push` works again in a repository whose published sessions together pass the history
  bound of one publication. Deciding which commits the Hub already holds walked every published
  branch under that bound, so no session in such a repository could push.
- `agit clone` of a Hub repository that has no history yet lays down `main` as a file line, so
  `agit commit … -m` and `agit push` work in it, as the Hub's empty-wiki prompt describes.
- `agit secrets` mutations print nothing under `--quiet`, `add` and `block add` print the
  `registered NAME (ID)` line scripts read the new id from, and `status` prints a status line
  instead of the worker's raw result.
- `agit import` reports a Git that cannot run as a failure (exit 1) instead of calling the
  destination branch an invalid Git ref (exit 2).

## [0.2.27] - 2026-10-25

### Added

- `agit import … --milestone <text> [--milestone-turn <n>]` marks a milestone on turn `#n` of the
  imported branch, or on its last settled turn. A turn the import does not land is refused before
  anything is written.

### Changed

- Captured content has only high-confidence credentials replaced with reversible placeholders:
  provider tokens, private keys, credential assignments and values you explicitly block. Runtime
  identifiers, hashes, beta-feature lists, document tokens, code and other values whose only
  evidence is high entropy or a key-like name stay unchanged; run
  `agit config privacy.entropy replace` to replace them too. PostHog personal API keys are now
  recognized. Protocol identifiers such as Codex
  `call_id`, Claude Code tool-use IDs, thinking signatures and encrypted reasoning are never
  rewritten, so a resumed session keeps its call pairing and verified reasoning.

- Hub requests no longer fail after a fixed 30 seconds. agit bounds connecting to the Hub and each
  wait for data once connected, so a large upload or a busy Hub keeps going while data moves. A Hub
  that sends nothing for 150 seconds is reported as stalled, with a note that the request may still
  complete on the Hub. Upgrade and large-file transfers use the same bounds, and Hub JSON responses
  may be up to 64 MiB.

### Fixed

- Claude Code 2.1.3xx sessions save again. Those versions write request snapshots that repeat tool
  calls already answered, and agit read them as calls still running, so `agit commit` and the Stop
  hook reported "the current turn is still in flight" and saved nothing.
- Historical imports date each turn at its last recorded event instead of the import time.
  `agit show` reports when a turn was recorded and, if it was imported later, when it was imported.
- A failed `agit import` onto a new branch can be retried onto the same branch. The retry failed
  with `checkout refresh failed: refusing to overwrite index path "session/meta.json"`.
- `agit branch rm` on a claimed branch names the sessions it still claims and the
  `agit import … --independent` command that moves each claim. `agit doctor` reports a claimed
  branch that no longer exists instead of "local Git inspection did not complete".

## [0.2.26] - 2026-10-25

### Fixed

- The first push of a newly imported session no longer fails with `HTTP 422 provenance_rejected:
  refs/heads/publication: session/meta.json unreadable (cwd is empty)`. Since 0.2.25 publishes new
  sessions in plain mode, the Hub verifies every commit of the branch, including the commit that
  claims it, and that commit recorded an empty working directory. A branch claimed by 0.2.25 keeps
  its empty claim and still cannot be pushed; import the session into a new branch instead.

## [0.2.25] - 2026-10-25

### Added

- `agit privacy mode OWNER/REPO@BRANCH plain` selects unencrypted publication before a session's
  first remote registration. The choice follows the logical session across branches, keeps source
  history unchanged, and is separate from repository visibility. Use `encrypted` to opt into
  encryption before registration, or omit the mode to inspect the local choice and registered mode.

- `agit privacy policy tool [NAME] --omit true|false` controls calls and results together, with
  exact tool-name and runtime-qualified overrides. `--reset` removes a named override.
- `agit privacy policy attachment omit|metadata|original` controls recorded attachments in
  encrypted public views, privacy exports and shares. Plain push retains its existing path.

### Changed

- New sessions, first imports and independent forks default to plain publication again.
  0.2.24 created them with encrypted publication by default, which its release notes did not
  mention. Existing recorded and registered modes are preserved, including those of sessions
  0.2.24 created. Select `privacy mode OWNER/REPO@BRANCH encrypted` before registration to
  enable encryption.
- Privacy projections default to visible tools and original attachments, including when an
  existing policy omits the new fields. Configure omission explicitly to hide them. Captured
  file-tool content no longer requires a workspace/include match; explicit and mandatory file
  exclusions remain effective. Readable text remains sanitized; recorded binary carriers are retained.

### Fixed

- A privacy dictionary upload that the Hub, or a network policy in front of it, refuses is no
  longer sent again on every push, fetch, clone and login. agit retries it after a delay that
  starts at 10 minutes and grows to 6 hours, and keeps downloading dictionaries from other
  devices meanwhile. `agit push` and `agit doctor` say whether a network policy or the Hub
  refused it; `agit login` retries at once.

- Publication notices distinguish configured hiding, attachment handling, unsupported formats
  and unverifiable sources. Omitted tools preserve call/result classification instead of opening
  user turns, and activity summaries exclude both legacy and specific omission notices.

- Encrypted publication restores originals against a frozen dictionary across all batches and
  resolves evidence references independently of LOG order. Repeated events no longer fail with
  "hydration has ambiguous evidence provenance" after partial foreground hydration or forward
  evidence references; unavailable mappings stop preparation before upload.

- Raise the sanitized public projection budget to 64 MiB to match the private ciphertext budget,
  so large session displays can be published alongside their encrypted originals. The complete
  envelope remains bounded to 128 MiB.

- `agit push` previews session branches with a long plain history instead of failing with
  "plain preview exceeds its content budget", and prepares them substantially faster. The
  preview shows each distinct file version once and an append-only file as the bytes it adds.
- After a macOS restart, `agit rc status`, `agit upgrade` and daemon startup
  recognize the control socket a stopped daemon left behind and reclaim it,
  instead of asking for a manual `agit rc local recover`.
- Loading earlier messages of a remote conversation whose saved archive could not
  be applied no longer fails as an expired history snapshot; the page kept
  spinning on "Loading the latest conversation" while it retried.
- A conversation taken over from a registered runtime source keeps the name the
  device shows for it, instead of appearing as an untitled session in its
  project.
- Encrypted publication preserves Git authors, committers, timestamps, message encoding and
  commit messages. Existing published snapshots remain compatible when history continues.

## [0.2.24] - 2026-10-21

### Added

- `agit activity` and its MCP tool query a person's saved user and assistant messages
  over an explicit time range, with optional session selection and JSON pagination.
  Results are grouped by saved commit, oldest first, and a cursor keeps working while
  new work is saved. Existing clones are fetched once per query and read locally;
  other repositories are queried on the Hub without cloning. Encrypted publications
  use accepted account attribution and publication time, and incompleteness is
  reported for the whole query. `--assistant last` keeps every user message but only the
  final assistant reply to each. Requires a Hub supporting activity schema version 3.

### Fixed

- The bundled Linux Git runtime uses the current Alpine builds of expat, c-ares
  and the CA bundle; the previous builds are no longer published, so release
  builds could not assemble the runtime.
- Remote control opens Codex conversations that have been idle long enough for
  Codex to compress their transcript, and conversations Codex continued in a
  new transcript segment. Such conversations stayed on "Restoring
  conversation", returned history errors, and never archived their turns.
  Restoring a compressed transcript no longer pauses other conversations on the
  machine.
- A failed or timed-out remote resume releases the conversation once its
  processes have exited, instead of keeping it busy until the daemon restarts.
  A capture binding that cannot be resolved no longer blocks control: the
  conversation runs, and its turns archive once the binding resolves again.
- Re-registering a runtime source, or upgrading or removing the Codex program it
  was registered with, keeps existing conversations controllable and their
  captures valid.
- Busy and refusal errors say whether they clear on their own, so clients retry
  passing conditions silently and show the ones that need a person. A refused
  cloud connection is answered with its reason instead of being dropped, and
  subscribing to a paused conversation reports that it needs a resume.
- Opening history no longer copies the transcript and no longer fails while the
  runtime is writing to it. Identical history requests share one read, and
  history of a conversation whose folder was removed stays readable.
- Archive recovery backs off, parks failures that cannot succeed, and lets a
  resume take the conversation. A conversation larger than the capture budget
  is reported once instead of after every turn.
- The control channel keeps a slow or lagging reader connected, stops reads
  whose requester has left, evicts idle clients before refusing new ones, and
  reconnects within seconds after the machine wakes from sleep. A message sent
  again while its first delivery is in flight reports that delivery as pending
  and is never run twice.
- Claude Code conversations whose transcript Claude deleted, or that never
  received a message, reopen under their own id; one whose archived turns no
  longer have a transcript points to `agit resume`. Claude Code is found outside
  the daemon's PATH, bypass mode is not offered where Claude refuses it as root,
  transcript folders follow Claude's naming for long and non-BMP paths, and a
  terminal Claude session that cannot be shared says why.
- A failed `agit clone` suggests asking for the owner's grant only when Git
  reports an authentication failure; any other failed transfer suggests
  retrying and, if it fails the same way again, reporting it with the output.
  `agit fetch <owner/repo>` likewise offers the grant hint only when the Hub
  refuses access. A reader who already had access was sent to check
  collaborator grants after an interrupted transfer.
- `agit project bind --history all`, its `--dry-run` preview and `agit project sync`
  include Claude Code conversations that open with a large prompt, such as a pasted
  screenshot, instead of silently leaving them out of the project.
- `agit project bind --dry-run` refuses a directory that is already bound to
  another repository, as the bind itself does, instead of previewing a bind that
  would fail.

## [0.2.22] - 2026-10-07

### Added

- Remote control can attach to a supported native Claude Code conversation so
  multiple clients can send and steer messages, answer tool approvals and
  questions, stop the current turn, and change supported models while the native
  process remains the only transcript writer. The integration is installed by
  daemon startup and setup. A native process started before installation may
  need `/reload-plugins` before it accepts remote controls.
- Fresh native Claude conversations accept remote attachment before their first
  message. Native tool activity, compaction, questions and supported commands
  remain visible across reconnects. Only controls confirmed by the runtime are
  advertised; changing an active permission mode or answering an effort
  confirmation dialog is not supported by the native integration.

### Fixed

- Bursts of remote conversation events recover from the session journal without
  closing the control connection or leaving the composer stuck restoring.
  Operation receipts retain an independent output budget while history catches up.
- An explicitly refused Codex resume releases its opening reservation after
  cleanup, so subsequent attachment attempts do not remain permanently busy.
- Pending remote input cannot block tool approval or interruption. Ordered model
  changes, process and turn checks, and replayable receipts preserve operation
  order without repeating a prompt or stopping a later turn after reconnecting.
- Completed and interrupted native Claude turns are archived into ordinary Agit
  session history. Upload recovery retains pending publication through daemon
  replacement and waits until the complete captured prefix is published.
- Resuming a native conversation honors the explicitly requested authorized
  project and preserves its existing archive identity.
- Starting private Git commands from a large resident daemon preserves private
  file permissions without copying the daemon's memory into each child process.

## [0.2.21] - 2026-10-06

### Fixed

- Manual milestones and runtime stop hooks preserve each completed native
  continuation when a long-running goal reuses the same user prompt. Settlement
  extends a retained partial turn without skipping later completions or saving
  an already retained completion twice.

## [0.2.20] - 2026-10-06

### Fixed

- Remote terminal creation and input carry replayable receipts. A matching web
  client can recover a lost response without opening another shell or executing
  typed input again, while preserving input order and workspace isolation.
- Remote approval recovery reads authoritative pending requests so a lost reply
  does not apply the same decision twice. Codex stop retries target the selected
  native turn and cannot interrupt a later turn.
- Remote model settings recover across controller reconnects and expose the
  effective Codex reset model. Sequenced permission state allows controllers to
  reconcile a settings change without repeating an uncertain mutation.
- Steering rejected while a conversation is idle is distinguished from an
  uncertain delivery so the client can safely send it as a new turn.

## [0.2.19] - 2026-10-04

### Fixed

- Background remote-control archive recovery allows long commits and uploads to
  finish instead of interrupting them after a fixed request timeout. Recovery
  remains cancellable on shutdown and retains its pending publication receipts.
- Opening a saved conversation reads its verified archive boundary without
  waiting for the next turn's settlement locks. Native claim and publication
  checks continue to prevent a stale boundary from hiding uncached messages.
- Local daemon diagnostics include bounded failure details for archive landing,
  commits, and publication so a repository problem can be diagnosed directly.

## [0.2.18] - 2026-10-03

### Changed

- Remote control imports existing history when a session is resumed or explicitly
  watched in a workspace. Completed turns are archived in the workspace's Agit
  repository and uploaded automatically; a missing repository is created private.
  Collection and upload retries continue after the viewer disconnects and recover
  after a daemon restart without requiring another prompt.
- Remote history can return only messages after a verified canonical archive
  checkpoint. The matching backend and web update read saved history from ordinary
  Agit session storage and combine it with the device's uncached messages, without
  maintaining a separate web transcript.
- Remote viewers receive context-compaction activity from supported runtimes.

### Fixed

- Starting remote control after switching Cloud accounts enrolls the daemon for
  the current account and disables grants from the previous enrollment. Existing
  local conversation history remains available.
- Archive retries remain pending until the workspace publication target is ready,
  and recover when a confirmed push already retains the requested turn.
- Native Codex and Claude collection waits for the runtime's explicit completion
  boundary instead of archiving a partial turn between tool calls. Interrupted
  Codex turns also settle and upload their completed transcript.

## [0.2.17] - 2026-10-02

### Added

- Remote control on the web can send messages into a Claude Code session that is
  running in another process, such as the Claude desktop app or the `claude` CLI,
  through Claude Code's cross-session messaging (Claude Code 2.1.224 or later). The
  running session stays the only writer of its transcript: a message sent during a
  turn is read between tool calls, and an idle session starts a new turn with it.
  In bypass-permissions mode, the device owner's messages go through a relay that
  agit's Claude Code Stop hook (installed by `agit setup`) starts once a turn ends
  in that mode; Claude Code holds everyone else's messages, and the owner's while
  no relay runs, for approval on the machine. Only a workspace owner may send
  messages to a session whose transcript shows it has ever run in
  bypass-permissions mode, or records no permission mode at all.

### Fixed

- `agit push <owner/repo@branch> --to <existing repository>` (and `agit project sync`
  copies of sessions claimed elsewhere) no longer push the source repository's
  `main` file line. The destination keeps its own `main`; before, the copy failed
  as a rejected non-fast-forward, or fast-forwarded the destination's shared files
  to the source's. A destination the push creates still receives the source's `main`.
- A Claude Code prompt whose record starts with a runtime-injected block, such as the
  worktree reminder the desktop app puts ahead of a session's first message, is
  kept as the turn's prompt instead of being dropped with the injection, whether
  the reminder is a separate text block or leads the same string. Such a session
  lost its opening prompt, and one with a single prompt had no turn to save or
  upload.
- Remote control publishes a turn that finished while the device's sign-in had
  lapsed as soon as authentication recovers, instead of waiting for the next turn.
- Opening a device from the web no longer stalls when the device's first discovery
  reply is lost: the controller retries the read-only discovery once.

## [0.2.16] - 2026-10-02

### Added

- Each settled commit records the runtime's own name for the session in
  `session/meta.json` as `title`: the latest Claude Code title (`custom-title`,
  falling back to an older transcript's `summary`) or the Codex thread name from
  `session_index.jsonl`. A rename appears from the next settlement on. The
  `agit: unnamed` and `agit <owner/repo@branch>` placeholders that agit's own
  SessionStart hook assigns are not recorded. The title is protected like other
  metadata observations and is not part of encrypted metadata projection.

### Changed

- Privacy processing runs in a bounded local worker. Detected secrets receive
  reversible placeholders when their originals can be saved. Incomplete scans
  keep completed replacements; unavailable storage, scan failures and timeouts
  skip the affected privacy work and let conversation operations continue.
- Signed-in accounts obtain an automatically generated cloud dictionary key.
  Without a usable key, mappings remain in private local pending storage. Push,
  login and fetch schedule encrypted dictionary synchronization independently;
  another signed-in device can restore mappings after synchronization succeeds.
- Local default, global user and repository user rules share one policy path.
  Explicit blocks override allows, and allows override heuristic findings.
  Recovery records survive rule changes without becoming permanent block rules.
- Uploads require no secret-finding review or cloud privacy-policy resolution.
  Existing Git history is preserved. LFS payloads are outside privacy processing;
  their authorization, availability and integrity checks remain in place.
- This release requires the matching backend update to remove older server
  privacy gates. A skipped local transformation may retain original sensitive
  content, and pending dictionaries become recoverable on other devices only
  after successful encrypted synchronization.

- Project Stop hooks start session uploads in a background process and return
  immediately. Project status reports the resulting upload state.

### Fixed

- Automatic project binding installs hooks for available runtimes that support
  them; other runtime sessions remain publishable through project sync.

- Live output preserves every emitted fragment and the remaining stream tail
  when local privacy processing is unavailable.
- Legacy native message identifiers stored as secret placeholders can be restored
  without changing unmasked carrier identities.

- `agit project sync` publishes a session that another repository already claims, such
  as one started from a desktop RC project, as a separate copy of that claim's branch
  in the project repository, instead of failing with "session already belongs to another
  repository". The original claim and the RC project's own publication target are
  unchanged; each later sync refreshes the copy. `--history none` keeps excluding such
  earlier sessions.

## [0.2.15] - 2026-10-01

### Changed

- **New repositories use ordinary publication by default again.** Without
  `--encryption` or a `privacy.encryption` preference, `agit push`, `agit init`,
  `agit repo create` and the init wizard create repositories that publish original
  history and need no viewing password. Encryption is an explicit opt-in chosen at
  creation with `--encryption=true` or `agit config privacy.encryption true`, after
  which `agit privacy init` sets the viewing password. `agit privacy init` still
  creates a missing repository encrypted unless `false` is chosen, and existing
  repositories keep the mode they were created with. A separate `agit push --to`
  destination created from an encrypted repository stays encrypted unless
  `--encryption=false` or a `privacy.encryption` preference chooses otherwise.
- Local repositories set up by 0.2.13 or 0.2.14 without an explicit `--encryption`
  choice follow the new default. If `push.auto` is on and the repository does not
  exist on the Hub yet, the next automatic push creates it as an ordinary repository,
  private unless `push.visibility` says otherwise. To keep such a repository
  encrypted, run `agit privacy init OWNER/REPO` first, or set
  `agit config --global privacy.encryption true`.
- A checkout initialized with `agit init --encryption=true` refuses its first push
  to an existing repository that uses ordinary publication, instead of silently
  publishing original history there; pass `--encryption=false` to publish anyway,
  or push to a new repository name to create an encrypted one.
- Encrypted `agit share` links require an encrypted repository. Sharing from an
  ordinary repository needs `--public`; without it the command now says so and
  uploads nothing, instead of asking for a publication that cannot exist. Encrypted
  links for ordinary repositories are not available in this release.

### Fixed

- Ordinary pushes from scripts, CI and agent sessions no longer require `--yes`;
  a person at a terminal is still asked to confirm the destination, and declining
  or cancelling that prompt publishes nothing. Secret findings still block
  publication unless `--allow-secrets` is given, and encrypted publication keeps
  its confirmation.
- Automatic ordinary pushes after settlement, and Remote Control publication to an
  ordinary repository, no longer need a prior explicit push. `push.auto` authorizes
  them and they pass the same identity, access and secret checks as a manual push.
  Outside Remote Control, a missing repository is created with the visibility a
  non-interactive first push uses. Encrypted repositories still require an explicit
  push to confirm their policy and viewing recipient before automatic publication.
- Remote Control retires temporary runtime sources automatically once their
  directories disappear.

## [0.2.14] - 2026-09-30

### Added

- `agit project bind` enrolls a selected directory and its unbound subdirectories
  in an existing repository, with explicit history import and automatic upload
  choices. `project sync` retries synchronization, `project status` reports local
  results, and `project unbind` pauses project capture. Codex and Claude Code hooks
  preserve existing session claims and enforce the current project policy before
  publishing.

### Fixed

- Repeated Remote Control history reads reuse bounded, authenticated secret
  dictionary records and compiled protection patterns. Changed ciphertext,
  keys, or protection rules invalidate the corresponding cached state.
- Release builds optimize for runtime speed to reduce history projection and
  secret scanning latency.
- Remote Control retries failed local session saves while idle, without requiring
  another user turn to resume archiving.
- Push uses authenticated remote history to avoid rescanning already-published
  objects, while continuing to inspect new objects and their selected LFS payloads.
  Use `--audit` when a complete history review is needed.
- Native history reads take authenticated, bounded dictionary snapshots without
  waiting for a dictionary writer. Conservative pattern filtering avoids scanning
  history for secret patterns that cannot occur while preserving redaction rules.
- Compatible Cloud servers can read native history through a scoped project
  controller without opening a separate session controller. Project controllers
  remain read-only and cannot subscribe to or change a conversation.
- Completed RC turns retain durable archive jobs until protected publication has
  a receipt. A daemon restart retries pending jobs without starting the model or
  requiring a viewer to open the conversation.
- Settlement subprocesses retry temporary executable-busy failures within a
  bounded window while retaining the current authority lease.

## [0.2.13] - 2026-09-30

### Changed

- **Installations that skip install scripts are counted.** An installation whose
  installer recorded no install receipt, such as one run through `npx`, with
  npm `--ignore-scripts`, or by a package manager that skips install scripts,
  now records one from its first user command, marked
  `receipt_origin=first_command`; installer receipts are marked
  `receipt_origin=installer`. Setup, hooks, MCP, RC, background processes and
  commands an installer runs never record it, so a create-agit receipt keeps its
  acquisition key. Installations whose statistics state predates this release,
  or whose `AGIT_HOME` already held older state, are never reported this way. See
  [First-command receipt](docs/telemetry.md#acquisition-funnel-contract).

- **Session reuse and invitation counts on the Hub.** When `agit run` continues
  a session line or forks from a point on one, it sends the Hub one receipt
  naming the repository, the source session ID and commit, whether the run
  continues or forks, and the per-process operation ID, so the Hub can count
  how often sessions are picked up. A continue is reported once the session is
  ready to launch, a fork once its branch exists, and a `--mine` fork to the
  repository it copied. The receipt is sent in the background only for
  checkouts of the configured Hub, only from a settled session, and never with
  renewed credentials: an expired sign-in sends nothing. A Hub that is
  unreachable or refuses it never changes the run's output or exit code.
  `agit fork` and `agit resume` stay offline.
  `agit repo invite <owner/repo>@<branch>` also tells the Hub which session its
  link opens. The download attribution headers still carry no arguments or
  names. See
  [Session reuse and invitations](docs/telemetry.md#session-reuse-and-invitations).

### Fixed

- Remote Control can return a fresh, bounded native history page with watch
  admission, avoiding a separate network round trip when opening a conversation.
  Native history transfers negotiate compression, preserving complete tool
  results and compatibility with older clients and servers. Optional history
  reads do not block the daemon or prevent a successful live subscription.
- Device name changes synchronize to Cloud without enrolling another device.
- Failed session publication retries while the conversation is idle, so a
  transient upload failure does not require another user message to recover.

- `agit login` checks that it can save credentials under `AGIT_HOME` before it
  asks the Hub for anything. An agent sandbox that refuses writes there no
  longer uses up the human's approval: login stops with the directory to allow
  and says no login request was created, and `agit login --complete` leaves the
  approved sign-in unclaimed so the same command finishes it later. If saving
  still fails after sign-in, the new Hub session is signed out again.
- Signing in from an agent no longer fails when the agent runtime stops the
  waiting `agit login`. Every browser and device-code request is recorded
  privately under `AGIT_HOME` when the Hub creates it, and
  `agit login --complete` without a value finishes it from any later process,
  including an interrupted `agit login --device`. `--complete` now waits for
  the approval for up to 90 seconds (`--wait <seconds>`, `0` checks once)
  instead of checking once, keeps waiting through a failed poll, keeps the
  request when the wait runs out, and forgets it when the Hub says it expired
  or was already used. Running the printed `--complete` command again after
  the sign-in finished reports success instead of "no longer valid". A new
  `agit login` finishes a request the human already approved instead of
  replacing it. `agit whoami --check`, `agit commit` and commands that stop
  with "not logged in" first claim an approved request once, and otherwise
  point to `agit login --complete` instead of a new login. Signing out cancels
  a sign-in still being claimed: a session approved after `agit logout` is
  signed out again instead of saved, and a session a concurrent sign-in saved
  while `agit logout` ran is revoked with the credentials it removes. A second
  `agit login --complete` or a new `agit login` started while another process
  claims the same sign-in waits for it and reports success instead of "no
  longer valid" or a new login link.
- Local state problems read as local problems. `agit whoami --check` reports
  an unwritable home or a held credential lock instead of "can't reach" the
  Hub, `agit doctor` checks that `AGIT_HOME` is writable, and every command
  that meets one exits with the precondition code. Messages name the agit lock
  another process holds and say that agit's lock files are released when their
  process exits and must not be deleted; credential locks give up after a
  bounded wait, and session locks say what they are waiting for.
- Re-importing a session describes its claim: a registration is saved by the
  import, a saved session says where and how to record later turns, and a
  session saved elsewhere says how to move it. The link file path is no longer
  offered as a next step, and a failed import restores the session's previous
  link, including none.
- A session targeted at `main` is refused before anything is adopted or
  created, with the reason and an example branch in the caller's syntax.
- `agit push` leaves an `origin` that already points at the destination alone
  and writes a changed one under the repository lock, so a manual push and a
  hook's auto-push no longer collide on `.git/config`. Git failures on a held
  lock say not to delete it while a git process runs; a refused write says the
  checkout is not writable instead.
- `agit setup` recognizes its own hooks by their arguments, so installing from
  another executable path replaces the old entry instead of adding a second
  one.

## [0.2.12] - 2026-09-28

### Fixed

- Retry RC daemon reconciliation when a global npm installation's lifecycle
  scripts are disabled. The next CLI invocation schedules the guarded handoff
  in the background, while project-local and temporary npm packages cannot
  replace a persistent daemon.

## [0.2.11] - 2026-09-28

### Fixed

- Reconcile the local RC daemon after self-upgrade, global npm installation, or
  `npx create-agit`, including installations at a different executable path.
  Idle sessions attached to a persistent Codex server can detach for a safe
  restart; busy daemons retry after their work clears.
- Stage the `create-agit` executable before replacing it, so an installation
  failure preserves the previous CLI. Report when another `agit` on `PATH`
  shadows the installed copy.
- Reach the Hub without a fixed stall when one of its addresses is unreachable
  from your network. agit dialed the resolved addresses one after another and
  gave the first one most of the connection budget, so every command that talked
  to the Hub could wait about 20 seconds before trying the next address. The
  addresses now race: the next one starts after 250 ms or as soon as the
  previous one fails. Hub API requests, SOCKS proxy connections, LFS
  availability checks and `agit upgrade` downloads all use it.

## [0.2.10] - 2026-09-28

### Fixed

- Preserve native catalog scan health through delegated permission filtering,
  including incremental responses with no changed conversations. Remote Control
  can distinguish a running scan from an unavailable source while source
  identities remain scoped to readable conversations.

## [0.2.9] - 2026-09-27

### Fixed

- Accept start and steer instructions for attached, source-qualified Codex
  conversations through Cloud controllers. Managed conversations retain their
  executor identity and control policy; catalog entries alone do not authorize
  these operations.
- Keep runtime source identities when local builds differ in directory creation
  time support. Device and inode checks remain required, and conflicting known
  creation times still reject a replaced directory.

- Preserve the enrolled runtime source when resolving a saved conversation after
  a daemon restart, so Cloud controllers resume the same native conversation.

## [0.2.8] - 2026-09-27

### Added

- Discover conversations in registered Codex runtime homes independently of RC
  attachment. `agit rc sources add` accepts custom directory names and selected
  executables or endpoints. Same-account native processes and recognized profiles
  can enroll their runtime homes automatically.
- Connect RC and native Codex clients to the same persistent app-server, using
  its standard socket when supported. Both launch orders preserve one native
  conversation and keep the service alive after a subscriber disconnects.
- Persist source-qualified catalog identities and incremental discovery cursors,
  including reconnect recovery, removals, source health, and missing-index fallback.
- Retain native parent relationships and internal-conversation provenance for
  scoped expanded catalog views.

### Fixed

- Queue messages into the existing native conversation and execute them without
  starting a competing writer. Durable message receipts prevent repeated delivery.
- Read and update supported native model, reasoning effort, and permission
  settings through the selected runtime source, preserving its current policy
  during attachment. Shared approvals reconcile across subscribers.
- Release acquired private fallback control after 15 minutes without a human
  instruction only when native background unloading is verified. Running work
  and transcript observation continue; shared service connections do not expire.
- Revalidate source, project, caller, and delegated authority across reconnects,
  control transitions, pending instructions, and approval replies.

## [0.2.7] - 2026-09-25

### Fixed

- Validate lexical and canonical private-state ancestors consistently for Remote
  Control and bundled Git. Sticky shared home directories remain usable while
  replaceable ancestors and writable private state are rejected.
- Page large protected conversations without loading the entire history into
  memory or exhausting the snapshot byte budget.
- Preserve real device paths in Remote Control and allow authorized folder
  browsing outside the home directory. Report unreadable folders as errors and
  reject binding the filesystem root as a project.
- Expose the observed model, reasoning effort, and permission mode while
  following a native Codex conversation without acquiring its writer. Setting
  changes remain under the original controller's authority.
- Protect imported and newly created session metadata, retain structured MCP
  image context, and handle escaped repository placeholders correctly.
- Apply consistent publication inspection to binary artifacts and Git LFS content.

## [0.2.6] - 2026-09-23

### Changed

- **Download counting on the Hub.** Git and Git LFS requests that agit sends to
  the configured Hub carry `X-AgentGit-Command` (the top-level command, such as
  `clone` or `pull`) and `X-AgentGit-Operation` (a random ID generated once per
  process), so the Hub counts one download per invocation and repository
  instead of one per negotiation round or credential retry. The headers contain
  no arguments, names, paths or machine identifiers, and other Git remotes never
  receive them. See [Hub download attribution](docs/telemetry.md#hub-download-attribution).
- Session page links printed by `agit show` and `agit file link` add
  `sharer=<username>` when you are signed in to that Hub, so visits through a
  link you paste are credited to you. Signed out, links are unchanged.

## [0.2.5] - 2026-09-23

### Added

- **Invite links from the CLI.** `agit repo invite <owner/repo>` prints a link
  that adds whoever opens it as a collaborator (`--role read|write|owner`,
  default `read`). `agit repo invite <owner/repo>@<branch>` (or `-b <branch>`)
  also lands the invitee on that branch's session page after they accept. Only
  repository owners can create links; they do not expire and can be revoked in
  the repository's settings (Invite by link). A session link requires the
  session to be pushed already. `--json` reports the link, role, repository,
  invitation ID and, for a session, its page URL.

### Changed

- Remote Control history projection reuses compiled persona patterns, so long
  shared histories page in faster.

### Fixed

- `agit rc stop` now waits for the daemon to exit before reporting success, so
  an immediate `agit rc start` no longer races the previous daemon.
- Remote Control discovers native sessions when the same project folder is
  bound through equivalent directory paths.
- The `npx create-agit` installer's closing hint no longer suggests an
  outdated `agit import` invocation; it points to the quickstart instead.

## [0.2.4] - 2026-09-21

### Fixed

- Return an existing native Codex inbox receipt before probing the executable,
  so reconnect retries remain observable when Codex is temporarily unavailable.
  Retries with the same message ID do not enqueue another message.
- Coalesce repeated native history protection failures and keep internal error
  details in daemon logs instead of repeating them in conversation history.
- Preserve native conversation titles in session pickers and omit runtime
  bookkeeping from message previews and counts.
- Settle multiple newly detected heuristic secrets in one pass, and bound identity
  evidence to the provenance budget when scanning large inputs.

## [0.2.3] - 2026-09-20

### Added

- **Send to externally owned Codex conversations.** Authorized operators can
  enqueue text through the installed Codex CLI while its original process keeps
  the writer lock. Capability discovery is nonmutating, and durable receipts
  prevent duplicate submission after reconnect. Queue acceptance does not mean
  execution; Codex controls consumption. Live controls still require ownership.
- Include Cloud connection diagnostics and executor history phase timings to
  help locate connection and history-loading delays.

### Changed

- Batch native history protection and watch projection, and coalesce concurrent
  history captures while retaining session identity and turn order.
- Remove the standalone `rc cloud enroll` command. `agit login` followed by
  `agit rc start --detach` performs registration automatically.

### Fixed

- Resume Codex sessions promptly after the native writer releases ownership.
- Preserve empty Codex sessions before publishing them to a workspace.
- Keep unadopted native previews available with secret protection.
- Compare hydrated native content when resolving resume identity, and ignore
  bookkeeping after a settled turn when checking for unsettled work.

## [0.2.2] - 2026-09-19

### Added

- **Shared workspace controllers.** Accept project- and session-scoped authority
  from the Hub controller, so collaborators can operate a shared session without
  separate device grants. This requires a Hub with shared controller support;
  personal peer/Cloud connections retain their existing protocol.
- Expose native model and reasoning-effort controls through peer remote control.
- Protect detected secrets automatically with reversible, repository-local
  placeholders before saving or publishing session content.

### Changed

- Keep native controls responsive while prepared settlement completes, reuse
  healthy session worktrees and Hub connections, and replace repeated Git child
  processes with bounded local object reads.
- Start tunnel transport independently of repository storage and overlap Cloud
  admission and peer setup to reduce connection overhead.
- Official Hub usage requires usage statistics; installation attempts and stages
  are included in the documented collection policy. Other Hubs retain their
  opt-out controls. See [usage statistics](docs/telemetry.md).

### Fixed

- Keep shared native subscriptions attached to controller authority, isolate each
  viewer's replay, and suppress duplicate events on Cloud utility routes.
- Preserve early session events, uncertain command receipt identities, native
  titles, and saved session identities outside the active catalog.
- Correct Windows path component handling and directory navigation.
- Distinguish stale daemon records from reused process IDs, recover stopped
  daemons after upgrades, and enforce private state permissions under permissive
  umasks.
- Preserve fresh native history during model reads and settlement without
  blocking read-only controls on repository metadata work.

## [0.2.1] - 2026-09-17

### Added

- **Native Windows peer remote control.** Windows supports the same controller,
  executor and Cloud tunnel protocol as Linux and macOS, using a current-user
  named pipe for local owner RPC.
- **Bundled Git and Git LFS.** Official distribution binaries include their Git
  runtime, so session version control does not require a separate Git installation.

### Changed

- **Owner-only Cloud access by default.** Run `agit login`, then
  `agit rc start --detach`. Startup registers the device and enables remote control
  for the signed-in owner, without a separate enrollment command. Use the same Hub
  account in Web Workspaces or the desktop app. This does not grant public access.
- **Remove the paired RC transport.** Startup and device management use peer/Cloud
  only. Users of the paired default in 0.2.0 must upgrade; the existing Linux 0.2.0
  peer/Cloud protocol remains supported. Server retirement follows verification of
  the new Windows and Linux release artifacts.
- Reuse Cloud HTTP connections, overlap admission checks with tunnel setup, and
  race resolved TCP addresses to avoid waiting on a slow address before trying another.
- Reduce repeated repository checks and keep repository preparation and background
  metadata work off the native session command loop. Record startup phase timings
  without logging prompt contents.

### Fixed

- Prevent detached Windows daemon helpers from opening console windows; release
  inherited caller pipes when starting in the background.
- Preserve tunnel failure details and renew Cloud authority without repeatedly
  reconnecting healthy idle sessions.
- Recover a revoked device registration when its owner explicitly starts RC again;
  background reconnects do not undo revocation.
- Read history and goals from fresh Codex sessions before a transcript file exists,
  and isolate history readers from snapshot capture and session control.
- Preserve native history identity and snapshot consistency, and protect generated
  session observations with repository secret rules before settlement.

## [0.2.0] - 2026-09-16

### Added

- **Daemon peers and Cloud tunnels.** `agitd` manages local harness sessions while
  independent SSH or Cloud tunnel workers transport peer messages. Controllers can
  discover and operate sessions on another executor through the same peer protocol.
  Peer hosting is available on Linux and macOS. Cloud connections require a Hub
  with peer relay support enabled.
- **Independent control and inbound access.** A local controller can connect to
  another device while its own inbound access stays disabled. Cloud admission and
  executor session permissions are enforced separately. Adapters can constrain
  requests to a lower role, enforced by the executor when a queued write runs.
- **More native conversation sources.** Discover and import OpenClaw, Hermes and
  WorkBuddy sessions, with runtime-specific setup and transcript handling.
- **Large session files.** Stage binary deliverables with `agit file add --lfs`;
  verify payloads during upload and download before publishing or materializing them.
- **Local and scoped search.** Search saved local history without Hub requests, or
  limit remote session searches to an organization or the current code origin.
- **Optional automatic publishing.** Configure `push.auto` per user or repository
  to publish settled turns, while keeping explicit publication available.
- **Usage statistics controls.** Inspect collection with `agit telemetry`, disable
  it with `agit telemetry disable` or `DO_NOT_TRACK=1`, and preview the field policy.
  Setup discloses the default-on choice; a recorded opt-out is preserved. See the
  [collection and privacy details](docs/telemetry.md).

### Changed

- **Use `agit run` for saved sources.** `agit open` is removed. Update scripts to
  use `agit run owner/repo@ref`; `agit resume` still continues a selected session.
- Review a frozen publication in an interactive agent before pushing, including
  explicit handling of credential findings.
- Status includes bounded native session details, shared files, merge progress and
  project metadata without unbounded transcript scans.
- Interactive startup can offer to install an available update after confirmation.
  Non-interactive update notices go to stderr and preserve JSON output.
- Repository secret dictionaries keep their encryption keys in local Git metadata,
  avoiding repeated Keychain prompts after migration. Neither the dictionary nor
  its key is uploaded by push.

### Fixed

- Recover from expired CLI credentials and retry browser sign-in choices without
  losing the selected Hub or repository.
- Keep session discovery and native watch preparation responsive, hide internal
  runtime sessions, and preserve transcript identity across polling and replay.
- Correlate canonical Codex user history and prevent duplicate Claude prompts.
- Preserve native Codex session titles and recover remote session history reliably.
- Keep controller requests, tunnel failures and harness failures within their
  ownership boundaries, with bounded queues and diagnostic logs for recovery.
- Support Hub SOCKS proxies and preserve Windows browser launch, process ownership
  and named-pipe behavior.
- Scope installed AgentGit skills to explicit session operations and retire legacy
  home-directory instructions without replacing unrelated user content.

## [0.1.2] - 2026-09-12

### Added

- **Session files with explicit staging.** Use `agit file` to add, inspect, commit,
  retrieve and link deliverables in a selected branch without changing its conversation
  VIEW. File staging remains separate from automatic turn settlement.
- **Windows x64 distribution.** Install the native Windows CLI through npm or
  download the executable from the GitHub Release, including remote-control support.
- **More remote-control workflows.** Connect existing Codex conversations through
  the native inbox, use OpenCode remote control and transcript snapshots, and navigate
  between connected machines and their workspaces.
- **Richer history inspection.** Inspect saved VIEWs and LOGs, raw native JSONL and
  archived evidence; compare semantic prefixes and unsettled native turns with `diff`.
- **Scoped search and integrity checks.** Search authenticated repository scopes,
  inspect incomplete-result diagnostics, and run bounded, read-only `doctor` checks.
- **Safer import and review.** Choose native-session lineage explicitly, preview and
  name sessions interactively, and review committed VIEWs with guarded scan remedies.

### Changed

- **Explicit session targeting.** Use `owner/repo@branch` or `AGIT_SESSION` for
  automation. Interactive commands offer target selection; `agit switch` and implicit
  workspace targeting are removed. Update scripts that depended on those defaults.
- **Structured agent output.** JSON output includes typed recovery actions, while
  human output identifies verified targets and quiet mode suppresses progress output.
- Merge-agent exploration remains available as archived evidence and visible session
  history without adding that exploration to the merged VIEW.

### Fixed

- Preserve Codex fork history, portable provider metadata, mixed-runtime sharing and
  paired tool evidence during capture, resume and export.
- Preserve reference, network, authentication, policy and cancellation error categories;
  bind credentials to the selected Hub and validate setup before applying changes.
- Keep failed remote API turns responsive, preserve shared-message authors and native
  execution feedback, and support HTTP CONNECT proxies between `agitd` and the Hub.
- Validate imports before adoption, prevent duplicate runtime claims, guard resume
  against tracking divergence, and preserve private-publication checks.
- Avoid repository-wide migration scans on clean stores and skip tags already present
  on a verified remote during push.

## [0.1.1] - 2026-09-04

### Added

- **A terminal interface for people.** `agit`, `agit resume`, `agit new`, `agit log`,
  `agit import`, `agit init` and `agit config` open a full-screen interface when run in
  an interactive terminal without their key argument: browse sessions, repositories,
  the timeline and conversation content; name and adopt sessions that are not tracked
  yet; an `init` wizard and a `config` editor; hand the terminal to Claude Code or
  Codex and come back to refreshed lists. Pipes, CI, scripts and agent sessions keep
  the existing output, and `--no-tui`, `AGIT_TUI=0` or any machine-output flag
  (`--json`, `-q`, `-y`) turns it off. See [docs/07_tui.md](docs/07_tui.md).
- **Update check.** On user-facing startup agit checks, at most once a day, whether the
  hub announces a newer release and prints a reminder; `agit upgrade` installs it.
  Nothing upgrades on its own.
- **A file keystore for machines without a credential store.** On an SSH login or a CI
  runner no Secret Service answers, so the secret-filter key had nowhere to go and the
  first `agit commit` whose transcript carried a heuristic finding failed with "cannot
  open the operating-system credential store". `agit config secrets.keystore file` (or
  `AGIT_SECRETS_KEYSTORE=file`) keeps the key in a private file under
  `$AGIT_HOME/keystore/` instead. Unix only, chosen explicitly and never a silent
  fallback; its protection is the file mode, so a backup of `$AGIT_HOME` carries the key
  along with the global vault — the boundary is drawn in
  [docs/05_global_secret_filter.md](docs/05_global_secret_filter.md).
- **`agit doctor` reports the secret keystore.** It probes the configured store the way
  a commit uses it and unlocks the vault if one exists, so a machine that cannot hold
  the key shows up at setup time rather than at the first commit that finds a secret.

### Fixed

- `agit fork` of a sealed branch no longer produces a sealed branch: the seal marker is
  branch-local and is dropped when the fork gets its identity (issue 23).
- Codex sessions under a custom `CODEX_HOME` are discovered and settled, and the
  SessionStart and Stop hooks locate the session and settle it correctly.
- Missing local branches, phantom `origin` branches, the log limit's performance, and
  cursor restoration after leaving the interface.
- When the OS credential store is unavailable, the error names both remedies — install
  and configure a credential store, or select the file keystore. Every other keyring
  error keeps its own meaning.
- Hints are highlighted in bright magenta so they stand out from ordinary output.

### Internal

- The GitHub mirror job clears stale replace refs before planting the graft, so a reused
  runner checkout no longer aborts the mirror.

## [0.1.0] - 2026-09-01

First public release.

- Lossless version control for agent sessions: `agit import`, `commit`, `push`, `clone`
  and `resume` for Claude Code, Codex, OpenCode and Cursor.
- Session lines as branches, workspaces, forks and merges across several people.
- Secret scanning before publishing, a device-local filter for registered low-entropy
  secrets, and reversible repository-local placeholders.
- Distribution through npm — `npx -y create-agit` or `npm i -g @einsia/agent-git` — with
  per-platform packages for Linux and macOS on x64 and arm64, and GitHub Release
  artifacts with `SHA256SUMS`.

[0.2.1]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.2.1
[0.2.0]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.2.0
[0.1.2]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.2
[0.1.1]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.1
[0.1.0]: https://github.com/Einsia/agent-git/releases/tag/agit-v0.1.0
