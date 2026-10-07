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
  const approvalReplies = [];
  const claims = new Map();
  const pages = new Map();
  let acknowledged = 0;
  let session = "a";
  const api = {
    session: { id: async () => session, cwd: async () => "/project", model: async () => "native-model" },
    clock: { every() {} },
    prompt: { submit: async value => { calls.push(["prompt", value]); return { text: value.text }; } },
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
    if (path === "/approval") return approvalReplies.shift()(body);
    for (const result of body.results) claims.set(body.generation + ":" + result.id, result);
    const reply = replies.shift();
    return reply ? reply(body) : { generation: body.generation, ack_seq: body.events.at(-1)?.seq ?? 0 };
  };
  api.env = { get: async () => null };
  api.process = { run: async (argv, init) => {
    if (argv[2] === "native-approval") {
      const reply = await request(api, "/approval", {
        session: argv[4], generation: argv[6], id: argv[8],
      });
      return { exitCode: 0, stdout: JSON.stringify(reply) };
    }
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
    return { exitCode: 0, stdout: JSON.stringify({
    registration: { session: argv[4], generation: argv[6] }, descriptor: "/owned-descriptor",
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
  return {
    calls, requests, replies, approvalReplies, control, api,
    fire: (name, input = {}, downstream = next, context = api) => hooks.get(name)(context, input, downstream),
    setSession: value => { session = value; },
    poll: () => control.poll(api),
    latest: () => requests.filter(r => r.path === "/poll").at(-1).body,
  };
}

test("session ABA, lost replies, and stale turn IDs never repeat or retarget native effects", async () => {
  const f = await fixture();
  await f.fire("session.start");
  const delayed = deferred();
  f.replies.push(() => delayed.promise);
  const polling = f.poll();
  await flush();
  const first = f.latest();
  await f.fire("classic.SessionEnd", { session_id: "a" });
  f.setSession("b");
  await f.fire("classic.SessionStart", { session_id: "b", cwd: "/project" });
  await f.fire("classic.SessionEnd", { session_id: "b" });
  f.setSession("a");
  await f.fire("classic.SessionStart", { session_id: "a", cwd: "/project" });
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

test("an unanswered native interruption persists its boundary outside the cancelled turn context", async () => {
  const f = await fixture();
  const saved = deferred();
  const run = f.api.process.run;
  const completed = [];
  f.api.process.run = async argv => {
    if (argv[2] !== "native-complete") return run(argv);
    completed.push(argv);
    await saved.promise;
    return { exitCode: 0, stdout: "{}" };
  };
  await f.fire("session.start");
  await f.fire("turn.start", { turnId: "stopped" });
  const cancelled = { env: { get: () => { throw new Error("Turn cancelled"); } },
    process: { run: () => { throw new Error("Turn cancelled"); } } };
  const stopping = f.fire("turn.complete", { turnId: "stopped", reason: "aborted", answer: "" },
    async () => ({}), cancelled);
  await flush();
  assert.equal(completed.length, 1);
  await f.poll();
  assert.equal(f.latest().turn, "stopped", "completion cannot overtake its durable boundary");
  saved.resolve();
  await stopping;
  await f.poll();
  assert.equal(f.latest().turn, null);
  assert.ok(f.latest().events.some(event => event.kind === "turn_completed" && event.turn === "stopped"));
  await f.fire("turn.start", { turnId: "answered" });
  await f.fire("turn.complete", { turnId: "answered", reason: "answer", answer: "Done", usage: {} });
  assert.equal(completed.length, 1, "answered turns must wait for their native transcript flush");
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

test("a native approval answer retires remote control outside the cancelled hook context", async () => {
  const f = await fixture();
  await f.fire("session.start");
  await f.fire("turn.start", { turnId: "main-turn" });
  const controller = new AbortController();
  const inherited = {};
  const next = async () => inherited;
  next.signal = controller.signal;
  const delayed = deferred();
  f.approvalReplies.push(() => delayed.promise);
  const permission = f.fire("classic.PermissionRequest", {
    session_id: "a", tool_name: "Write", tool_input: { file_path: "/project/result" },
  }, next);
  await flush();
  await f.poll();
  const target = f.latest();
  assert.equal(target.approvals.length, 1);
  const id = target.approvals[0].id;
  controller.abort();
  await f.poll();
  assert.deepEqual(f.latest().approvals, []);
  assert.ok(f.latest().events.some(e => e.kind === "approval_resolved" && e.id === id));
  delayed.resolve({ generation: target.generation, id, decision: { behavior: "allow" } });
  assert.equal(await permission, inherited, "a late remote allow cannot replace a native terminal answer");

  await f.fire("turn.complete", { turnId: "child-turn", agentId: "child", reason: "completed" });
  await f.poll();
  assert.equal(f.latest().turn, "main-turn", "a subagent completion does not end the main conversation");
  assert.equal(await f.fire("classic.PermissionRequest", {
    session_id: "a", agent_id: "child", tool_name: "Write", tool_input: {},
  }, next), inherited);
});

test("an unanswered question waits through native APIs and applies its exact answer", async () => {
  const f = await fixture();
  const retry = deferred();
  f.api.clock.sleep = () => assert.fail("waiting for the person cannot consume the hook budget");
  const input = { questions: [{ question: "Continue?", options: [{ label: "Yes" }] }] };
  const decision = { behavior: "allow", updatedInput: { ...input, answers: { "Continue?": "Yes" } } };
  f.approvalReplies.push(async body => {
    await retry.promise;
    return { generation: body.generation, id: body.id, decision: null };
  });
  f.approvalReplies.push(body => ({ generation: body.generation, id: body.id, decision }));
  await f.fire("session.start");
  const permission = f.fire("classic.PermissionRequest", {
    session_id: "a", tool_name: "AskUserQuestion", tool_input: input,
  });
  await flush();
  await f.poll();
  assert.equal(f.latest().approvals.length, 1, "waiting cannot discard a pending native question");
  retry.resolve();
  assert.deepEqual(await permission, { decision });
  await f.poll();
  assert.equal(f.latest().approvals.length, 0);
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
  const generation = f.latest().generation;
  await f.fire("classic.SessionStart", { session_id: "a", cwd: "/project", source: "compact" });
  await f.poll();
  assert.equal(f.latest().generation, generation, "compaction must not replace the live control generation");
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
