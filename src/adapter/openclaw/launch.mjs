import { readFileSync, readdirSync, realpathSync, statSync } from "node:fs";
import { parseArgs } from "node:util";
import { pathToFileURL } from "node:url";

const { values } = parseArgs({ options: {
  root: { type: "string" }, cwd: { type: "string" },
  "session-id": { type: "string" }, message: { type: "string" },
  agent: { type: "string" }, json: { type: "boolean", default: false },
  timeout: { type: "string" },
} });
const root = realpathSync(values.root);
if (JSON.parse(readFileSync(root + "/package.json", "utf8")).version !== "2026.9.4") {
  throw new Error("OpenClaw launch requires the verified 2026.9.4 workspace API");
}
const cwd = realpathSync(values.cwd);
if (!statSync(cwd).isDirectory()) throw new Error("OpenClaw workspace is not a directory");
if (!values["session-id"] || !values.message) throw new Error("A session and message are required");

async function entry(prefix, exportText) {
  const candidates = readdirSync(root + "/dist").filter(name =>
    name.startsWith(prefix) && name.endsWith(".mjs") &&
    readFileSync(root + "/dist/" + name, "utf8").includes(exportText));
  if (candidates.length !== 1) throw new Error("OpenClaw launch API is unavailable or ambiguous");
  return import(pathToFileURL(root + "/dist/" + candidates[0]));
}

const { ensureCliExecutionBootstrap } = await entry("command-execution-startup-", "export { applyCliExecutionStartupPresentation, ensureCliExecutionBootstrap }");
const { agentCliCommand } = await entry("agent-via-gateway-", "export { agentCliCommand,");
const { requestExitAfterOneShotOutput, runCliWithExitFinalization } = await entry("one-shot-exit-", "export { exitCliAfterOutput, requestExitAfterOneShotOutput,");
const { a: closeCliResources, s: runCliDisposer } = await entry("one-shot-exit-", "export { closeCliResources as a,");
const { withCliProcessScope, withCliCommandCleanup } = await entry("runtime-cleanup-", "export { getCliPluginInvocationResources,");
const { defaultRuntime } = await import(pathToFileURL(root + "/dist/plugin-sdk/runtime-env.js"));
process.chdir(cwd);
process.env.OPENCLAW_SESSION_ID = values["session-id"];
await withCliProcessScope(() => runCliWithExitFinalization({
  runtime: defaultRuntime,
  run: () => withCliCommandCleanup(false, async cleanup => {
    const resources = cleanup?.pluginResources;
    try {
      await ensureCliExecutionBootstrap({
        runtime: defaultRuntime, commandPath: ["agent"],
        startupPolicy: {
          suppressDoctorStdout: values.json, skipConfigGuard: false,
          loadPlugins: true, pluginRegistry: { scope: "all" },
        },
      });
      await resources?.waitForRegistrations();
      // Both values are invocation facts; native configuration must not redirect project operations.
      const run = () => agentCliCommand({
        local: true, sessionId: values["session-id"], agent: values.agent,
        workspaceDir: cwd, cwd, message: values.message,
        json: values.json, timeout: values.timeout,
      }, defaultRuntime);
      await (resources ? resources.run(run) : run());
      await resources?.waitForRegistrations();
      requestExitAfterOneShotOutput(defaultRuntime);
    } finally {
      try {
        await closeCliResources(cleanup);
      } finally {
        if (resources) await runCliDisposer("plugin-registration-resources", () => resources.release());
        if (!process.stdin.isTTY) process.stdin.pause();
      }
    }
  }),
  onError: async error => {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  },
}));
