// agentgit-opencode-plugin __AGIT_PLUGIN_VERSION__
// Written by `agit setup`, which replaces it with its own newer version; `agit setup --remove-skill`
// removes it with the rest of agit's OpenCode files.
// When an OpenCode conversation becomes idle, it hands that conversation's id and folder to agit,
// which settles the finished turn and, in a folder that uploads by itself, uploads it. Nothing is
// read from the conversation here and nothing leaves this computer from this file. agit runs
// detached, so OpenCode never waits for it; when agit is not installed nothing happens.
import { spawn } from "node:child_process";

const AGIT = "__AGIT_EXE__";

export default async ({ directory }) => ({
  event: async ({ event }) => {
    if (event?.type !== "session.idle") return;
    const session = event.properties?.sessionID;
    if (typeof session !== "string" || session === "") return;
    try {
      const child = spawn(AGIT, ["hooks", "settle", "--runtime", "opencode"], {
        detached: true,
        stdio: ["pipe", "ignore", "ignore"],
      });
      child.on("error", () => {});
      child.stdin.on("error", () => {});
      child.stdin.end(JSON.stringify({ session_id: session, cwd: directory }));
      child.unref();
    } catch {}
  },
});
