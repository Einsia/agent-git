import assert from "node:assert/strict";
import { test } from "node:test";

const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};
const flush = () => new Promise(resolve => setImmediate(resolve));

async function fixture() {
  const hooks = new Map();
  const calls = [];
  const requests = [];
  const replies = [];
  const claims = new Map();
  const pages = new Map();
  let acknowledged = 0;
  let session = "a";
  let lease = { known: true, state: null, notice: null };
  let leaseFails = false;
  let refused = false;
  const hookRuns = [];
  // Claude Code appends a terminal's first records (its session-start hooks') right after it
  // loads a conversation; the fixture does so before the first poll or prompt of each session.
  let startupRows = true;
  let appendedFor = null;
  const hookArgs = [];
  const api = {
    session: { id: async () => session, cwd: async () => "/project", model: async () => "native-model" },
    clock: { every() {} },
    prompt: {
      submit: async value => { calls.push(["prompt", value]); return { text: value.text }; },
      fill: async value => { calls.push(["fill", value]); return {}; },
    },
    ui: { status: value => { calls.push(["status", value]); },
      toast: value => { calls.push(["toast", value]); } },
    turn: { abort: async value => { calls.push(["abort", value]); } },
  };
  const request = async ($, path, body) => {
    requests.push({ path, body });
    if (path === "/claim") {
      const key = body.generation + ":" + body.id;
      const claimed = !claims.has(key);
      if (claimed) claims.set(key, null);
      return { generation: body.generation, id: body.id, claimed, result: claims.get(key) };
    }
    for (const result of body.results) claims.set(body.generation + ":" + result.id, result);
    const reply = replies.shift();
    return reply ? reply(body) : { generation: body.generation, ack_seq: body.events.at(-1)?.seq ?? 0 };
  };
  const env = {};
  api.env = { get: async name => env[name] ?? null };
  api.process = { run: async (argv, init) => {
    if (argv[2] === "native-events") {
      const request = JSON.parse(init.stdin);
      if (request.events?.length) {
        const last = request.events.at(-1).seq;
        if (last > acknowledged) pages.set(request.events[0].seq, request.events);
      }
      acknowledged = Math.max(acknowledged, request.ack_seq ?? 0);
      for (const [first, events] of pages) if (events.at(-1).seq <= acknowledged) pages.delete(first);
      const events = [...pages].sort((a, b) => a[0] - b[0])[0]?.[1] ?? [];
      return { exitCode: 0, stdout: JSON.stringify({ events: events.filter(event => event.seq > acknowledged) }) };
    }
    hookRuns.push(argv[2]);
    hookArgs.push(argv);
    if (argv[2] === "native-lease") {
      return leaseFails ? { exitCode: 1, stdout: "" } : { exitCode: 0, stdout: JSON.stringify({ lease }) };
    }
    if (refused) return { exitCode: 0, stdout: JSON.stringify({ refused: "not_current_writer" }) };
    return { exitCode: 0, stdout: JSON.stringify({
    registration: { session: argv[4], generation: argv[6] }, descriptor: "/owned-descriptor",
    lease: leaseFails ? { known: false } : lease,
    }) };
  } };
  api.fs = { read: async () => JSON.stringify({ socket: "/owned-socket", token: "owned-token" }) };
  api.http = { fetch: async (url, init) => ({
    ok: true, text: JSON.stringify(await request(api, new URL(url).pathname, JSON.parse(init.body))),
  }) };
  const control = await import("./plugin/hooks/control.js?instance=" + crypto.randomUUID());
  control.registerControl((name, fn) => hooks.set(name, fn));
  const next = async () => ({});
  next.signal = new AbortController().signal;
  const append = (uuid, input = {}) => hooks.get("session.append")(api,
    { uuid, door: "hook-context", message: { type: "attachment", content: [] }, ...input }, next);
  const startup = async () => {
    if (!startupRows || appendedFor === session) return;
    appendedFor = session;
    await append("00000000-0000-4000-8000-" + String(Math.floor(Math.random() * 1e12)).padStart(12, "0"));
  };
  return {
    calls, requests, replies, control, api, hooks, append,
    fire: async (name, input = {}, downstream = next, context = api) => {
      if (name === "prompt.submit") await startup();
      const result = await hooks.get(name)(context, input, downstream);
      if (name === "session.end") appendedFor = null;
      return result;
    },
    setSession: value => { session = value; appendedFor = null; },
    setStartupRows: value => { startupRows = value; },
    hookArgs,
    setLease: value => { lease = value; },
    setLeaseFails: value => { leaseFails = value; },
    setRefused: value => { refused = value; },
    setEnv: (name, value) => { env[name] = value; },
    hooksRun: hookRuns,
    poll: async () => { await startup(); return control.poll(api); },
    latest: () => requests.filter(r => r.path === "/poll").at(-1).body,
  };
}

