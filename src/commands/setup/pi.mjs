// agentgit-pi-extension __AGIT_PLUGIN_VERSION__
// Written by `agit setup`, which replaces it with its own newer version; `agit setup --remove-skill`
// removes it with the rest of agit's Pi files.
// When Pi settles after a run, or a session ends, it hands that session's id, file and folder to
// agit, which settles the finished turn and, in a folder that uploads by itself, uploads it.
// Nothing is read from the conversation here and nothing leaves this computer from this file. agit
// runs detached, so Pi never waits for it; when agit is not installed nothing happens.
// A turn agit couldn't save is told on Pi's screen, by the one line agit prints for it: after a
// run, where Pi has a screen, once for each kind of failure in a conversation. The line goes to
// the screen alone, never into the conversation or to the model.
import { spawn } from "node:child_process";

const AGIT = "__AGIT_EXE__";

// How much of agit's output is held while a line is still coming: agit prints one short line.
const PENDING = 64 * 1024;

// Shows each `{"notice": ...}` line agit prints. The output is read as it comes and keeps nothing
// open: Pi exits whenever it would, and a context Pi has replaced since only throws, here.
function tell(ctx, output) {
  let pending = "";
  output.setEncoding("utf8");
  output.on("error", () => {});
  output.on("data", (chunk) => {
    pending += chunk;
    let end;
    while ((end = pending.indexOf("\n")) >= 0) {
      const line = pending.slice(0, end);
      pending = pending.slice(end + 1);
      try {
        const notice = JSON.parse(line)?.notice;
        if (typeof notice === "string" && notice !== "") ctx.ui.notify(notice, "warning");
      } catch {}
    }
    if (pending.length > PENDING) pending = "";
  });
  output.unref?.();
}

function settle(ctx, event) {
  const file = ctx?.sessionManager?.getSessionFile?.();
  const session = ctx?.sessionManager?.getSessionId?.();
  if (typeof file !== "string" || file === "") return;
  if (typeof session !== "string" || session === "") return;
  const shows = event === "agent_settled" && ctx?.hasUI === true;
  try {
    const child = spawn(AGIT, ["hooks", "settle", "--runtime", "pi"], {
      detached: true,
      stdio: ["pipe", shows ? "pipe" : "ignore", "ignore"],
    });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    if (shows && child.stdout) tell(ctx, child.stdout);
    child.stdin.end(
      JSON.stringify({
        runtime: "pi",
        session_id: session,
        transcript_path: file,
        cwd: ctx.cwd,
        hook_event_name: event,
        shows_notice: shows,
      }),
    );
    child.unref();
  } catch {}
}

export default function (pi) {
  pi.on("agent_settled", async (_event, ctx) => settle(ctx, "agent_settled"));
  pi.on("session_shutdown", async (_event, ctx) => settle(ctx, "session_shutdown"));
}
