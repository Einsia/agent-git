import { spawn } from "node:child_process";

function invoke(command, action, payload) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, ["hooks", action, "--runtime", "openclaw"], {
      cwd: payload.cwd,
      stdio: ["pipe", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(() => {
      child.kill("SIGTERM");
      reject(new Error("AgentGit hook timed out"));
    }, 60000);
    child.stdout.on("data", chunk => { stdout += chunk; });
    child.stderr.on("data", chunk => { if (stderr.length < 4096) stderr += chunk; });
    child.on("error", error => { clearTimeout(timer); reject(error); });
    child.stdin.on("error", error => { clearTimeout(timer); reject(error); });
    child.on("close", code => {
      clearTimeout(timer);
      if (code !== 0) return reject(new Error(`AgentGit ${action} failed: ${stderr.trim()}`));
      try { resolve(stdout.trim() ? JSON.parse(stdout) : null); }
      catch { reject(new Error("AgentGit hook returned invalid JSON")); }
    });
    child.stdin.end(JSON.stringify(payload));
  });
}

export default {
  id: "agit",
  name: "AgentGit",
  register(api) {
    const command = api.pluginConfig?.command;
    if (typeof command !== "string" || !command) throw new Error("AgentGit executable is not configured");
    const payload = context => ({
      session_id: context.sessionId,
      cwd: context.workspaceDir,
      source: "startup",
    });
    api.on("before_prompt_build", async (_event, context) => {
      if (!context.sessionId || !context.workspaceDir) return;
      const response = await invoke(command, "ingest", payload(context));
      if (typeof response?.context === "string") return { prependContext: response.context };
    }, { timeoutMs: 60000 });
    api.on("agent_end", async (_event, context) => {
      if (!context.sessionId || !context.workspaceDir) return;
      await invoke(command, "settle", payload(context));
    }, { timeoutMs: 60000 });
  },
};
