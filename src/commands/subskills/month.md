---
name: agit-month
description: Write a person's month in an organization from their own sessions, with their review, and publish it to the Hub as their own text.
---

# Write a month summary

Use `agit month` when the person asks for a summary of their own work in an organization over one month. You write the text with the person's own model, on their machine; the Hub stores it as their text and never runs a model over it.

```bash
agit month draft acme 2026-10
agit month publish acme 2026-10
agit month clean acme 2026-10
```

1. Run `agit month draft <org> <YYYY-MM>`. It reads the person's month from the Hub (building it first when needed) and, for each repository in it, what was asked and what came back in the sessions the Hub lists for them. It prints the bundle's directory, a private directory under the AgentGit home; never copy the bundle into the working directory or anywhere else. A new draft replaces the earlier one. If the directory holds a `summary.md` or another file the person wrote, the draft refuses and names it; ask the person before you run it again with `--replace`, which deletes that file.
2. Read the bundle's `README.md` and its JSON files. Each file has the repository's `sessions` (id, title, dates, turns, milestones) and their `contributions` (the person's messages and the last reply to each). A non-empty `incomplete` means part of a session could not be read: say so rather than treating it as absent.
3. Write `summary.md` in the bundle directory: one `## owner/repo` section per repository the person wants published, in their voice — what was worked on and what came of it. Write each section only from its own repository's file: readers of that repository see it, so nothing from another file, a private repository's above all, goes into it. Name the sessions a section draws on as links whose target is the session id, such as `[Lease pool rework](agit-…)`, using only ids listed in that repository's file. Describe the work; never evaluate people, and leave out anything that looks like a credential, a personal detail or another person's private matter.
4. Show the person `summary.md` and wait for their edits. Change it as they ask.
5. Tell the person to run `agit month publish <org> <YYYY-MM>` themselves. Do not run it for them: it refuses to run without a terminal, lists each section with its size and who would read it, and sends only after they confirm. A section is refused when there is no draft of its repository next to `summary.md`, it names no session, a session it names is not theirs in that repository or can no longer be shown, or it looks like it holds a credential (including values registered with `agit secrets`). Each section is sent with the version of the section the draft read; if the person hid a day or rebuilt the month since, the Hub refuses it and the month has to be drafted again.

What is published stays the person's alone until they share the month on the website; members who can read a repository then see its section, labelled as written by the person. A later withdrawal, exclusion, hide or erase of a session a section names takes that section off the Hub until the person publishes again. A successful publish deletes the private bundle under the AgentGit home, and `agit month clean` deletes it without publishing; a bundle written with `--out` stays where it is until the person deletes it.