test("session ABA, lost replies, and stale turn IDs never repeat or retarget native effects", async () => {
  const f = await fixture();
  // Claude Code passes user plugins over on classic events where an organization's security
  // default is seated, so a classic handler here would silently never run.
  assert.deepEqual([...f.hooks.keys()].filter(name => name.startsWith("classic.")), []);
  await f.fire("session.start");
  const delayed = deferred();
  f.replies.push(() => delayed.promise);
  const polling = f.poll();
  await flush();
  const first = f.latest();
  await f.fire("session.end", { sessionId: "a", reason: "resume" });
  f.setSession("b");
  await f.fire("session.end", { sessionId: "b", reason: "resume" });
  f.setSession("a");
  const prompt = { ...first, id: "send", method: "prompt", text: "One native message" };
  delayed.resolve({ generation: first.generation, commands: [prompt] });
  await polling;
  await flush();
  assert.deepEqual(f.calls, [], "an old response must not enter a re-opened native session");

  await f.poll();
  const current = f.latest();
  assert.notEqual(first.generation, current.generation);
  const command = { ...prompt, generation: current.generation };
  f.replies.push(body => ({ generation: body.generation, commands: [command, command] }));
  await f.poll();
  await flush();
  assert.equal(f.calls.length, 1);
  f.replies.push(() => { throw new Error("lost receipt"); });
  await f.poll();
  f.replies.push(body => ({ generation: body.generation, ack_results: ["send"], commands: [command] }));
  await f.poll();
  await flush();
  assert.deepEqual(f.latest().results, [{ id: "send", outcome: "accepted" }]);
  assert.equal(f.calls.length, 1, "receipt reconciliation cannot replay a native prompt");

  await f.fire("turn.start", { turnId: "first-turn" });
  await f.fire("turn.complete", { turnId: "first-turn", reason: "completed", durationMs: 1 });
  await f.fire("turn.start", { turnId: "second-turn" });
  f.replies.push(body => ({ generation: body.generation, commands: [
    { ...command, id: "stop-old", method: "abort", turnId: "first-turn" },
    { ...command, id: "stop-current", method: "abort", turnId: "second-turn" },
  ] }));
  await f.poll();
  await flush();
  assert.deepEqual(f.calls.at(-1), ["abort", { turnId: "second-turn" }]);
  assert.equal(f.calls.length, 2, "only the exact active native turn can be interrupted");
});

test("offline lifecycle overflow replays every turn in bounded batches and resumes control after lost receipts", async () => {
  const f = await fixture();
  const run = f.api.process.run;
  let loseDiskReceipt = true;
  f.api.process.run = async (argv, init) => {
    const reply = await run(argv, init);
    if (argv[2] === "native-events" && JSON.parse(init.stdin).events?.length && loseDiskReceipt) {
      loseDiskReceipt = false;
      throw new Error("lost disk receipt");
    }
    return reply;
  };
  await f.fire("session.start");
  for (let index = 0; index < 1024; index++) {
    await f.fire("turn.start", { turnId: "turn-" + index });
    await f.fire("tool.call", { tool_use_id: "tool-" + index, tool: "Read" });
    await f.fire("turn.complete", { turnId: "turn-" + index, reason: "answer", answer: "Done", durationMs: 1.25 });
  }
  const received = new Map();
  const receive = body => {
    assert.ok(body.events.length <= 1024, "offline history must not produce an oversized poll");
    assert.equal(body.latest_seq, 4096, "attachment needs the live state boundary, not only the current page");
    for (const event of body.events.slice(0, 512)) received.set(event.seq, event);
    return { generation: body.generation, ack_seq: Math.max(...received.keys()) };
  };
  f.replies.push(body => { receive(body); throw new Error("lost poll receipt"); });
  await f.poll();
  for (let attempt = 0; received.size < 4096 && attempt < 20; attempt++) {
    f.replies.push(receive);
    await f.poll();
  }
  assert.deepEqual([...received.keys()], Array.from({ length: 4096 }, (_, index) => index + 1));
  assert.deepEqual([...received.values()].filter(event => event.kind === "turn_completed")
    .map(event => event.turn), Array.from({ length: 1024 }, (_, index) => "turn-" + index));
  assert.equal(received.get(4).duration_ms, 1);
  const readingEmptyPage = deferred();
  const releasePage = deferred();
  const currentRun = f.api.process.run;
  f.api.process.run = async (argv, init) => {
    const reply = await currentRun(argv, init);
    if (argv[2] === "native-events" && Object.keys(JSON.parse(init.stdin)).length === 0) {
      readingEmptyPage.resolve();
      await releasePage.promise;
    }
    return reply;
  };
  const reading = f.poll();
  await readingEmptyPage.promise;
  const starting = f.fire("turn.start", { turnId: "after-offline" });
  await flush();
  releasePage.resolve();
  await Promise.all([reading, starting]);
  assert.equal(f.latest().last_seq, 4096, "an empty page cannot claim a concurrent queued event");
  assert.equal(f.latest().latest_seq, 4097, "attachment must include the concurrently observed live turn");
  await f.fire("turn.complete", { turnId: "after-offline", reason: "answer", answer: "Done" });
  await f.poll();
  f.replies.push(body => ({ generation: body.generation, ack_seq: body.last_seq,
    commands: [{ ...body, id: "after-reconnect", method: "prompt", text: "Continue" }] }));
  await f.poll();
  await flush();
  assert.equal(f.latest().saturated, false);
  assert.deepEqual(f.latest().events, []);
  assert.equal(f.calls.filter(call => call[0] === "prompt").length, 1);
});

