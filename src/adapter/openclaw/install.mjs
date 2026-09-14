// The native accessor owns transcript indexes and transaction publication.
import { readFileSync, readdirSync } from "node:fs";
import { pathToFileURL } from "node:url";

const root = process.argv[1];
const packageInfo = JSON.parse(readFileSync(root + "/package.json", "utf8"));
if (packageInfo.version !== "2026.9.4") {
  throw new Error("Native OpenClaw restoration requires the verified 2026.9.4 storage API");
}
const candidates = readdirSync(root + "/dist").filter(name => {
  if (!/^session-accessor-[^.]+\.mjs$/.test(name)) return false;
  const code = readFileSync(root + "/dist/" + name, "utf8");
  return code.includes("export {") && code.includes("replaceTranscriptEventsSync,");
});
if (candidates.length !== 1) throw new Error("OpenClaw native storage accessor is unavailable or ambiguous");
const api = await import(pathToFileURL(root + "/dist/" + candidates[0]));
const config = await import(pathToFileURL(root + "/dist/plugin-sdk/config-runtime.js"));
const input = JSON.parse(readFileSync(0, "utf8"));
const agentId = config.resolveDefaultAgentId(config.loadConfig());
const scope = { agentId, sessionId: input.id, sessionKey: `agent:${agentId}:explicit:${input.id}` };
const events = input.content.split("\n").filter(line => line.trim()).map(line => JSON.parse(line));
await api.withTranscriptWriteTransaction(scope, () => {
  if (api.loadSessionEntry(scope) || api.loadTranscriptEventsSync(scope).length !== 0) {
    throw new Error("OpenClaw session already exists; refusing to overwrite it");
  }
  api.replaceSessionEntrySync(scope, { sessionId: input.id, updatedAt: Date.now() });
  if (!api.replaceTranscriptEventsSync(scope, events)) {
    throw new Error("OpenClaw refused the native transcript; transaction rolled back");
  }
});
