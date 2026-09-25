#!/usr/bin/env node
// Fake ACP agent (newline-delimited JSON-RPC) for live structured view tests.
//
// FAKE_ACP_SCRIPT names a JSON file: { turns: [{ updates: [...session/update], stopReason }] }.
// Each session/prompt consumes one turn; afterwards prompts get a default one-chunk turn. Pseudo
// updates: wait_for_release (waits for `<script>.release`), wait_ms, permission_request and
// elicitation_request (sent as real client requests and awaited).

import { createInterface } from "node:readline";
import { readFileSync, existsSync, appendFileSync, writeFileSync } from "node:fs";
import { randomBytes } from "node:crypto";

// Write failures mean the peer is gone; an uncaught EPIPE would kill the agent mid-turn.
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
// Registering a listener suppresses Node's exit, so log the cause and exit explicitly.
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

// FAKE_ACP_TURN_STATE persists the turn cursor across respawns (for example rate-limit resume).
const TURN_STATE_PATH = process.env.FAKE_ACP_TURN_STATE;
let turnCursor = (() => {
  if (!TURN_STATE_PATH || !existsSync(TURN_STATE_PATH)) return 0;
  const n = Number.parseInt(readFileSync(TURN_STATE_PATH, "utf8").trim(), 10);
  return Number.isFinite(n) && n >= 0 ? n : 0;
})();