test("acknowledged operations do not exhaust a long session or allow old effects to repeat", async () => {
  const f = await fixture();
  await f.fire("session.start");
  await f.poll();
  const target = f.latest();
  const command = index => ({ ...target, id: "prompt-" + index, method: "prompt", text: "Continue" });
  for (let start = 0; start < 4224; start += 32) {
    f.replies.push(body => ({ generation: body.generation,
      ack_results: body.results.map(result => result.id),
      commands: Array.from({ length: 32 }, (_, offset) => command(start + offset)),
    }));
    await f.poll();
    await flush();
  }
  assert.equal(f.calls.length, 4224);
  assert.equal(f.latest().saturated, false);
  f.replies.push(body => ({ generation: body.generation,
    ack_results: body.results.map(result => result.id), commands: [command(0)],
  }));
  await f.poll();
  await flush();
  await f.poll();
  assert.equal(f.calls.length, 4224, "an evicted identity still needs its durable native claim");
  assert.deepEqual(f.latest().results, [{ id: "prompt-0", outcome: "accepted" }]);
});

test("model changes remain pending until native confirmation and honor live policy locks", async () => {
  const f = await fixture();
  const row = { key: "model", kind: "choice", value: "sonnet", options: ["default", "sonnet", "haiku"],
    provider: { plugin: "engine", tier: "core" }, isLocked: false };
  const receipt = deferred();
  f.api.prompt.submit = async value => {
    f.calls.push(["prompt", value, row.value]);
    return { text: value.text };
  };
  f.api.config = { list: async () => [row], set: async value => {
    f.calls.push(["model", value]);
    await receipt.promise;
    row.value = value.value;
    return { value: row.value };
  } };
  await f.fire("session.start");
  await f.poll();
  const command = { ...f.latest(), method: "model", model: "haiku", id: "change" };
  f.replies.push(body => ({ generation: body.generation, commands: [command, command] }));
  await f.poll();
  await flush();
  await f.poll();
  assert.equal(f.latest().model_controls.pending, "change");
  assert.equal(f.latest().model_controls.selected, "sonnet");
  assert.deepEqual(f.latest().results, [], "a queued write is not a native acknowledgement");
  f.replies.push(body => ({ generation: body.generation,
    commands: [{ ...command, id: "overlap", model: "default" },
      { ...command, id: "after-model", method: "prompt", text: "Use the selected model" }] }));
  await f.poll();
  await flush();
  assert.equal(f.calls.length, 1, "a second controller cannot overlap an unresolved native write");
  receipt.resolve();
  await flush();
  await f.poll();
  assert.equal(f.latest().model_controls.selected, "haiku");
  assert.equal(f.latest().model_controls.pending, null);
  assert.deepEqual(f.calls[1], ["prompt", { text: "Use the selected model", asUser: true }, "haiku"]);
  assert.ok(f.latest().results.some(result => result.id === "change" && result.outcome === "applied"));
  row.isLocked = true;
  f.replies.push(body => ({ generation: body.generation,
    commands: [{ ...command, id: "locked", model: "sonnet" }] }));
  await f.poll();
  await flush();
  await f.poll();
  assert.equal(f.latest().model_controls.locked, true);
  assert.equal(f.calls.filter(call => call[0] === "model").length, 1,
    "policy is rechecked at execution, not inferred from an old catalog");
  assert.ok(f.latest().results.some(result => result.id === "locked" && result.outcome === "not_sent"));
});

