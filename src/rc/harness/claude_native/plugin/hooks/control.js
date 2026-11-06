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
// How long a prompt submitted during a turn is given to be refused before the receipt says
// Claude Code holds it: a refusal comes from checking the submission, not from the turn.
const QUEUE_REFUSAL_MS = 250;

// Receipts contain operation metadata only. The native transcript remains the message record.
const randomId = () => crypto.randomUUID();
let identity = null;
let registration = null;
let descriptor = null;
let registrationRetry = 0;
let registering = null;
let ending = false;
let turn = null;
let polling = false;
// Whether a person is at this process's prompt: false in the daemon's own headless writer.
let interactive = true;
// This process is the daemon's own headless writer (`claude -p` it supervises). The daemon admits
// every input it sends there against the conversation's lease, so nothing here holds it.
let supervised = false;
// The newest record this process appended to the conversation, and when it loaded it: the
// branch it continues, which the daemon judges the terminal by.
let leaf = null;
let loadedAt = 0;
const tools = new Map();
const compactions = new Set();
const operations = new Map();
let queue = null;
let journal = null;
let modelChange = null;
// Whether this terminal may write the conversation, as the daemon last said: `notice` is the
// reason its own input waits; `unknownSince` is when the daemon stopped answering; `stale` is the
// reason this terminal continues an older branch than the conversation's latest, once confirmed;
// `staleNow` is whether the daemon's last answer still said so. `typed` is whether a prompt was
// held back here, which makes this terminal's branch the one Claude Code resumes next;
// `offered` the message held back with the offer to send it anyway, and `continued` whether the
// person did; `told` whether the stale notice was shown as a toast.
let lease = null;
const LEASE_RECHECK_MS = 2000;
// A daemon that cannot be asked holds the terminal back only this long: waiting beside a writer
// agit can no longer reach protects nothing. A stale history is not such a wait: nothing but
// resuming the conversation again brings it up to date, so no timer releases it.
const LEASE_UNKNOWN_MS = 30000;
// How long a prompt waits for the daemon's first answer before it goes ahead without one.
const LEASE_FIRST_ANSWER_MS = 2000;
// How long a terminal waits for its first record before it asks for the lease without naming its
// branch, which holds it back as stale.
const LEAF_WAIT_MS = 2000;
const STALE_TOAST_MS = 30000;
const CHECKING = "Checking with agit whether another process still writes this conversation.";
// agit answered that another process still writes this conversation (the daemon's own writer
// before it hands the conversation over, or another terminal): a definite answer, so no timer
// releases it. Any failure of native-control other than not_current_writer still expires after
// 30 s (LEASE_UNKNOWN_MS).
const NOT_WRITER = "Not sent yet: another Claude Code is still open on this conversation. Close "
  + "it and this terminal can write; your input waits here.";
const UNREACHABLE = "agit couldn't check whether another process writes this conversation, so "
  + "your input is sent as usual.";
const STALE = (session) => "This terminal continues an older branch of this conversation, without "
  + `the turn sent from the web. Exit and run \`claude --resume ${session}\` now, before typing `
  + "here: a message typed in this terminal keeps it on an older branch of the conversation.";
// Once a prompt was held back here, resuming may land on this terminal's branch again: the only
// way on from this terminal is to continue without the web's turn, knowingly.
const CONTINUE = "Not sent: this terminal continues an older branch of this conversation, without "
  + "the turn sent from the web. Submit the same message again to send it anyway: this continues "
  + "without the turn sent from the web; that turn stays in the history.";

function rotate(session, cwd) {
  identity = { session, cwd, generation: randomId() };
  ending = false;
  registration = null;
  descriptor = null;
  registrationRetry = 0;
  turn = null;
  modelChange = null;
  leaf = null;
  loadedAt = Date.now();
  lease = { known: false, notice: null, unknownSince: Date.now(), checkAt: 0, shown: undefined,
    stale: null, staleNow: false, typed: false, offered: null, continued: false, told: false };
  queue = { sequence: 0, buffered: 0, events: [], spilled: false, work: Promise.resolve() };
  operations.clear();
  tools.clear();
  compactions.clear();
}

async function ensureRegistered($, target) {
  if (registration?.generation === target.generation) return true;
  if (registering?.generation === target.generation) return registering.promise;
  if (Date.now() < registrationRetry || !matches(target)) return false;
  // A terminal asks for the lease with the branch it continues: Claude Code appends its first
  // records (its session-start hooks') right after loading the conversation.
  if (interactive && leaf === null && Date.now() - loadedAt < LEAF_WAIT_MS) return false;
  const pending = { generation: target.generation };
  pending.promise = registerTarget($, target).finally(() => {
    if (registering === pending) registering = null;
  });
  registering = pending;
  return pending.promise;
}

