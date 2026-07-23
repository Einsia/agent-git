---
sidebar_position: 2
title: Install
---

# Install

Install the `agit` CLI, then make sure a runtime is on your `PATH`. The hub (`agit-hub`) is a separate
server; you do not need to install it to use the CLI. To run one, see
[Deploying the hub](../self-hosting/deploying.md).

## Install the CLI

### From npm

```bash
npm install -g @einsia/agentgit
```

This installs the `agit` client globally.

### Build from source

The project is Rust (edition 2024). Building needs `cargo >= 1.78`.

```bash
git clone https://github.com/Einsia/agent-git
cd agent-git
./build.sh --release
```

`build.sh` checks the toolchain and runs the release build. The binary lands at
`target/release/agit`; put it on your `PATH`.

## Install a runtime

agit records sessions from a coding-agent runtime. Install at least one and make sure it is on your
`PATH`:

- **Claude Code** (`claude-code`)
- **Codex** (`codex`)

Commands that read a session use the runtime you name with `--from`, else the only one present, else they
ask. Install both if you want to move a session between them; see [Runtimes](../cli/runtimes.md).

## Verify

```bash
agit --version
```

```
agit 0.2.1
```

Then check that your environment is set up correctly:

```bash
agit doctor
```

`agit doctor` prints a fast health check: which runtimes are on your `PATH`, your git identity, the
active agent's store versus its remote, and whether a watch daemon is running. See
[Diagnostics](../cli/diagnostics.md).

:::note
agit attributes each recorded session to your git identity and refuses to record while it is unset. If
`agit doctor` reports no identity, set it once as you would for any git repository:

```bash
git config --global user.name  "Your Name"
git config --global user.email you@example.com
```
:::

## Next

- [Quickstart](./quickstart.md): record, resume, and publish your first session.
- [Concepts](./concepts.md): the mental model before you start.
