const DEFAULT_AGIT_CLI = "agit";

async function request($, path, body) {
  if (!registration || registration.generation !== body.generation) {
    throw new Error("Native control generation is not registered");
  }
  // Read the descriptor again so daemon replacement never needs a native-session restart.
  const { socket, token } = JSON.parse(await $.fs.read(descriptor));
  if (!socket || !token) throw new Error("Native control is not attached");
  const response = await $.http.fetch("http://localhost" + path, {
    socketPath: socket,
    method: "POST",
    headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw new Error("Native control request was not accepted");
  return JSON.parse(response.text);
}


const MAX_OPERATIONS = 4096;
const MAX_EVENTS = 1024;
const MAX_PROMPT_CHARS = 128 * 1024;

// Receipts contain operation metadata only. The native transcript remains the message record.
const randomId = () => crypto.randomUUID();
let identity = null;
let registration = null;
let descriptor = null;
let registrationRetry = 0;
let registering = null;
let ending = false;
let turn = null;
let permissionMode = null;
let polling = false;
const tools = new Map();
const compactions = new Set();
const operations = new Map();
let queue = null;
let journal = null;
const approvals = new Map();
let modelChange = null;
let recordCompletion = null;

function rotate(session, cwd) {
  identity = { session, cwd, generation: randomId() };
  ending = false;
  registration = null;
  descriptor = null;
  registrationRetry = 0;
  turn = null;
  permissionMode = null;
  modelChange = null;
  queue = { sequence: 0, buffered: 0, events: [], spilled: false, work: Promise.resolve() };
  operations.clear();
  approvals.clear();
  tools.clear();
  compactions.clear();
}

async function ensureRegistered($, target) {
  if (registration?.generation === target.generation) return true;
  if (registering?.generation === target.generation) return registering.promise;
  if (Date.now() < registrationRetry || !matches(target)) return false;
  const pending = { generation: target.generation };
  pending.promise = registerTarget($, target).finally(() => {
    if (registering === pending) registering = null;
  });
  registering = pending;
  return pending.promise;
}

async function registerTarget($, target) {
  registrationRetry = Date.now() + 2000;
  const executable = await $.env.get("AGIT_NATIVE_CONTROL_CLI") || DEFAULT_AGIT_CLI;
  if (!matches(target)) return false;
  const reply = await $.process.run([
    executable, "hooks", "native-control", "--session", target.session,
    "--generation", target.generation,
  ], { cwd: target.cwd, timeoutMs: 5000,
    ...(descriptor ? { env: { AGIT_NATIVE_CONTROL_DESCRIPTOR: descriptor } } : {}) });
  if (reply.exitCode !== 0 || reply.isStdoutTruncated || !matches(target)) return false;
  const registered = JSON.parse(reply.stdout);
  if (registered.registration?.session !== target.session
    || registered.registration?.generation !== target.generation
    || typeof registered.descriptor !== "string") return false;
  registration = registered.registration;
  descriptor = await $.env.get("AGIT_NATIVE_CONTROL_DESCRIPTOR") || registered.descriptor;
  return true;
}

async function current($) {
  const [session, cwd] = await Promise.all([$.session.id(), $.session.cwd()]);
  if (ending && session === identity?.session) return null;
  if (!identity || session !== identity.session || cwd !== identity.cwd) rotate(session, cwd);
  return identity;
}

const matches = (value) => !ending && identity && value?.session === identity.session
  && value?.generation === identity.generation && value?.cwd === identity.cwd;

function queued(queue, work) {
  const pending = queue.work.then(work);
  queue.work = pending.catch(() => {});
  return pending;
}

async function event(kind, fields = {}) {
  if (ending || !identity) return;
  const target = identity;
  const pending = queue;
  const value = { seq: ++pending.sequence, kind, ...fields };
  await queued(pending, async () => {
    if (!matches(target)) return;
    pending.events.push(value);
    pending.buffered = value.seq;
    if (pending.events.length >= MAX_EVENTS) {
      // Spill metadata outside the repository; native transcripts remain the message record.
      const batch = pending.events.slice(0, MAX_EVENTS);
      pending.spilled = true;
      try {
        await journal(target, { events: batch });
        pending.events.splice(0, batch.length);
      } catch {
        // Keep the same sequence for an uncertain disk receipt and retry without dropping it.
      }
    }
  });
}

async function pendingEvents(target, pending) {
  return queued(pending, async () => {
    if (pending.spilled) {
      const saved = await journal(target, {});
      if (saved.events.length) return { events: saved.events, last: saved.events.at(-1).seq };
      pending.spilled = false;
    }
    const events = pending.events.slice(0, MAX_EVENTS);
    return { events, last: events.at(-1)?.seq ?? pending.buffered };
  });
}

function result(record, outcome) {
  record.result = { id: record.id, ...outcome };
}

function validCommand(command) {
  if (typeof command?.id !== "string" || !command.id || command.id.length > 256) return false;
  if (command.method === "prompt") {
    return typeof command.text === "string" && command.text.trim().length > 0
      && command.text.length <= MAX_PROMPT_CHARS;
  }
  if (command.method === "model") {
    return typeof command.model === "string" && command.model.length > 0
      && command.model.length <= 256 && !/[\x00-\x1f\x7f]/.test(command.model);
  }
  return command.method === "abort" && typeof command.turnId === "string"
    && command.turnId.length > 0 && command.turnId.length <= 256;
}

// Native panel and session-switch commands cannot be completed by the web composer.
const TEXT_COMMANDS = new Set(["compact", "context", "cost", "init"]);
async function commandCatalog($) {
  try {
    return (await $.command.list()).filter(command => TEXT_COMMANDS.has(command.name))
      .map(command => ({ name: command.name, description: command.description })).slice(0, 128);
  } catch {
    return [];
  }
}

async function modelControls($) {
  try {
    const row = (await $.config.list()).find(row => row.key === "model"
      && row.provider?.plugin === "engine" && row.provider?.tier === "core");
    if (row?.kind !== "choice" || typeof row.value !== "string") return null;
    return { selected: row.value, locked: row.isLocked !== false,
      choices: (row.options ?? []).filter(value => typeof value === "string"
        && value.length > 0 && value.length <= 256).slice(0, 128),
      pending: modelChange?.id ?? null };
  } catch {
    // Missing configuration support must not disable the native conversation controller.
    return null;
  }
}

async function execute($, command, target) {
  if (!validCommand(command) || !matches(target) || !matches(command)) return;
  // Unacknowledged work stays in memory; durable server claims fence acknowledged replays.
  if (operations.has(command.id)) return;
  if (operations.size >= MAX_OPERATIONS) return;
  const record = { id: command.id, result: null };
  operations.set(command.id, record);
  let invoked = false;
  let finishModel = null;
  try {
    if (command.method === "model") {
      if (modelChange) {
        result(record, { outcome: "not_sent" });
        return;
      }
      record.finished = new Promise(resolve => { finishModel = resolve; });
      modelChange = record;
    }
    // The server durably claims the operation before any native side effect is possible.
    const claim = await request($, "/claim", { ...target, id: command.id });
    await current($);
    if (!matches(target) || claim?.generation !== target.generation || claim?.id !== command.id) {
      result(record, { outcome: "not_sent" });
      return;
    }
    if (claim.claimed !== true) {
      result(record, claim.result?.id === command.id ? claim.result : { outcome: "unknown" });
      return;
    }
    if (command.method === "abort") {
      if (turn !== command.turnId) {
        result(record, { outcome: "no_longer_active" });
        return;
      }
      invoked = true;
      await $.turn.abort({ turnId: command.turnId });
      result(record, { outcome: "requested" });
    } else if (command.method === "model") {
      const controls = await modelControls($);
      await current($);
      if (!matches(target) || !controls || controls.locked
        || !controls.choices.includes(command.model)) {
        result(record, { outcome: "not_sent" });
        return;
      }
      invoked = true;
      const changed = await $.config.set({ key: "model", value: command.model });
      result(record, { outcome: changed?.deny === undefined ? "applied" : "not_sent" });
    } else {
      // A prompt issued after a settings write must observe its native confirmation.
      while (modelChange) await modelChange.finished;
      await current($);
      if (!matches(target)) {
        result(record, { outcome: "not_sent" });
        return;
      }
      const slash = /^\/([\p{L}\p{N}_:-]+)(?:\s+([\s\S]*))?$/u.exec(command.text.trim());
      if (slash) {
        const commands = await commandCatalog($);
        await current($);
        if (!matches(target) || !commands.some(command => command.name === slash[1])) {
          result(record, { outcome: "not_sent" });
          return;
        }
        invoked = true;
        await $.command.run({ command: slash[1], args: slash[2]?.trim() ?? "" });
        result(record, { outcome: "command_completed" });
      } else {
        invoked = true;
        const accepted = await $.prompt.submit({ text: command.text, asUser: true });
        result(record, { outcome: accepted?.drop === undefined ? "accepted" : "not_sent" });
      }
    }
  } catch {
    // A lost native reply cannot prove that the prompt was refused before delivery.
    result(record, { outcome: invoked ? "unknown" : "not_sent" });
  } finally {
    if (modelChange === record) modelChange = null;
    finishModel?.();
  }
}

export async function poll($) {
  if (polling) return;
  polling = true;
  let target;
  try {
    target = await current($);
    if (!target || !await ensureRegistered($, target)) return;
    for (const [id, value] of approvals) {
      if (value.signal.aborted) {
        approvals.delete(id);
        await event("approval_resolved", { id });
      }
    }
    const pending = queue;
    const batch = await pendingEvents(target, pending);
    if (!matches(target)) return;
    const sentEvents = batch.events;
    const sentResults = [...operations.values()].filter(r => r.result);
    const live = { turn, permission_mode: permissionMode, latest_seq: pending.sequence,
      approvals: [...approvals.values()].map(r => r.wire),
      tools: [...tools.values()], compacting: compactions.size > 0 };
    const reply = await request($, "/poll", {
      ...target, ...live, registration, version: 1,
      last_seq: batch.last,
      model: await $.session.model(), saturated: false,
      model_controls: await modelControls($), commands: await commandCatalog($),
      events: sentEvents, results: sentResults.map(r => r.result),
    });
    // /clear and /resume may run while an HTTP request is in flight, including an ABA switch.
    await current($);
    if (!matches(target) || reply?.generation !== target.generation) return;
    const sentSequence = sentEvents.at(-1)?.seq ?? 0;
    if (Number.isSafeInteger(reply.ack_seq) && reply.ack_seq >= 0 && reply.ack_seq <= sentSequence) {
      await queued(pending, async () => {
        if (pending.spilled) await journal(target, { ack_seq: reply.ack_seq });
        while (pending.events[0]?.seq <= reply.ack_seq) pending.events.shift();
      });
    }
    if (!matches(target)) return;
    const acknowledged = new Set(Array.isArray(reply.ack_results) ? reply.ack_results : []);
    for (const record of sentResults) if (acknowledged.has(record.id)) operations.delete(record.id);
    if (Array.isArray(reply.commands) && reply.commands.length <= 32) {
      for (const command of reply.commands) void execute($, command, target);
    }
  } catch {
    // The native terminal stays usable while its optional control channel reconnects.
    if (matches(target) && Date.now() >= registrationRetry) registration = null;
  } finally {
    polling = false;
  }
}

async function permission($, e, next) {
  const inherited = await next(e);
  if (inherited.decision || e.agent_id !== undefined) return inherited;
  const target = await current($);
  if (!target || e.session_id !== target.session || next.signal.aborted) return inherited;
  const id = randomId();
  const wire = { id, turn, tool: e.tool_name, input: e.tool_input };
  approvals.set(id, { wire, signal: next.signal });
  const executable = await $.env.get("AGIT_NATIVE_CONTROL_CLI") || DEFAULT_AGIT_CLI;
  try {
    while (!next.signal.aborted && matches(target)) {
      try {
        if (await ensureRegistered($, target)) {
          // Native API waits preserve the hook budget while the person chooses an answer.
          const result = await $.process.run([executable, "hooks", "native-approval",
            "--session", target.session, "--generation", target.generation, "--id", id],
          { cwd: target.cwd, timeoutMs: 35000,
            ...(descriptor ? { env: { AGIT_NATIVE_CONTROL_DESCRIPTOR: descriptor } } : {}) });
          if (result.exitCode !== 0 || result.isStdoutTruncated) throw new Error("Native approval is reconnecting");
          const reply = JSON.parse(result.stdout);
          if (next.signal.aborted || !matches(target)) break;
          if (reply?.generation === target.generation && reply.id === id) {
            if (reply.decision?.behavior === "allow" || reply.decision?.behavior === "deny") {
              return { ...inherited, decision: reply.decision };
            }
          }
          continue;
        }
      } catch {
        // A transient control outage must not discard an unanswered native approval.
      }
      await $.clock.sleep(200, { signal: next.signal });
    }
  } catch {
    // A native terminal decision aborts all APIs in this hook's context.
  } finally {
    if (matches(target) && approvals.delete(id)) {
      await event("approval_resolved", { id });
    }
  }
  return inherited;
}

export function registerControl(on) {
  on("session.start", async ($, e, next) => {
    journal = async (target, body) => {
      const executable = await $.env.get("AGIT_NATIVE_CONTROL_CLI") || DEFAULT_AGIT_CLI;
      const result = await $.process.run([executable, "hooks", "native-events",
        "--session", target.session, "--generation", target.generation],
      { cwd: target.cwd, timeoutMs: 5000, stdin: JSON.stringify(body),
        ...(descriptor ? { env: { AGIT_NATIVE_CONTROL_DESCRIPTOR: descriptor } } : {}) });
      if (result.exitCode !== 0 || result.isStdoutTruncated) throw new Error("Native event queue is unavailable");
      const reply = JSON.parse(result.stdout);
      if (!Array.isArray(reply.events) || reply.events.length > MAX_EVENTS) {
        throw new Error("Native event queue returned an invalid page");
      }
      return reply;
    };
    recordCompletion = async (target, e) => {
      const executable = await $.env.get("AGIT_NATIVE_CONTROL_CLI") || DEFAULT_AGIT_CLI;
      return $.process.run([executable, "hooks", "native-complete", "--session", target.session,
        "--generation", target.generation, "--turn", e.turnId, "--reason", e.reason],
      { cwd: target.cwd, timeoutMs: 5000,
        ...(descriptor ? { env: { AGIT_NATIVE_CONTROL_DESCRIPTOR: descriptor } } : {}) });
    };
    await current($);
    $.clock.every(200, () => poll($));
    return next(e);
  });
  on("classic.SessionEnd", ($, e, next) => {
    // Invalidate before yielding to the runtime, even if the next session has the same ID.
    if (e.agent_id === undefined && e.session_id === identity?.session) ending = true;
    return next(e);
  });
  on("classic.SessionStart", ($, e, next) => {
    if (e.agent_id === undefined) {
      // Compaction can announce SessionStart while the same native turn is still running.
      if (ending || !identity || identity.session !== e.session_id || identity.cwd !== e.cwd) {
        rotate(e.session_id, e.cwd);
      }
      permissionMode = e.permission_mode ?? null;
    }
    return next(e);
  });
  on("turn.start", async ($, e, next) => {
    await current($);
    if (!ending) {
      turn = e.turnId;
      await event("turn_started", { turn: e.turnId });
    }
    return next(e);
  });
  on("turn.complete", async ($, e, next) => {
    const result = await next(e);
    if (e.agentId === undefined && turn === e.turnId) {
      const target = identity;
      try {
        // An interrupted turn cancels its own APIs; the session context outlives that turn.
        // Answered turns can flush their native reply after this hook returns.
        if (e.reason === "aborted" && !e.usage && e.answer === "") {
          await recordCompletion?.(target, e);
        }
      } catch {
        // Native input remains usable when local archive metadata cannot be written.
      }
      if (!matches(target)) return result;
      turn = null;
      await event("turn_completed", { turn: e.turnId, reason: e.reason,
        duration_ms: Number.isFinite(e.durationMs) && e.durationMs >= 0 ? Math.round(e.durationMs) : undefined });
    }
    return result;
  });
  on("classic.UserPromptSubmit", ($, e, next) => {
    if (e.agent_id === undefined && e.session_id === identity?.session && !ending) {
      permissionMode = e.permission_mode ?? null;
    }
    return next(e);
  });
  on("classic.PermissionRequest", permission);
  on("tool.call", async ($, e, next) => {
    if (e.agentId !== undefined || !identity || ending || !e.tool_use_id) return next(e);
    const target = identity;
    const id = e.tool_use_id;
    tools.set(id, { id, tool: e.tool });
    await event("tool_started", { id, tool: e.tool });
    try {
      return await next(e);
    } finally {
      if (matches(target) && tools.delete(id)) await event("tool_completed", { id });
    }
  });
  on("session.compact", async ($, e, next) => {
    if (e.agentId !== undefined || !identity || ending) return next(e);
    const target = identity;
    const id = randomId();
    compactions.add(id);
    await event("compaction_started", { id });
    try {
      return await next(e);
    } finally {
      if (matches(target) && compactions.delete(id)) await event("compaction_completed", { id });
    }
  });
}
