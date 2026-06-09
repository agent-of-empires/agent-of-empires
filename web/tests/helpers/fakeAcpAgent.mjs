#!/usr/bin/env node
// Fake ACP agent for structured view Playwright tests.
//
// Speaks just enough of the Agent Client Protocol (newline-delimited
// JSON-RPC 2.0) for `src/acp/acp_client.rs` to drive a turn:
//
//   initialize          -> return protocolVersion + agentCapabilities
//   session/new         -> return a deterministic sessionId
//   session/load        -> same shape; lets structured view's Resume mode work
//   session/prompt      -> emit scripted session/update notifications,
//                          then return a stop response. Script entries
//                          with `sessionUpdate: "permission_request"`
//                          are translated into a real outbound
//                          `session/request_permission` JSON-RPC
//                          REQUEST; the fake awaits the client's
//                          response before continuing the turn.
//   session/set_mode    -> emit current_mode_update (also accepts the
//                          legacy camelCase `session/setMode`)
//   session/cancel      -> notification (no response); sets a per-
//                          session cancel flag that the in-flight
//                          session/prompt loop polls so the prompt
//                          returns with stopReason "cancelled"
//
// Script source:
//
//   The env var FAKE_ACP_SCRIPT points to a JSON file describing the
//   event sequence(s) to emit per `session/prompt` call. If unset, the
//   default is a single happy-path turn that emits one agent_message_chunk
//   then stops.
//
// Script shape (rough):
//
//   {
//     "turns": [
//       {
//         "updates": [
//           { "sessionUpdate": "agent_message_chunk", "content": {...} },
//           ...
//         ],
//         "stopReason": "end_turn"
//       },
//       ...
//     ]
//   }
//
// Each `session/prompt` consumes one entry from `turns`. If the array is
// exhausted, subsequent prompts get the default happy-path turn.

import { createInterface } from "node:readline";
import { readFileSync, existsSync, appendFileSync } from "node:fs";
import { randomBytes } from "node:crypto";

// Silently swallow EPIPE/EBADF on stdout/stderr writes. The fake is a
// noisy stdout writer (one JSON line per session/update notification);
// if the supervisor briefly stalls draining the pipe (CI under v8
// coverage instrumentation has spent ~hundreds of ms behind the
// runtime worker), a subsequent write can emit EPIPE; without a
// handler, Node treats that as an uncaught exception and exits the
// process. The runner sees the child exit, deletes the worker_registry
// entry, the supervisor's reap pass publishes Stopped {
// reason: "user_stopped" }, and the UI banners "Structured view worker
// stopped" mid-turn. That matched the symptom in #1383 (see Once /
// upon visible but "a time." never arriving in the composer-streamed
// trace). Swallowing the error is safe: write failures here mean the
// peer is gone, and there is no surface in the fake that benefits
// from observing them.
const FAKE_DEBUG_PATH = process.env.FAKE_ACP_DEBUG_LOG;
function fakeDebug(line) {
  if (!FAKE_DEBUG_PATH) return;
  try {
    appendFileSync(FAKE_DEBUG_PATH, `[${Date.now()}] ${line}\n`);
  } catch {
    // ignore
  }
}
process.stdout.on("error", (err) => {
  fakeDebug(`stdout error swallowed: ${err.code ?? err.message}`);
});
process.stderr.on("error", () => {});
// Log the failure cause to fake-acp.log so post-mortems can see why
// the agent died, then crash-fast. Default Node behavior on
// uncaughtException is exit(1); merely registering a listener
// suppresses that, which lets a real bug keep the fake agent
// half-alive and surface as a confusing supervisor timeout downstream
// instead of the actual stack trace.
process.on("uncaughtException", (err) => {
  fakeDebug(`uncaughtException: ${err?.stack ?? err}`);
  process.exit(1);
});
process.on("unhandledRejection", (reason) => {
  fakeDebug(`unhandledRejection: ${reason}`);
  process.exit(1);
});
process.on("exit", (code) => {
  fakeDebug(`process.exit code=${code}`);
});
process.on("SIGTERM", () => {
  fakeDebug("SIGTERM received, exiting 0");
  process.exit(0);
});
process.on("SIGINT", () => {
  fakeDebug("SIGINT received, exiting 0");
  process.exit(0);
});
process.on("SIGPIPE", () => {
  fakeDebug("SIGPIPE received (ignored)");
});
process.on("SIGHUP", () => {
  fakeDebug("SIGHUP received (ignored)");
});
fakeDebug(`fake-acp starting pid=${process.pid} argv=${JSON.stringify(process.argv)}`);

