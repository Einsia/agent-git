// agentgit-pi-extension __AGIT_PLUGIN_VERSION__
// Written by `agit setup`, which replaces it with its own newer version; `agit setup --remove-skill`
// removes it with the rest of agit's Pi files.
// When Pi settles after a run, or a session ends, it hands that session's id, file and folder to
// agit, which settles the finished turn and, in a folder that uploads by itself, uploads it.
// Nothing is read from the conversation here and nothing leaves this computer from this file. agit
// runs detached, so Pi never waits for it; when agit is not installed nothing happens.
import { spawn } from "node:child_process";

const AGIT = "__AGIT_EXE__";

function settle(ctx, event) {
  const file = ctx?.sessionManager?.getSessionFile?.();
  const session = ctx?.sessionManager?.getSessionId?.();
  if (typeof file !== "string" || file === "") return;
  if (typeof session !== "string" || session === "") return;
  try {
    const child = spawn(AGIT, ["hooks", "settle", "--runtime", "pi"], {
      detached: true,
      stdio: ["pipe", "ignore", "ignore"],
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(
      JSON.stringify({
        runtime: "pi",
        session_id: session,
        transcript_path: file,
        cwd: ctx.cwd,
        hook_event_name: event,
      }),
    );
    child.unref();
  } catch {}
}

export default function (pi) {
  pi.on("agent_settled", async (_event, ctx) => settle(ctx, "agent_settled"));
  pi.on("session_shutdown", async (_event, ctx) => settle(ctx, "session_shutdown"));
}