test("native tool and compaction activity stays visible until completion without copying content", async () => {
  const f = await fixture();
  await f.fire("session.start");
  const tool = deferred();
  const compact = deferred();
  await f.fire("turn.start", { turnId: "compacting-turn" });
  const runningTool = f.fire("tool.call", { tool: "Read", tool_use_id: "read-1" }, () => tool.promise);
  const compacting = f.fire("session.compact", { messages: [{ text: "Private transcript content" }] }, () => compact.promise);
  await f.poll();
  assert.deepEqual(f.latest().tools, [{ id: "read-1", tool: "Read" }]);
  assert.equal(f.latest().compacting, true);
  assert.equal(JSON.stringify(f.latest()).includes("Private transcript content"), false);
  assert.equal(f.latest().turn, "compacting-turn");
  assert.equal(f.latest().compacting, true);
  const nativeToolResult = { result: "Private tool content" };
  tool.resolve(nativeToolResult);
  assert.equal(await runningTool, nativeToolResult);
  compact.resolve({ messages: [{ text: "Private summary" }] });
  await compacting;
  await f.poll();
  assert.deepEqual(f.latest().tools, []);
  assert.equal(f.latest().compacting, false);
  assert.equal(JSON.stringify(f.latest()).includes("Private"), false);
});


// Claude Code holds a prompt submitted during a turn until that turn ends, and only then settles
// the submission. A receipt that waited for it would leave the sender of a message to a terminal
// running a long command with no answer for the whole turn, and then told it was not sent while
// it arrives later. With no turn running, the prompt enters at once and its own checks decide,
// so the receipt still waits for them: a receipt sent early there would report a prompt the
// terminal holds back as accepted.
test("a prompt for a running turn is acknowledged as queued at once, and an idle one only once it enters", async () => {
  const f = await fixture();
  const wait = ms => new Promise(resolve => setTimeout(resolve, ms));
  const entered = deferred();
  f.api.prompt.submit = value => { f.calls.push(["prompt", value]); return entered.promise; };
  await f.fire("session.start");
  await f.fire("turn.start", { turnId: "long-turn" });
  await f.poll();
  const target = f.latest();
  const command = { ...target, id: "while-running", method: "prompt", text: "a note for the running turn" };
  f.replies.push(body => ({ generation: body.generation, commands: [command] }));
  await f.poll();
  await wait(400);
  await f.poll();
  assert.deepEqual(f.latest().results, [{ id: "while-running", outcome: "accepted" }]);
  assert.deepEqual(f.calls.filter(call => call[0] === "prompt").length, 1);

  f.api.prompt.submit = async () => { throw new Error("the submission was refused"); };
  f.replies.push(body => ({ generation: body.generation, ack_results: ["while-running"],
    commands: [{ ...command, id: "refused" }] }));
  await f.poll();
  await wait(50);
  await f.poll();
  assert.deepEqual(f.latest().results, [{ id: "refused", outcome: "unknown" }],
    "a submission that fails at once is not reported as queued");

  await f.fire("turn.complete", { turnId: "long-turn", reason: "completed", durationMs: 1 });
  entered.resolve({ text: command.text });
  f.api.prompt.submit = () => wait(400).then(() => ({ drop: "held back at this terminal" }));
  f.replies.push(body => ({ generation: body.generation, ack_results: ["refused"],
    commands: [{ ...command, id: "idle" }] }));
  await f.poll();
  await wait(300);
  await f.poll();
  assert.deepEqual(f.latest().results, [], "an idle prompt's receipt waits for it to enter");
  await wait(200);
  await f.poll();
  assert.deepEqual(f.latest().results, [{ id: "idle", outcome: "not_sent" }]);
});

test("native slash commands keep their receipt and activity without creating a model prompt", async () => {
  const f = await fixture();
  const completed = deferred();
  f.api.command = {
    list: async () => [{ name: "compact", description: "Compact context" },
      { name: "plan", description: "Change permissions" }],
    run: async value => {
      f.calls.push(["command", value]);
      return f.fire("session.compact", {}, async () => {
        await completed.promise;
        return { text: "The native transcript owns this output" };
      });
    },
  };
  await f.fire("session.start");
  await f.poll();
  const target = f.latest();
  assert.deepEqual(target.commands.map(command => command.name), ["compact"]);
  const command = { ...target, id: "compact", method: "prompt", text: "/compact keep the plan" };
  f.replies.push(body => ({ generation: body.generation, commands: [command, command,
    { ...command, id: "unsupported", text: "/plan" }] }));
  await f.poll();
  await flush();
  await f.poll();
  assert.deepEqual(f.calls, [["command", { command: "compact", args: "keep the plan" }]]);
  assert.equal(f.latest().compacting, true);
  assert.deepEqual(f.latest().results, [{ id: "unsupported", outcome: "not_sent" }]);
  completed.resolve();
  await flush();
  await f.poll();
  assert.equal(f.latest().compacting, false);
  assert.ok(f.latest().results.some(result => result.id === "compact"
    && result.outcome === "command_completed"));
  assert.equal(f.latest().turn, null, "a command receipt must not invent a model turn");
  assert.ok(!JSON.stringify(f.latest().results).includes("transcript owns"));
});