async function hook($, target, argv) {
  const executable = await $.env.get("AGIT_NATIVE_CONTROL_CLI") || DEFAULT_AGIT_CLI;
  if (!matches(target)) return null;
  const reply = await $.process.run([executable, "hooks", ...argv], { cwd: target.cwd,
    timeoutMs: 5000,
    ...(descriptor ? { env: { AGIT_NATIVE_CONTROL_DESCRIPTOR: descriptor } } : {}) });
  if (reply.exitCode !== 0 || reply.isStdoutTruncated || !matches(target)) return null;
  return JSON.parse(reply.stdout);
}

// Only a definite answer changes what the terminal may do; a query that fails keeps the last
// answer until the daemon has been unreachable for LEASE_UNKNOWN_MS.
function learn(target, answer) {
  if (!matches(target)) return;
  lease.checkAt = Date.now() + LEASE_RECHECK_MS;
  if (answer?.known === true) {
    lease.known = true;
    lease.notice = typeof answer.notice === "string" && answer.notice ? answer.notice : null;
    lease.unknownSince = null;
    // Only the daemon granting this terminal the conversation ends a stale hold; an answer with
    // no lease means only that no daemon runs it now, which does not reload the history.
    lease.staleNow = answer.state === "stale";
    if (lease.staleNow) lease.stale = lease.notice ?? lease.stale ?? STALE(target.session);
    else if (answer.state === "held") {
      lease.stale = null;
      lease.offered = null;
      lease.continued = false;
      lease.told = false;
    }
  } else {
    lease.unknownSince ??= Date.now();
  }
}

// The reason this terminal's own input waits now, or null when it may write.
function holding() {
  if (!lease) return null;
  if (lease.stale) {
    if (lease.continued) return null;
    return lease.typed ? CONTINUE : lease.stale;
  }
  if (lease.unknownSince !== null && Date.now() - lease.unknownSince >= LEASE_UNKNOWN_MS) return null;
  if (lease.known && lease.notice) return lease.notice;
  return lease.unknownSince === null ? null : CHECKING;
}

function showHold($) {
  if (!lease) return;
  const reason = holding();
  const expired = !reason && lease.unknownSince !== null;
  const shown = reason ?? (expired && !lease.stale ? UNREACHABLE : null);
  if (shown !== (lease.shown ?? null)) $.ui.status(shown ?? undefined);
  lease.shown = shown;
  // A stale terminal is told at once, before anything is typed into it: only then does resuming
  // bring back the web's turn.
  if (reason && lease.stale && !lease.told) {
    lease.told = true;
    $.ui.toast(reason, { timeoutMs: STALE_TOAST_MS });
  }
}

const withLeaf = (argv) => leaf ? [...argv, "--leaf", leaf] : argv;

async function registerTarget($, target) {
  registrationRetry = Date.now() + 2000;
  const registered = await hook($, target, withLeaf(["native-control", "--session", target.session,
    "--generation", target.generation])).catch(() => null);
  if (!registered) {
    learn(target, null);
    return false;
  }
  if (registered.refused === "not_current_writer") {
    learn(target, { known: true, notice: NOT_WRITER });
    return false;
  }
  if (registered.registration?.session !== target.session
    || registered.registration?.generation !== target.generation
    || typeof registered.descriptor !== "string") return false;
  registration = registered.registration;
  descriptor = await $.env.get("AGIT_NATIVE_CONTROL_DESCRIPTOR") || registered.descriptor;
  learn(target, registered.lease);
  return true;
}

// While the terminal waits, ask again without registering again.
async function recheckLease($, target) {
  if (!lease || holding() === null || Date.now() < lease.checkAt) return;
  lease.checkAt = Date.now() + LEASE_RECHECK_MS;
  const answer = await hook($, target, withLeaf(["native-lease", "--session", target.session]))
    .catch(() => null);
  learn(target, answer?.lease);
}

// The daemon's own headless writer reports the record its turn ended on: a terminal that takes
// the conversation over is current only if it continues from there.
async function reportWriterLeaf($) {
  const target = await current($);
  if (!target || !leaf) return;
  await hook($, target, ["native-lease", "--session", target.session, "--leaf", leaf])
    .catch(() => null);
}