const DEFAULT_TURN = {
  updates: [
    {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "Hello from fake ACP agent." },
    },
  ],
  stopReason: "end_turn",
};

function loadScript() {
  const path = process.env.FAKE_ACP_SCRIPT;
  if (!path || !existsSync(path)) return { turns: [] };
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch (err) {
    process.stderr.write(`[fakeAcpAgent] failed to parse FAKE_ACP_SCRIPT=${path}: ${err}\n`);
    return { turns: [] };
  }
}

const script = loadScript();
let turnCursor = 0;

function nextTurn() {
  if (turnCursor < script.turns.length) {
    return script.turns[turnCursor++];
  }
  return DEFAULT_TURN;
}

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function sendResult(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

function sendError(id, code, message, data) {
  const error = { code, message };
  if (data !== undefined) error.data = data;
  send({ jsonrpc: "2.0", id, error });
}

function sendNotification(method, params) {
  send({ jsonrpc: "2.0", method, params });
}

// Track outbound requests we send to the client so we can resolve them
// when the client's response arrives. Keyed on the request id.
let nextOutboundId = 1;
const pendingOutbound = new Map();

// 15s ceiling so a wedged supervisor surfaces as an explicit timeout
// instead of a hung promise that stalls the script's remaining updates
// and leaves Playwright to time out generically.
const OUTBOUND_REQUEST_TIMEOUT_MS = 15_000;

function sendRequest(method, params) {
  const id = `fake-acp-req-${nextOutboundId++}`;
  send({ jsonrpc: "2.0", id, method, params });
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      if (pendingOutbound.delete(id)) {
        reject(new Error(`fakeAcpAgent: outbound ${method} id=${id} timed out after ${OUTBOUND_REQUEST_TIMEOUT_MS}ms`));
      }
    }, OUTBOUND_REQUEST_TIMEOUT_MS);
    pendingOutbound.set(id, {
      resolve: (value) => {
        clearTimeout(timer);
        resolve(value);
      },
      reject: (err) => {
        clearTimeout(timer);
        reject(err);
      },
    });
  });
}

function resolveOutbound(msg) {
  const entry = pendingOutbound.get(msg.id);
  if (!entry) {
    process.stderr.write(`[fakeAcpAgent] response for unknown id ${msg.id}\n`);
    return;
  }
  pendingOutbound.delete(msg.id);
  if (msg.error) {
    entry.reject(msg.error);
  } else {
    entry.resolve(msg.result);
  }
}