// Runs `step` as if `ms` had passed since the test started.
async function later(ms, step) {
  const now = Date.now;
  const start = now();
  Date.now = () => start + ms;
  try {
    return await step();
  } finally {
    Date.now = now;
  }
}

const statuses = f => f.calls.filter(c => c[0] === "status").map(c => c[1]);

function typing() {
  const sent = [];
  const downstream = async e => { sent.push(e.text); return { text: e.text }; };
  downstream.signal = new AbortController().signal;
  return { sent, downstream };
}

const typed = { text: "my question", origin: { kind: "composer" }, wait: false };
const releasing = "A turn started from the web is still running for this conversation.";

test("a prompt typed while the terminal must wait is neither sent nor lost, and goes through after the grant", async () => {
  const f = await fixture();
  f.setLease({ known: true, state: "releasing", notice: releasing });
  await f.fire("session.start");
  await f.poll();
  assert.deepEqual(statuses(f), [releasing]);

  const { sent, downstream } = typing();
  assert.deepEqual(await f.fire("prompt.submit", typed, downstream), { drop: releasing });
  assert.deepEqual(sent, [], "a held prompt is not sent beside the headless turn");
  assert.deepEqual(f.calls.filter(c => c[0] === "fill"), [],
    "Claude Code keeps a dropped prompt in the box; filling it again would double it");

  // Nothing continues the conversation here while it waits, whatever its origin, the web input
  // this plugin carries included; slash commands run in the terminal itself.
  await f.fire("prompt.submit", { text: "/compact", command: { name: "compact", args: "" },
    origin: { kind: "composer" } }, downstream);
  for (const origin of [{ kind: "plugin", name: "agit-native-control" }, { kind: "unclassified" },
    { kind: "plugin", name: "another" }, undefined]) {
    assert.deepEqual(await f.fire("prompt.submit", { text: "held", origin }, downstream),
      { drop: releasing });
  }
  assert.deepEqual(sent, ["/compact"]);

  // The daemon grants the lease: the status clears and the resent prompt goes through, and the
  // re-check asked for the lease without registering again.
  const registrations = f.hooksRun.filter(name => name === "native-control").length;
  f.setLease({ known: true, state: "held", notice: null });
  await later(5000, () => f.poll());
  assert.equal(statuses(f).at(-1), undefined);
  assert.equal(f.hooksRun.filter(name => name === "native-control").length, registrations);
  assert.ok(f.hooksRun.includes("native-lease"));
  await f.fire("prompt.submit", typed, downstream);
  assert.deepEqual(sent, ["/compact", "my question"]);
});

test("a prompt typed before the daemon first answers waits for that answer", async () => {
  const held = await fixture();
  held.setLease({ known: true, state: "releasing", notice: releasing });
  await held.fire("session.start");
  const first = typing();
  assert.deepEqual(await held.fire("prompt.submit", typed, first.downstream), { drop: releasing },
    "no poll has run yet, and the prompt still does not reach the model beside the headless turn");
  assert.deepEqual(first.sent, []);

  const free = await fixture();
  await free.fire("session.start");
  const second = typing();
  await free.fire("prompt.submit", typed, second.downstream);
  assert.deepEqual(second.sent, ["my question"], "a terminal nothing holds back types at once");
});

// The rule: only a definite answer holds a prompt. However the daemon check fails to answer, what
// the person typed goes ahead: it must never be lost to an answer nobody has.
test("an answer agit could not get never holds a prompt", async () => {
  const events = { exitCode: 0, stdout: JSON.stringify({ events: [] }) };
  const unknown = { exitCode: 0, stdout: JSON.stringify({ lease: { known: false } }) };
  const failures = {
    // A daemon that accepts and never answers: the registration never returns.
    hangs: () => new Promise(() => {}),
    // The command cannot even start.
    throws: async () => { throw new Error("spawn failed"); },
    // Claude Code aborts a process.run past its budget, after the prompt began waiting.
    "times out": () => new Promise((_, reject) =>
      setTimeout(() => reject(new Error("still running after 5000ms")), 2500)),
    // The command ran and failed: a refused registration, or a daemon past its bound.
    "fails": async () => ({ exitCode: 1, stdout: "" }),
    // The command ran and has no answer to give.
    "says it does not know": async () => unknown,
  };
  for (const [failure, run] of Object.entries(failures)) {
    const f = await fixture();
    f.api.process.run = async (argv) => argv[2] === "native-events" ? events : run();
    await f.fire("session.start");
    const { sent, downstream } = typing();
    const answer = await f.fire("prompt.submit", typed, downstream);
    assert.equal(answer?.drop, undefined, `the check ${failure}`);
    assert.deepEqual(sent, ["my question"], `the check ${failure}: the prompt was sent`);
  }

  // The check itself breaks before it can ask.
  const f = await fixture();
  await f.fire("session.start");
  f.api.session.id = async () => { throw new Error("no session"); };
  const { sent, downstream } = typing();
  assert.equal((await f.fire("prompt.submit", typed, downstream))?.drop, undefined);
  assert.deepEqual(sent, ["my question"]);
});