function nextTurn() {
  const turn = turnCursor < script.turns.length ? script.turns[turnCursor++] : DEFAULT_TURN;
  if (TURN_STATE_PATH) {
    try {
      writeFileSync(TURN_STATE_PATH, String(turnCursor));
    } catch (err) {
      process.stderr.write(`[fakeAcpAgent] failed to persist turn cursor: ${err}\n`);
    }
  }
  return turn;
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

let nextOutboundId = 1;
const pendingOutbound = new Map();

// Time out rather than stall the script's remaining updates.
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

// Every PermissionOptionKind aoe's pick_option_id consults.
const DEFAULT_PERMISSION_OPTIONS = [
  { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
  { optionId: "allow-always", name: "Allow always", kind: "allow_always" },
  { optionId: "reject-once", name: "Reject once", kind: "reject_once" },
  { optionId: "reject-always", name: "Reject always", kind: "reject_always" },
];

// Polled by the in-flight prompt loop so a cancel stops the scripted updates.
const cancelFlags = new Map();

// FAKE_ACP_MODE_VIA_CONFIG_OPTION: truthy uses OpenCode's config-only mode channel; `codex`
// advertises modes through both channels.
const OPENCODE_MODE_CHOICES = [
  { value: "build", name: "Build" },
  { value: "plan", name: "Plan" },
];
const CODEX_MODE_CHOICES = [
  {
    value: "read-only",
    name: "Read-only",
    description: "Requires approval to edit files and run commands.",
  },
  {
    value: "agent",
    name: "Agent",
    description: "Read and edit files, and run commands.",
  },
  {
    value: "agent-full-access",
    name: "Agent (full access)",
    description: "Edit outside the workspace and use network access.",
  },
];
const modeBySession = new Map();

function modeFixture() {
  if (process.env.FAKE_ACP_MODE_VIA_CONFIG_OPTION === "codex") {
    return {
      choices: CODEX_MODE_CHOICES,
      defaultValue: "agent",
      includeSessionModes: true,
    };
  }
  if (process.env.FAKE_ACP_MODE_VIA_CONFIG_OPTION) {
    return {
      choices: OPENCODE_MODE_CHOICES,
      defaultValue: "build",
      includeSessionModes: false,
    };
  }
  return null;
}

function makeModeOption(currentValue, fixture) {
  return {
    id: "mode",
    name: "Session Mode",
    category: "mode",
    type: "select",
    currentValue,
    options: fixture.choices,
  };
}

// Selectors shipped in session/new, load, and fork responses; undefined when
// FAKE_ACP_EMIT_CONFIG_OPTIONS is "0".
function buildSessionConfigOptions(sessionId) {
  if (process.env.FAKE_ACP_EMIT_CONFIG_OPTIONS === "0") return undefined;
  const configOptions = [
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
  const fixture = modeFixture();
  if (fixture) {
    const current = modeBySession.get(sessionId) ?? fixture.defaultValue;
    modeBySession.set(sessionId, current);
    configOptions.push(makeModeOption(current, fixture));
  }
  return configOptions;
}

function buildSessionModes(sessionId) {
  const fixture = modeFixture();
  if (!fixture?.includeSessionModes) return undefined;
  const current = modeBySession.get(sessionId) ?? fixture.defaultValue;
  modeBySession.set(sessionId, current);
  return {
    availableModes: fixture.choices.map(({ value, name, description }) => ({
      id: value,
      name,
      ...(description ? { description } : {}),
    })),
    currentModeId: current,
  };
}

async function emitSessionUpdates(sessionId, updates) {
  for (const u of updates) {
    if (cancelFlags.get(sessionId)) return;
    if (u && u.sessionUpdate === "wait_for_release") {
      const releasePath = `${process.env.FAKE_ACP_SCRIPT}.release`;
      while (!existsSync(releasePath)) {
        if (cancelFlags.get(sessionId)) return;
        await new Promise((resolve) => setTimeout(resolve, 20));
      }
      continue;
    }
    if (u && u.sessionUpdate === "wait_ms") {
      // Clamped to 0..60s so bad fixture data cannot fire instantly or hang CI; sliced so a cancel lands promptly.
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
      const permissionResponse = await sendRequest("session/request_permission", {
        sessionId,
        toolCall: u.toolCall ?? {
          toolCallId: `fake-tool-call-${Date.now()}`,
          title: "fake tool call",
          kind: "edit",
        },
        options: u.options ?? DEFAULT_PERMISSION_OPTIONS,
      }).catch((err) => {
        process.stderr.write(`[fakeAcpAgent] permission_request rejected: ${JSON.stringify(err)}\n`);
        return undefined;
      });
      // `echoDecision` puts the chosen option id in the transcript.
      if (u.echoDecision) {
        const outcome = permissionResponse?.outcome;
        const picked = outcome?.outcome === "selected" ? outcome.optionId : "cancelled";
        sendNotification("session/update", {
          sessionId,
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: `permission_option=${picked}` },
          },
        });
        await new Promise((resolve) => setTimeout(resolve, 5));
      }
      continue;
    }
    if (u && u.sessionUpdate === "elicitation_request") {
      await sendRequest("elicitation/create", {
        mode: "form",
        sessionId,
        message: u.message ?? "Pick one",
        requestedSchema: u.requestedSchema ?? {
          type: "object",
          properties: {
            question_0: {
              type: "string",
              title: u.message ?? "Pick one",
              oneOf: [
                { const: "Yes", title: "Yes" },
                { const: "No", title: "No" },
              ],
            },
          },
        },
      }).catch((err) => {
        process.stderr.write(`[fakeAcpAgent] elicitation/create rejected: ${JSON.stringify(err)}\n`);
      });
      continue;
    }
    sendNotification("session/update", { sessionId, update: u });
    // A 1ms tick lost first chunks under CI contention.
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
    // Only `fork` is consulted by aoe; the rest mirror the real adapter.
    sessionCapabilities: {
      fork: {},
      resume: {},
      list: {},
      close: {},
      delete: {},
    },
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
    // Must stay at or above CLAUDE_AGENT_ACP_MIN_VERSION in src/acp/agent_compat.rs.
    version: "0.55.0",
  },
  // Omitted rather than empty: some clients read an empty list as auth required.
};

// FAKE_ACP_IMPERSONATE: the version gate keys off the spawned binary, so report that agent's
// name and a version at its floor. FAKE_ACP_STEERING advertises `_session/steering` at the
// separate steering floor.
const STEERING_ENABLED = process.env.FAKE_ACP_STEERING === "1";
const STEERING_MIN_VERSION = "0.64.0";

// Running prompts decide `injected` versus `promptRequired`, as the real adapter's turnQueue does.
const activeTurns = new Set();

function resolveAgentInfo() {
  if (process.env.FAKE_ACP_IMPERSONATE === "opencode") {
    return { name: "OpenCode", version: "1.16.0" };
  }
  if (process.env.FAKE_ACP_IMPERSONATE === "codex") {
    return { name: "@agentclientprotocol/codex-acp", version: "1.1.9" };
  }
  if (STEERING_ENABLED) {
    return { ...INITIALIZE_RESULT.agentInfo, version: STEERING_MIN_VERSION };
  }
  return INITIALIZE_RESULT.agentInfo;
}

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
    } catch {}
  }
  // script.failOn = { method, code, message, data, repeat? } rejects that method once (or always).
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
      // script.promptCapabilities overrides the all-false defaults.
      const result = script.promptCapabilities
        ? {
            ...INITIALIZE_RESULT,
            agentInfo: resolveAgentInfo(),
            agentCapabilities: {
              ...INITIALIZE_RESULT.agentCapabilities,
              promptCapabilities: {
                ...INITIALIZE_RESULT.agentCapabilities.promptCapabilities,
                ...script.promptCapabilities,
              },
            },
          }
        : { ...INITIALIZE_RESULT, agentInfo: resolveAgentInfo() };
      if (STEERING_ENABLED) {
        result._meta = { steering: { supported: true } };
      }
      sendResult(id, result);
      return;
    }

    // Content steered after the turn settled is `promptRequired`: not consumed, the host resends it.
    case "_session/steering": {
      const sessionId = params?.sessionId;
      if (!STEERING_ENABLED) {
        sendError(id, -32601, "method not found");
        return;
      }
      if (!activeTurns.has(sessionId)) {
        sendResult(id, { outcome: "promptRequired", reason: "noRunningTurn" });
        return;
      }
      const text = (params?.prompt ?? [])
        .filter((b) => b?.type === "text")
        .map((b) => b.text)
        .join("");
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          sessionId,
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: `steered: ${text}` },
          },
        },
      });
      sendResult(id, { outcome: "injected" });
      return;
    }

    case "session/new":
    case "session/load": {
      const sessionId = params?.sessionId ?? makeSessionId();
      // FAKE_ACP_COMMANDS: emit available_commands_update after session/new.
      const commandsJson = process.env.FAKE_ACP_COMMANDS;
      if (commandsJson) {
        try {
          const parsed = JSON.parse(commandsJson);
          if (Array.isArray(parsed) && parsed.length > 0) {
            // `input: { hint }` only for commands that accept input.
            const availableCommands = parsed.map((c) => ({
              name: c.name,
              description: c.description ?? "",
              ...(c.accepts_input ? { input: { hint: c.hint ?? "" } } : {}),
            }));
            // After the response, which is what binds the session id.
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
      // Selectors ship in the response, not a notification. Codex's LoadSessionResponse has no sessionId.
      const result = method === "session/load" && process.env.FAKE_ACP_IMPERSONATE === "codex" ? {} : { sessionId };
      const configOptions = buildSessionConfigOptions(sessionId);
      if (configOptions) result.configOptions = configOptions;
      const modes = buildSessionModes(sessionId);
      if (modes) result.modes = modes;
      // FAKE_ACP_LOAD_REPLAY(_USER): replay history on session/load like claude-agent-acp.
      const loadReplay = process.env.FAKE_ACP_LOAD_REPLAY;
      const replayLoadHistory = () => {
        const userReplay = process.env.FAKE_ACP_LOAD_REPLAY_USER;
        if (userReplay) {
          sendNotification("session/update", {
            sessionId,
            update: {
              sessionUpdate: "user_message_chunk",
              content: { type: "text", text: userReplay },
            },
          });
        }
        sendNotification("session/update", {
          sessionId,
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: loadReplay },
          },
        });
      };
      const replayBeforeResponse = process.env.FAKE_ACP_LOAD_REPLAY_BEFORE_RESPONSE === "1";
      if (method === "session/load" && loadReplay && replayBeforeResponse) {
        replayLoadHistory();
      }
      sendResult(id, result);
      if (method === "session/load" && loadReplay && !replayBeforeResponse) {
        setImmediate(replayLoadHistory);
      }
      return;
    }

    case "session/fork": {
      // FAKE_ACP_FORK_FAIL rejects session/fork as a permanent agent-side failure.
      if (process.env.FAKE_ACP_FORK_FAIL) {
        sendError(id, -32000, "fork failed: parent session not found");
        return;
      }
      // A fresh `fork-` id proves session/fork, not session/new, minted the session.
      const forkedId = `fake-acp-fork-${randomBytes(4).toString("hex")}`;
      const result = { sessionId: forkedId };
      const configOptions = buildSessionConfigOptions(forkedId);
      if (configOptions) result.configOptions = configOptions;
      const modes = buildSessionModes(forkedId);
      if (modes) result.modes = modes;
      sendResult(id, result);
      return;
    }

    case "session/setMode":
    case "session/set_mode": {
      // Also accepts the legacy camelCase spelling.
      const sessionId = params?.sessionId;
      const modeId = params?.modeId;
      const fixture = modeFixture();
      if (fixture && !fixture.choices.some((m) => m.value === modeId)) {
        sendError(id, -32000, `mode not found: ${modeId}`);
        return;
      }
      sendResult(id, {});
      if (sessionId && modeId) {
        if (fixture?.includeSessionModes) {
          // codex-acp sends no current_mode_update, leaving the config snapshot stale.
          modeBySession.set(sessionId, modeId);
          return;
        }
        await emitSessionUpdates(sessionId, [{ sessionUpdate: "current_mode_update", currentModeId: modeId }]);
      }
      return;
    }

    case "session/set_config_option": {
      // Like claude-agent-acp, respond with the new configOptions and send no notification.
      // FAKE_ACP_REJECT_CONFIG_OPTION rejects instead.
      const configId = params?.configId;
      const value = params?.value;
      if (process.env.FAKE_ACP_REJECT_CONFIG_OPTION) {
        sendError(id, -32000, process.env.FAKE_ACP_REJECT_CONFIG_OPTION);
        return;
      }
      const fixture = modeFixture();
      if (fixture && configId === "mode") {
        if (!fixture.choices.some((m) => m.value === value)) {
          sendError(id, -32000, `mode not found: ${value}`);
          return;
        }
        const sessionId = params?.sessionId;
        if (sessionId) modeBySession.set(sessionId, value);
        sendResult(id, { configOptions: [makeModeOption(value, fixture)] });
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
      fakeDebug(`session/prompt prompt=${JSON.stringify(params?.prompt ?? [])}`);
      const turn = nextTurn();
      if (sessionId) cancelFlags.set(sessionId, false);
      if (sessionId) {
        activeTurns.add(sessionId);
        try {
          await emitSessionUpdates(sessionId, turn.updates);
        } finally {
          activeTurns.delete(sessionId);
        }
      }
      const wasCancelled = sessionId ? cancelFlags.get(sessionId) : false;
      if (sessionId) cancelFlags.set(sessionId, false);
      // A `rateLimit` turn fails session/prompt with only `errorKind`, like claude-agent-acp; a reset
      // time must come from a usage_update in `updates`. A cancel still wins.
      if (!wasCancelled && turn.rateLimit) {
        sendError(id, -32000, turn.rateLimit.message ?? "rate limit reached", {
          errorKind: "rate_limit",
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
      resolveOutbound(msg);
    } else if (msg.method) {
      // Only session/cancel is modelled among notifications.
      if (msg.method === "session/cancel") {
        const sid = msg.params?.sessionId;
        fakeDebug(`session/cancel sessionId=${sid ?? ""}`);
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
