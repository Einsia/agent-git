# hub-ui

The AgentGitHub web frontend — a React SPA that renders the agent roster, session
traces (with the signature **session spine**), provenance, permalinks, revision diffs,
an organization overview (`/orgs/<name>`), and a code-repo index (`/repos`).
It consumes the JSON API served by the `agit-hub` binary at the same origin.

Stack: Vite + React + TypeScript + Tailwind CSS v4 + shadcn/ui (new-york) + react-router.

## Build

```sh
./build.sh ui        # from the repo root — npm install (first time) + npm run build
# or:
cd hub-ui && npm install && npm run build
```

The build emits fixed, unhashed filenames into `hub-ui/dist/`:

```
dist/index.html
dist/assets/app.js
dist/assets/app.css
```

`src/bin/agit-hub.rs` embeds those three files at compile time with `include_str!`, so the
Hub ships as a single self-contained binary. **`hub-ui/dist/` is committed** for that reason —
rebuild it and commit the result whenever you change the frontend, then `./build.sh` the Rust side.

## Dev

```sh
cd hub-ui && npm run dev
```

Runs Vite's dev server. Point it at a running `agit-hub serve` for the API (proxy or absolute
URLs); the production build assumes the API is same-origin.