// Claude Code's reported folder follows a `cd`, while the session stays registered with the folder
// it started in: this is the same conversation, and a new one is told apart by its id.
test("a cd in the terminal is not a new conversation", async () => {
  const f = await fixture();
  await f.fire("session.start");
  await f.poll();
  const registrations = () => f.hooksRun.filter(name => name === "native-control").length;
  const before = registrations();
  assert.ok(before > 0);
  f.api.session.cwd = async () => "/project/sub";
  await later(5000, () => f.poll());
  assert.equal(registrations(), before, "it does not register again from the new folder");
  const { sent, downstream } = typing();
  await later(5001, () => f.fire("prompt.submit", typed, downstream));
  assert.deepEqual(sent, ["my question"]);
  assert.deepEqual(statuses(f).filter(status => /Checking with agit/.test(status ?? "")), []);

  f.setSession("b");
  await later(10000, () => f.poll());
  assert.ok(registrations() > before, "another conversation registers, whatever its folder");
});

const olderBranch = "This terminal continues an older branch of this conversation, without the "
  + "turn sent from the web. Exit and run `claude --resume a` now, before typing here: a message "
  + "typed in this terminal keeps it on an older branch of the conversation.";
const continueAnyway = "Not sent: this terminal continues an older branch of this conversation, "
  + "without the turn sent from the web. Submit the same message again to send it anyway: this "
  + "continues without the turn sent from the web; that turn stays in the history.";

// A held-back prompt leaves a record on the terminal's branch, and Claude Code resumes the
// branch with the newest one: the terminal must be told to resume before anything is typed.
test("a confirmed stale terminal is told at once to resume, before anything is typed", async () => {
  const f = await fixture();
  f.setLease({ known: true, state: "stale", notice: olderBranch });
  await f.fire("session.start");
  await f.poll();
  assert.deepEqual(statuses(f), [olderBranch]);
  assert.deepEqual(f.calls.filter(c => c[0] === "toast"), [["toast", olderBranch]]);
  await later(5000, () => f.poll());
  assert.equal(f.calls.filter(c => c[0] === "toast").length, 1, "told once");
});

// Once a prompt was held back, resuming may land on this terminal's branch again. The person
// may continue here knowingly: the same message submitted again at the prompt goes through,
// exactly as typed, with nothing added; no daemon silence releases the hold otherwise.
test("on an older branch, only the same message submitted again at the prompt goes through", async () => {
  const f = await fixture();
  f.setLease({ known: true, state: "stale", notice: olderBranch });
  await f.fire("session.start");
  await f.poll();
  const { sent, downstream } = typing();
  assert.deepEqual(await f.fire("prompt.submit", typed, downstream), { drop: continueAnyway });
  assert.equal(statuses(f).at(-1), continueAnyway, "resuming is no longer offered");
  const other = { ...typed, text: "another question" };
  assert.deepEqual(await f.fire("prompt.submit", other, downstream), { drop: continueAnyway },
    "a different message is a new offer");
  const web = { ...other, origin: { kind: "remote-control" } };
  assert.deepEqual(await f.fire("prompt.submit", web, downstream), { drop: continueAnyway },
    "input that is not typed at this prompt never continues");
  assert.deepEqual(sent, []);

  // The daemon stops answering for far longer than the unreachable bound: still held.
  f.setLeaseFails(true);
  for (const at of [40000, 3600000]) {
    await later(at, () => f.poll());
    const held = await later(at + 1, () => f.fire("prompt.submit", { ...typed, text: "x" + at },
      downstream));
    assert.deepEqual(held, { drop: continueAnyway }, `no timer releases a stale hold (${at} ms)`);
  }
  f.setLeaseFails(false);

  const again = { ...typed, text: "my final question" };
  assert.deepEqual(await f.fire("prompt.submit", again, downstream), { drop: continueAnyway });
  await f.fire("prompt.submit", again, downstream);
  assert.deepEqual(sent, ["my final question"], "sent exactly as typed");
  await f.fire("prompt.submit", typed, downstream);
  assert.deepEqual(sent, ["my final question", "my question"], "the person chose this branch");
  assert.deepEqual(f.calls.filter(c => c[0] === "prompt"), [], "agit submits nothing itself");
  assert.equal(statuses(f).at(-1), undefined);
});