async function current($) {
  const [session, cwd] = await Promise.all([$.session.id(), $.session.cwd()]);
  if (ending && session === identity?.session) return null;
  // A conversation is its session. A `cd` moves the working folder Claude Code reports, while the
  // session stays registered with the folder it started in: this process goes on speaking for it
  // from there, or every registration would be refused as another folder's.
  if (!identity || session !== identity.session) rotate(session, cwd);
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
        const submitted = $.prompt.submit({ text: command.text, asUser: true });
        // While a turn runs, Claude Code holds a submitted prompt for that turn's end, and the
        // submission settles only then, which can be hours. A submission that has not failed by
        // the time the queue has had to refuse it is in that queue, so the receipt says so at
        // once and the daemon tells the sender it starts the next turn. Waiting for the turn's
        // end instead leaves the sender with no answer for as long as the turn runs.
        const queued = turn !== null && await Promise.race([
          submitted.then(() => false, () => false),
          new Promise(resolve => setTimeout(resolve, QUEUE_REFUSAL_MS, true)),
        ]);
        if (queued) {
          submitted.catch(() => {});
          result(record, { outcome: "accepted" });
        } else {
          const accepted = await submitted;
          result(record, { outcome: accepted?.drop === undefined ? "accepted" : "not_sent" });
        }
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
    if (!target) return;
    if (!await ensureRegistered($, target)) {
      showHold($);
      return;
    }
    await recheckLease($, target);
    showHold($);
    const pending = queue;
    const batch = await pendingEvents(target, pending);
    if (!matches(target)) return;
    const sentEvents = batch.events;
    const sentResults = [...operations.values()].filter(r => r.result);
    // Permission asks and the permission mode arrive through the session's settings hooks,
    // which Claude Code runs where it passes user plugins over on its classic events.
    const live = { turn, permission_mode: null, latest_seq: pending.sequence, approvals: [],
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

export function registerControl(on) {
  on("session.start", async ($, e, next) => {
    interactive = e.isInteractive !== false;
    supervised = !interactive && await $.env.get("AGIT_RC_SUPERVISED_HOOK") === "1";
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
    await current($);
    $.clock.every(200, () => poll($));
    return next(e);
  });
  on("session.end", async ($, e, next) => {
    // Invalidate before yielding to the runtime, even if the next session has the same ID.
    const ended = e.sessionId === identity?.session;
    if (ended) ending = true;
    const result = await next(e);
    // Whatever session follows, a /clear or a resume of this one included, starts a new
    // generation at the next read.
    if (ended) identity = null;
    return result;
  });
  on("prompt.submit", async ($, e, next) => {
    // While the conversation's writer is elsewhere, or this terminal continues an older branch,
    // no prompt may continue the conversation here: the person's stays in the box with the reason
    // (Claude Code keeps a dropped prompt), and web input this plugin carries is reported as not
    // sent. Slash commands run in the terminal itself. Only such a definite answer holds a
    // prompt: one that agit couldn't get, whatever the reason, goes ahead, because what a person
    // typed must never be lost to an answer nobody has. On an older branch the person may continue
    // knowingly: the same message submitted again at this prompt goes through as typed, and
    // nothing else is added.
    if (ending || e.command || supervised) return next(e);
    let reason = null;
    try {
      const target = await current($);
      if (target && !lease.known) {
        await Promise.race([ensureRegistered($, target).catch(() => false),
          new Promise(resolve => setTimeout(resolve, LEASE_FIRST_ANSWER_MS))]);
      }
      reason = holding();
      if (reason === CHECKING) reason = null;
      if (reason && lease.stale && !lease.continued && e.origin?.kind === "composer") {
        if (lease.offered === e.text) {
          lease.continued = true;
          reason = null;
        } else {
          lease.offered = e.text;
          reason = CONTINUE;
        }
      }
      // A held-back prompt leaves a record on this terminal's branch.
      if (reason) lease.typed = true;
      showHold($);
    } catch {
      // Whether another process writes is not known, so the prompt goes ahead.
      reason = null;
    }
    return reason ? { drop: reason } : next(e);
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
    // Before the turn's end reaches the daemon, which stops a writer handing over there.
    if (e.agentId === undefined && !interactive) await reportWriterLeaf($).catch(() => {});
    const result = await next(e);
    if (e.agentId === undefined && turn === e.turnId) {
      if (!matches(identity)) return result;
      turn = null;
      await event("turn_completed", { turn: e.turnId, reason: e.reason,
        duration_ms: Number.isFinite(e.durationMs) && e.durationMs >= 0 ? Math.round(e.durationMs) : undefined });
    }
    return result;
  });
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
  on("session.append", async ($, e, next) => {
    const result = await next(e);
    // Only record ids: which branch this process continues, never what the records say. A row
    // is Claude Code's own; nothing here may fail it.
    if (e.agentId === undefined && typeof e.uuid === "string") {
      await current($).then(() => {
        if (!ending && identity) leaf = e.uuid;
      }).catch(() => {});
    }
    return result;
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