// Default permission options when a `permission_request` script entry
// omits its own. Covers every PermissionOptionKind aoe's
// `pick_option_id` (src/acp/acp_client.rs) consults so an
// `allow` / `allow_always` / `deny` decision always maps cleanly.
const DEFAULT_PERMISSION_OPTIONS = [
  { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
  { optionId: "allow-always", name: "Allow always", kind: "allow_always" },
  { optionId: "reject-once", name: "Reject once", kind: "reject_once" },
  { optionId: "reject-always", name: "Reject always", kind: "reject_always" },
];

// Per-session cancel flags. session/cancel is a notification (no id);
// the in-flight session/prompt loop polls this flag at each step and
// short-circuits when set so the prompt returns with stopReason
// "cancelled" instead of running the rest of the scripted updates.
const cancelFlags = new Map();

// OpenCode-shaped mode advertisement (#1764). When
// FAKE_ACP_MODE_VIA_CONFIG_OPTION is set, the fake advertises its modes
// ONLY as a `category:"mode"` config option (no ACP SessionModeState
// `modes` field) and rejects any mode value outside this list, exactly
// like `opencode acp`. Lets a test prove the structured view picker reads the
// config-option channel and never offers a phantom "default" mode.
const OPENCODE_MODE_CHOICES = [
  { value: "build", name: "Build" },
  { value: "plan", name: "Plan" },
];
const opencodeModeBySession = new Map();

// Unstable `session_model` channel (ACP unstable_session_model, #1820).
// Agents like Claude can advertise their model selector via the
// SessionModelState field on the session/new (and session/load) response
// instead of a generic config_option. Tests opt in with
// FAKE_ACP_EMIT_SESSION_MODEL=1 (usually alongside
// FAKE_ACP_EMIT_CONFIG_OPTIONS=0 so the unstable channel is the only
// model source) and drive a switch via the same cockpit/config-option
// endpoint using the reserved synthetic id, which aoe routes to
// session/set_model.
const SESSION_MODEL_CHOICES = [
  { modelId: "fake-sonnet", name: "Fake Sonnet" },
  { modelId: "fake-opus", name: "Fake Opus" },
];
const sessionModelBySession = new Map();

function makeOpencodeModeOption(currentValue) {
  return {
    id: "mode",
    name: "Session Mode",
    category: "mode",
    type: "select",
    currentValue,
    options: OPENCODE_MODE_CHOICES,
  };
}

async function emitSessionUpdates(sessionId, updates) {
  for (const u of updates) {
    if (cancelFlags.get(sessionId)) return;
    if (u && u.sessionUpdate === "wait_ms") {
      // Story-spec helper: pause emission inside a turn so the UI
      // observes the turn as active long enough to click Stop, queue a
      // follow-up, etc. Not part of ACP; the fake just swallows it.
      // Clamp the raw value: NaN, Infinity, or negative numbers would
      // make setTimeout fire immediately or behave unpredictably and
      // mask bad fixture data. Cap at 60s so a typo can't hang CI.
      const raw = typeof u.ms === "number" && Number.isFinite(u.ms) ? u.ms : 200;
      const ms = Math.min(60_000, Math.max(0, Math.floor(raw)));
      // Sleep in 50ms slices so a cancel notification arriving during
      // a long wait_ms doesn't have to wait for the full duration
      // before the cancel flag is observed.
      const sliceMs = 50;
      let remaining = ms;
      while (remaining > 0) {
        if (cancelFlags.get(sessionId)) return;
        const slice = Math.min(sliceMs, remaining);
        await new Promise((resolve) => setTimeout(resolve, slice));
        remaining -= slice;
      }
      continue;
    }
    if (u && u.sessionUpdate === "permission_request") {
      // ACP carries permissions on a separate JSON-RPC request, not on
      // session/update. Translate the scripted entry into a real
      // `session/request_permission` request and wait for the client's
      // decision before continuing the turn so the assertions in the
      // approval spec can observe ApprovalRequested + resolve it.
      await sendRequest("session/request_permission", {
        sessionId,
        toolCall: u.toolCall ?? {
          toolCallId: `fake-tool-call-${Date.now()}`,
          title: "fake tool call",
          kind: "edit",
        },
        options: u.options ?? DEFAULT_PERMISSION_OPTIONS,
      }).catch((err) => {
        process.stderr.write(`[fakeAcpAgent] permission_request rejected: ${JSON.stringify(err)}\n`);
      });
      continue;
    }
    sendNotification("session/update", { sessionId, update: u });
    // Inter-update tick so the structured view reducer can apply each event in
    // order rather than batching them. Bumped from 1ms to 5ms after
    // CI flakes (#1383): under 6-worker contention on the 4-core CI
    // runner, a 1ms tick didn't survive the event-loop pressure and
    // some specs lost the first chunk before the React reducer ran.
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

function makeSessionId() {
  return `fake-acp-${Date.now()}-${randomBytes(4).toString("hex")}`;
}

const INITIALIZE_RESULT = {
  protocolVersion: 1,
  agentCapabilities: {
    loadSession: true,
    promptCapabilities: {
      image: false,
      embeddedContext: false,
    },
    mcpCapabilities: {
      http: false,
      sse: false,
    },
  },
  agentInfo: {
    name: "@agentclientprotocol/claude-agent-acp",
    version: "0.41.0",
  },
  // No authMethods key at all. An empty array is interpreted by some
  // ACP client implementations as "auth methods listed but none
  // available", which surfaces as AuthRequired on the next call.
  // Omitting the key signals "no auth required" cleanly.
};

async function handleRequest(msg) {
  const { id, method, params } = msg;
  fakeDebug(`handleRequest method=${method} id=${id}`);
  if (process.env.FAKE_ACP_DEBUG) {
    try {
      const { appendFileSync } = await import("node:fs");
      appendFileSync(
        process.env.FAKE_ACP_DEBUG,
        `req method=${method} id=${id} params=${JSON.stringify(params).slice(0, 200)}\n`,
      );
    } catch {
      // ignore log errors
    }
  }
  // Script-controlled failure injection: lets specs simulate "adapter
  // returns a JSON-RPC error on method X" without needing a hand-rolled
  // ACP server per failure mode. Shape: script.failOn = { method:
  // "session/new", code: -32603, message: "Internal error", data: {...} }.
  // Single-shot by default; set `repeat: true` to keep failing on every
  // call. See web/tests/live/acp-stories/startup-error-banner-native-binary.spec.ts.
  if (script.failOn && script.failOn.method === method) {
    const f = script.failOn;
    sendError(
      id,
      typeof f.code === "number" ? f.code : -32603,
      typeof f.message === "string" ? f.message : "Internal error",
      f.data,
    );
    if (!f.repeat) {
      script.failOn = null;
    }
    return;
  }

  switch (method) {
    case "initialize": {
      // A script may advertise prompt capabilities (image / audio /
      // embeddedContext) so attachment specs can exercise the gate
      // without a real agent. Defaults stay all-false. See #1000 / #965.
      const result = script.promptCapabilities
        ? {
            ...INITIALIZE_RESULT,
            agentCapabilities: {
              ...INITIALIZE_RESULT.agentCapabilities,
              promptCapabilities: {
                ...INITIALIZE_RESULT.agentCapabilities.promptCapabilities,
                ...script.promptCapabilities,
              },
            },
          }
        : INITIALIZE_RESULT;
      sendResult(id, result);
      return;
    }

    case "session/new":
    case "session/load": {
      const sessionId = params?.sessionId ?? makeSessionId();
      // Test hook: when FAKE_ACP_COMMANDS is set, emit an
      // `available_commands_update` session/update notification right
      // after the session/new response so the structured view composer's
      // slash-command popover is populated for stories that drive the
      // `/` picker (e.g. composer-slash-pick-no-arg #1512).
      const commandsJson = process.env.FAKE_ACP_COMMANDS;
      if (commandsJson) {
        try {
          const parsed = JSON.parse(commandsJson);
          if (Array.isArray(parsed) && parsed.length > 0) {
            // ACP wire shape (per
            // agent-client-protocol-schema 0.12 client.rs:447-508):
            // each AvailableCommand has `name`, `description`, and an
            // optional `input` of `{ hint: "..." }` for unstructured
            // free-form args. accepts_input=false serializes as input
            // omitted.
            const availableCommands = parsed.map((c) => ({
              name: c.name,
              description: c.description ?? "",
              ...(c.accepts_input ? { input: { hint: c.hint ?? "" } } : {}),
            }));
            // Defer emission to the next tick so the session/new
            // response lands first; the structured view acp_client wires the
            // session id from the response before it can route follow-up
            // session/update notifications.
            setImmediate(() => {
              sendNotification("session/update", {
                sessionId,
                update: {
                  sessionUpdate: "available_commands_update",
                  availableCommands,
                },
              });
            });
          }
        } catch (err) {
          process.stderr.write(`[fakeAcpAgent] bad FAKE_ACP_COMMANDS: ${err}\n`);
        }
      }
      // Mirror claude-agent-acp v0.37.0: the initial set of
      // per-session selectors (model + effort + mode) ships in the
      // session/new and session/load *response*, not as a subsequent
      // notification. The structured view's acp_client reads the response
      // field and emits Event::ConfigOptionsUpdated. See #1403.
      const includeConfigOptions = process.env.FAKE_ACP_EMIT_CONFIG_OPTIONS !== "0";
      const result = { sessionId };
      if (includeConfigOptions) {
        result.configOptions = [
          {
            id: "model",
            name: "Model",
            category: "model",
            type: "select",
            currentValue: "claude-opus-4-7",
            options: [
              { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
              { value: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" },
            ],
          },
          {
            id: "effort",
            name: "Reasoning Effort",
            category: "thought_level",
            type: "select",
            currentValue: "default",
            options: [
              { value: "default", name: "Default" },
              { value: "low", name: "Low" },
              { value: "medium", name: "Medium" },
              { value: "high", name: "High" },
            ],
          },
        ];
      }
      if (process.env.FAKE_ACP_MODE_VIA_CONFIG_OPTION) {
        const current = opencodeModeBySession.get(sessionId) ?? "build";
        opencodeModeBySession.set(sessionId, current);
        result.configOptions = [...(result.configOptions ?? []), makeOpencodeModeOption(current)];
      }
      if (process.env.FAKE_ACP_EMIT_SESSION_MODEL === "1") {
        const current = sessionModelBySession.get(sessionId) ?? SESSION_MODEL_CHOICES[0].modelId;
        sessionModelBySession.set(sessionId, current);
        // ACP SessionModelState (camelCase wire shape per
        // agent-client-protocol-schema unstable_session_model).
        result.models = {
          currentModelId: current,
          availableModels: SESSION_MODEL_CHOICES.map((m) => ({
            modelId: m.modelId,
            name: m.name,
          })),
        };
      }
      sendResult(id, result);
      return;
    }

    case "session/setModel":
    case "session/set_model": {
      // Unstable model-switch channel (#1820). ACP wire method is
      // `session/set_model` (snake_case); accept both spellings. The
      // real adapter only acks with no state echo, so aoe synthesizes
      // the follow-up ConfigOptionsUpdated itself. Tests opt into a
      // rejection with FAKE_ACP_REJECT_SET_MODEL.
      const sessionId = params?.sessionId;
      const modelId = params?.modelId;
      if (process.env.FAKE_ACP_REJECT_SET_MODEL) {
        sendError(id, -32000, process.env.FAKE_ACP_REJECT_SET_MODEL);
        return;
      }
      if (!SESSION_MODEL_CHOICES.some((m) => m.modelId === modelId)) {
        sendError(id, -32000, `model not found: ${modelId}`);
        return;
      }
      if (sessionId) sessionModelBySession.set(sessionId, modelId);
      sendResult(id, {});
      return;
    }

    case "session/setMode":
    case "session/set_mode": {
      // ACP wire method name is `session/set_mode` (snake_case) per
      // agent-client-protocol-schema. Accept both spellings so a future
      // rename doesn't silently regress this fake.
      const sessionId = params?.sessionId;
      const modeId = params?.modeId;
      if (process.env.FAKE_ACP_MODE_VIA_CONFIG_OPTION && !OPENCODE_MODE_CHOICES.some((m) => m.value === modeId)) {
        // OpenCode rejects set_mode for any id outside its real mode
        // list (this is the "mode not found" the trapped user hit).
        sendError(id, -32000, `mode not found: ${modeId}`);
        return;
      }
      sendResult(id, {});
      if (sessionId && modeId) {
        // Emit the ACP-correct variant so the supervisor translates to
        // a server-side Event::CurrentModeChanged for the reducer.
        await emitSessionUpdates(sessionId, [{ sessionUpdate: "current_mode_update", currentModeId: modeId }]);
      }
      return;
    }

    case "session/set_config_option": {
      // Mirrors claude-agent-acp's setSessionConfigOption: accept the
      // new value, return the updated configOptions list in the
      // response payload. Real claude-agent-acp's setSessionConfigOption
      // does NOT emit a follow-up config_option_update notification
      // (acp-agent.js:1003-1057); the response carries the new state
      // and aoe synthesizes Event::ConfigOptionsUpdated from it. Tests
      // opt into a rejected response by setting
      // FAKE_ACP_REJECT_CONFIG_OPTION. See #1403.
      const configId = params?.configId;
      const value = params?.value;
      if (process.env.FAKE_ACP_REJECT_CONFIG_OPTION) {
        sendError(id, -32000, process.env.FAKE_ACP_REJECT_CONFIG_OPTION);
        return;
      }
      if (process.env.FAKE_ACP_MODE_VIA_CONFIG_OPTION && configId === "mode") {
        // OpenCode-shaped mode switch via the config-option channel.
        if (!OPENCODE_MODE_CHOICES.some((m) => m.value === value)) {
          sendError(id, -32000, `mode not found: ${value}`);
          return;
        }
        const sessionId = params?.sessionId;
        if (sessionId) opencodeModeBySession.set(sessionId, value);
        sendResult(id, { configOptions: [makeOpencodeModeOption(value)] });
        return;
      }
      const configOptions =
        configId && value
          ? [
              {
                id: configId,
                name: configId === "model" ? "Model" : "Reasoning Effort",
                category: configId === "model" ? "model" : "thought_level",
                type: "select",
                currentValue: value,
                options:
                  configId === "model"
                    ? [
                        { value: "claude-opus-4-7", name: "Claude Opus 4.7" },
                        {
                          value: "claude-sonnet-4-6",
                          name: "Claude Sonnet 4.6",
                        },
                      ]
                    : [
                        { value: "default", name: "Default" },
                        { value: "low", name: "Low" },
                        { value: "medium", name: "Medium" },
                        { value: "high", name: "High" },
                      ],
              },
            ]
          : [];
      sendResult(id, { configOptions });
      return;
    }

    case "session/prompt": {
      const sessionId = params?.sessionId;
      const turn = nextTurn();
      // Reset any prior cancel flag so this turn starts clean.
      if (sessionId) cancelFlags.set(sessionId, false);
      if (sessionId) {
        await emitSessionUpdates(sessionId, turn.updates);
      }
      const wasCancelled = sessionId ? cancelFlags.get(sessionId) : false;
      if (sessionId) cancelFlags.set(sessionId, false);
      // Rate-limit park: a turn with `rateLimit` returns session/prompt as
      // a JSON-RPC error carrying the fingerprint aoe's
      // classify_rate_limit_error reads (errorKind "rate_limit" plus a
      // resets_at), instead of a normal stopReason. The structured view worker
      // then emits a typed RateLimit event and parks the session. See
      // #1281 / #1715. A cancel mid-turn still wins over the park.
      if (!wasCancelled && turn.rateLimit) {
        sendError(id, -32000, turn.rateLimit.message ?? "rate limit reached", {
          errorKind: "rate_limit",
          resets_at: turn.rateLimit.resets_at,
        });
        return;
      }
      sendResult(id, {
        stopReason: wasCancelled ? "cancelled" : (turn.stopReason ?? "end_turn"),
      });
      return;
    }

    default:
      sendError(id, -32601, `fakeAcpAgent: method '${method}' not implemented`);
  }
}

async function main() {
  fakeDebug("main() entry");
  const rl = createInterface({ input: process.stdin });
  process.stdin.on("end", () => fakeDebug("stdin end"));
  process.stdin.on("close", () => fakeDebug("stdin close"));
  process.stdin.on("error", (err) => fakeDebug(`stdin error: ${err.code ?? err.message}`));
  rl.on("line", async (line) => {
    const trimmed = line.trim();
    if (!trimmed) return;
    let msg;
    try {
      msg = JSON.parse(trimmed);
    } catch (err) {
      process.stderr.write(`[fakeAcpAgent] bad JSON: ${err}\n`);
      return;
    }
    if (msg.id !== undefined && msg.method) {
      try {
        await handleRequest(msg);
      } catch (err) {
        process.stderr.write(`[fakeAcpAgent] handler error: ${err}\n`);
        sendError(msg.id, -32603, `internal: ${err}`);
      }
    } else if (msg.id !== undefined && (msg.result !== undefined || msg.error !== undefined)) {
      // Response to one of our outbound requests (e.g.
      // session/request_permission). Resolve the awaiting Promise.
      resolveOutbound(msg);
    } else if (msg.method) {
      // Notification from client. session/cancel is the one we model:
      // per ACP spec it's a notification (no id), and the in-flight
      // session/prompt MUST be aborted with stopReason="cancelled".
      // Other notifications (fs/* responses, etc) are ignored; tests
      // that need them script their turns to avoid triggering tool
      // calls.
      if (msg.method === "session/cancel") {
        const sid = msg.params?.sessionId;
        if (sid) cancelFlags.set(sid, true);
      } else {
        process.stderr.write(`[fakeAcpAgent] received notification: ${msg.method}\n`);
      }
    }
  });

  rl.on("close", () => {
    fakeDebug("readline close, exiting 0");
    process.exit(0);
  });
}

main().catch((err) => {
  process.stderr.write(`[fakeAcpAgent] fatal: ${err}\n`);
  process.exit(1);
});