// The daemon judges a terminal by the branch it continues: the plugin names it by the id of the
// newest record this process appended, and waits for its first record before asking.
test("a terminal names its branch by record id, and the headless writer reports its turn's last record", async () => {
  const f = await fixture();
  f.setStartupRows(false);
  await f.fire("session.start", { isInteractive: true });
  await f.poll();
  assert.ok(!f.hooksRun.includes("native-control"), "no lease is asked before the first record");
  const first = "11111111-1111-4111-8111-111111111111";
  await f.append(first);
  f.setLease({ known: true, state: "stale", notice: olderBranch });
  await f.poll();
  assert.deepEqual(f.hookArgs.find(argv => argv[2] === "native-control")?.slice(-2),
    ["--leaf", first]);
  const subagent = "22222222-2222-4222-8222-222222222222";
  await f.append(subagent, { agentId: "helper" });
  await later(5000, () => f.poll());
  assert.deepEqual(f.hookArgs.filter(argv => argv[2] === "native-lease").at(-1)?.slice(-2),
    ["--leaf", first], "a subagent's records are not the conversation's branch");

  // Without any record, it asks after a bound and is held back until it names one.
  const quiet = await fixture();
  quiet.setStartupRows(false);
  await quiet.fire("session.start", { isInteractive: true });
  await later(2500, () => quiet.poll());
  assert.deepEqual(quiet.hookArgs.find(argv => argv[2] === "native-control")?.slice(3, 5),
    ["--session", "a"]);
  assert.ok(!quiet.hookArgs.some(argv => argv.includes("--leaf")));

  // The daemon's headless writer reports its last record before its turn's end goes on.
  const writer = await fixture();
  writer.setStartupRows(false);
  await writer.fire("session.start", { isInteractive: false });
  const last = "33333333-3333-4333-8333-333333333333";
  await writer.append("44444444-4444-4444-8444-444444444444");
  await writer.append(last);
  await writer.fire("turn.start", { turnId: "t" });
  let reported = null;
  await writer.fire("turn.complete", { turnId: "t", reason: "completed", durationMs: 1 }, async () => {
    reported = writer.hookArgs.find(argv => argv[2] === "native-lease");
    return {};
  });
  assert.deepEqual(reported, ["agit", "hooks", "native-lease", "--session", "a", "--leaf", last]);
});

