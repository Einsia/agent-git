// Pi's extension shows the notice agit prints for a turn it couldn't save, and nothing else: only
// after a run, only where Pi has a screen, and agit is told whether it does, so a notice is counted
// as told only where it was shown. An extension that showed agit's other output would put agit's
// diagnostics on the person's screen; one that read agit's output where Pi shows nothing, or told
// agit it showed it there, would spend the conversation's one notice of that failure on no one.
import assert from "node:assert/strict";
import { test } from "node:test";
import { chmodSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

// Waits, within a bound, for `done` to hold.
async function until(what, done) {
  const deadline = Date.now() + 20_000;
  while (!done()) {
    assert.ok(Date.now() < deadline, `timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

// The extension as `agit setup` writes it, running a stand-in for agit that keeps the payload it
// was handed and prints AGIT_STANDIN_OUT.
async function extension(dir) {
  const agit = join(dir, "agit");
  writeFileSync(
    agit,
    '#!/bin/sh\ncat > "$AGIT_STANDIN_DIR/payload-$$"\nprintf \'%s\' "$AGIT_STANDIN_OUT"\n: > "$AGIT_STANDIN_DIR/done-$$"\n',
  );
  chmodSync(agit, 0o755);
  const source = readFileSync(join(here, "pi.mjs"), "utf8")
    .replaceAll("__AGIT_EXE__", agit)
    .replaceAll("__AGIT_PLUGIN_VERSION__", "test");
  const file = join(dir, "index.mjs");
  writeFileSync(file, source);
  const handlers = new Map();
  (await import(pathToFileURL(file).href)).default({
    on: (name, handler) => handlers.set(name, handler),
  });
  return handlers;
}

test("the extension shows agit's notice once a run ends where Pi has a screen, and nothing else", {
  skip: process.platform === "win32",
}, async () => {
  const dir = mkdtempSync(join(tmpdir(), "agit-pi-extension-"));
  process.env.AGIT_STANDIN_DIR = dir;
  const handlers = await extension(dir);
  const notes = [];
  const context = (hasUI) => ({
    cwd: "/work",
    hasUI,
    sessionManager: { getSessionFile: () => "/work/session.jsonl", getSessionId: () => "s1" },
    ui: { notify: (message, type) => notes.push([message, type]) },
  });
  const runs = () => readdirSync(dir).filter((name) => name.startsWith("done-")).length;
  const payloads = () =>
    readdirSync(dir)
      .filter((name) => name.startsWith("payload-"))
      .map((name) => JSON.parse(readFileSync(join(dir, name), "utf8")));

  // Where Pi shows nothing, and when a session ends, agit's output is not read.
  process.env.AGIT_STANDIN_OUT = '{"notice":"agit: unseen"}\n';
  await handlers.get("agent_settled")({}, context(false));
  await until("the run without a screen", () => runs() === 1);
  await handlers.get("session_shutdown")({}, context(true));
  await until("the session's end", () => runs() === 2);
  assert.deepEqual(
    payloads()
      .map((payload) => [payload.hook_event_name, payload.shows_notice])
      .sort(),
    [
      ["agent_settled", false],
      ["session_shutdown", false],
    ],
  );

  // After a run on a screen, the notice alone is shown, as a warning.
  process.env.AGIT_STANDIN_OUT = '{"systemMessage":"agit: other"}\nnot json\n{"notice":"agit: shown"}\n';
  await handlers.get("agent_settled")({}, context(true));
  await until("the notice", () => notes.length > 0);
  await until("the run on a screen", () => runs() === 3);
  assert.deepEqual(notes, [["agit: shown", "warning"]]);
  assert.ok(
    payloads().some(
      (payload) =>
        payload.hook_event_name === "agent_settled" &&
        payload.shows_notice === true &&
        payload.session_id === "s1" &&
        payload.transcript_path === "/work/session.jsonl",
    ),
  );
});