// Claude Code records a slash command's run and its text on the conversation's message chain and
// sends them to the model with the next prompt. The plugin therefore registers no command and
// answers none, through any event it handles; adding one must fail here first.
test("the plugin registers no slash command", async () => {
  const f = await fixture();
  const registered = [];
  f.api.command = { register: async value => { registered.push(value); return {}; },
    list: async () => [], run: async () => ({}) };
  f.setLease({ known: true, state: "stale", notice: olderBranch });
  await f.fire("session.start");
  await f.poll();
  await f.fire("prompt.submit", typed, async e => ({ text: e.text }));
  await later(60000, () => f.poll());
  assert.deepEqual(registered, []);
  assert.ok(!f.hooks.has("command.run"));
  const source = await import("node:fs/promises")
    .then(fs => fs.readFile(new URL("./plugin/hooks/control.js", import.meta.url), "utf8"));
  assert.doesNotMatch(source, /command\.register|["']command\.run["']/);
});

// After a stale hold, the person reloads: a new terminal process. If the daemon is unreachable
// then, nothing says this terminal is behind, so nothing holds it. agit puts nothing on the conversation's message chain on the way: it
// registers no command and submits no prompt of its own; a held prompt is only refused.
test("a reload with the daemon unreachable stays writable, says so, and agit adds nothing to the conversation", async () => {
  const f = await fixture();
  const registered = [];
  f.api.command = { register: async value => { registered.push(value); return {}; } };
  f.setLeaseFails(true);
  await f.fire("session.start");
  const { sent, downstream } = typing();
  await later(1000, () => f.fire("prompt.submit", typed, downstream));
  assert.deepEqual(sent, ["my question"], "nothing definite holds it");
  await later(40000, () => f.poll());
  assert.match(statuses(f).at(-1), /couldn't check/);

  assert.deepEqual(registered, [], "agit registers no slash command");
  assert.ok(!f.hooks.has("command.run"), "agit answers no slash command");
  assert.deepEqual(f.calls.filter(c => c[0] === "prompt"), [], "agit submits no prompt of its own");
});

test("a release agit could not confirm holds the terminal with the way out, and no timer ends it", async () => {
  const f = await fixture();
  const failed = "agit could not confirm that the web's process for this conversation has stopped, "
    + "so this terminal cannot take over yet. Make sure no `claude -p` for it is still running, then "
    + "restart agit's background service (`agit rc stop`, then `agit rc start`), exit and run "
    + "`claude --resume a` before typing here.";
  f.setLease({ known: true, state: "release_failed", notice: failed });
  await f.fire("session.start");
  await f.poll();
  assert.equal(statuses(f).at(-1), failed, "the way out shows in the terminal");
  const { sent, downstream } = typing();
  for (const at of [1000, 40000, 3600000]) {
    await later(at, () => f.poll());
    assert.deepEqual(await later(at + 1, () => f.fire("prompt.submit", typed, downstream)),
      { drop: failed });
  }
  assert.deepEqual(sent, []);
});

test("a lease query that fails keeps the hold, surfaced and bounded, never stuck", async () => {
  const f = await fixture();
  f.setLease({ known: true, state: "releasing", notice: releasing });
  await f.fire("session.start");
  await f.poll();
  const { sent, downstream } = typing();

  // The daemon stops answering: the hold stands rather than reading the silence as a grant.
  f.setLeaseFails(true);
  f.setLease({ known: true, state: null, notice: null });
  await later(5000, () => f.poll());
  assert.deepEqual(await later(6000, () => f.fire("prompt.submit", typed, downstream)),
    { drop: releasing });

  // Still unreachable long after: the hold ends and the status says why.
  await later(40000, () => f.poll());
  assert.match(statuses(f).at(-1), /couldn't check/);
  await later(41000, () => f.fire("prompt.submit", typed, downstream));
  assert.deepEqual(sent, ["my question"]);
});

// While the daemon's headless writer still runs the conversation, agit refuses this terminal's
// registration with a definite answer. That is not an outage: the terminal holds its input, with
// the reason shown, for as long as the refusal lasts, and no timer ends it. A plugin that treated
// the refusal like an unreachable daemon let typed input out after 30 s, beside the headless turn.
test("a terminal agit refuses as writer holds its input past the release window until it is granted", async () => {
  const f = await fixture();
  f.setRefused(true);
  await f.fire("session.start");
  await f.poll();
  const waiting = /Not sent yet: another process still writes this conversation\. Your input waits here until that process ends or hands the conversation to this terminal\./;
  assert.match(statuses(f).at(-1), waiting);
  const { sent, downstream } = typing();
  for (const at of [1000, 40000, 3600000]) {
    await later(at, () => f.poll());
    assert.match(statuses(f).at(-1), waiting);
    assert.match((await later(at + 1, () => f.fire("prompt.submit", typed, downstream))).drop,
      waiting);
  }
  assert.deepEqual(sent, [], "nothing typed goes out while another process writes");
  assert.ok(!statuses(f).some(status => /could not confirm/.test(status ?? "")));

  // The headless writer hands over: registration succeeds and the terminal holds the lease.
  f.setRefused(false);
  f.setLease({ known: true, state: "held", notice: null });
  await later(3610000, () => f.poll());
  assert.equal(statuses(f).at(-1), undefined);
  await later(3610001, () => f.fire("prompt.submit", typed, downstream));
  assert.deepEqual(sent, ["my question"]);
});

// The plugin also runs in the daemon's own headless writer, the `claude -p` it supervises, which
// shares the conversation with a terminal while it hands over. Its registration is refused the
// same way then, but nothing it is given waits: the daemon admitted that input against the lease.
// A terminal, or a headless process the daemon does not supervise, still holds.
test("the daemon's own headless writer never holds its input, a refused terminal still does", async () => {
  const writer = await fixture();
  writer.setRefused(true);
  writer.setEnv("AGIT_RC_SUPERVISED_HOOK", "1");
  await writer.fire("session.start", { isInteractive: false });
  await writer.poll();
  const passed = typing();
  for (const at of [1000, 40000]) {
    await later(at, () => writer.fire("prompt.submit", { ...typed, origin: undefined }, passed.downstream));
  }
  assert.deepEqual(passed.sent, ["my question", "my question"]);

  for (const [env, input] of [[null, { isInteractive: false }], ["1", {}]]) {
    const other = await fixture();
    other.setRefused(true);
    if (env) other.setEnv("AGIT_RC_SUPERVISED_HOOK", env);
    await other.fire("session.start", input);
    await other.poll();
    const held = typing();
    assert.match((await later(40000, () => other.fire("prompt.submit", typed, held.downstream))).drop,
      /another process still writes/);
    assert.deepEqual(held.sent, []);
  }
});
