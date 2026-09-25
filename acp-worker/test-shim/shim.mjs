#!/usr/bin/env node
/**
 * Scripted ACP agent for structured-view integration tests; calls no model.
 * A plain prompt echoes it, runs one read tool call, and ends with "done".
 * Prompt keywords and SHIM_* env vars select the scenarios below.
 */

import * as acp from "@agentclientprotocol/sdk";
import net from "node:net";
import { appendFile, access, writeFile } from "node:fs/promises";
import { Duplex, Readable, Writable } from "node:stream";

// One shim process serves one ACP connection, so module state is per connection.
const sessions = new Map();
// Resolves a parked prompt when session/cancel arrives.
let parkedPromptResolve = null;
// Current value of the SHIM_THOUGHT_LEVEL config option.
let thoughtLevel = "medium";

// SHIM_PRESEED_SESSION_ID: a known session for a Resume attach that skips session/new.
if (process.env.SHIM_PRESEED_SESSION_ID) {
  sessions.set(process.env.SHIM_PRESEED_SESSION_ID, {});
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function waitForFile(path) {
  while (true) {
    try {
      await access(path);
      return;
    } catch {
      await sleep(10);
    }
  }
}

async function record(envVar, line) {
  const file = process.env[envVar];
  if (file) await appendFile(file, line);
}

function park() {
  return new Promise((resolve) => {
    parkedPromptResolve = resolve;
  }).then(() => ({ stopReason: "cancelled" }));
}

const textContent = (text) => [{ type: "content", content: { type: "text", text } }];
const usage = (used, cost) => ({
  sessionUpdate: "usage_update",
  used,
  size: 200000,
  ...(cost ? { cost: { amount: 0.01, currency: "USD" } } : {}),
});

/**
 * SHIM_EMIT_UNSOLICITED_NOTIF=<delay ms>: after a Resume reattach, emit one
 * mid-turn chunk with no prompt, then stay silent. SHIM_UNSOLICITED_RELEASE_FILE
 * holds the emit until that file exists.
 */
function emitUnsolicitedNotifIfRequested(client) {
  const raw = process.env.SHIM_EMIT_UNSOLICITED_NOTIF;
  const sessionId = process.env.SHIM_PRESEED_SESSION_ID;
  if (raw === undefined || !sessionId) return;
  const delayMs = Number.parseInt(raw, 10);
  setTimeout(
    async () => {
      const release = process.env.SHIM_UNSOLICITED_RELEASE_FILE;
      if (release) await waitForFile(release);
      client
        .notify("session/update", {
          sessionId,
          update: {
            sessionUpdate: "agent_message_chunk",
            content: { type: "text", text: "mid-turn chunk after reattach" },
          },
        })
        .catch(() => {});
    },
    Number.isFinite(delayMs) ? delayMs : 150,
  );
}

function handleInitialize(params) {
  // SHIM_LOAD_SESSION=1 advertises loadSession.
  const agentCapabilities = { loadSession: process.env.SHIM_LOAD_SESSION === "1" };
  // SHIM_DELETE_CAPABILITY=1 advertises session/delete.
  if (process.env.SHIM_DELETE_CAPABILITY === "1") {
    agentCapabilities.sessionCapabilities = { delete: {} };
  }
  // SHIM_MCP_CAPABILITY: comma list of "http" / "sse" MCP transports.
  if (process.env.SHIM_MCP_CAPABILITY) {
    const caps = process.env.SHIM_MCP_CAPABILITY.split(",").map((s) => s.trim());
    agentCapabilities.mcpCapabilities = {
      http: caps.includes("http"),
      sse: caps.includes("sse"),
    };
  }
  return {
    protocolVersion: params.protocolVersion ?? acp.PROTOCOL_VERSION,
    agentCapabilities,
    agentInfo: {
      name: "@agentclientprotocol/claude-agent-acp",
      // At or above the floor in src/acp/agent_compat.rs.
      version: "0.55.0",
    },
  };
}

/**
 * session/delete (only with SHIM_DELETE_CAPABILITY=1; otherwise the SDK answers
 * method_not_found). SHIM_DELETE_MODE: success | slow | error.
 * SHIM_DELETE_RECORD_FILE records each requested session id.
 */
async function handleDeleteSession(params) {
  const mode = process.env.SHIM_DELETE_MODE ?? "success";
  await record("SHIM_DELETE_RECORD_FILE", `${params.sessionId}\n`);
  if (mode === "slow") await sleep(3000);
  if (mode === "error") {
    throw acp.RequestError.internalError({}, "shim deliberate failure");
  }
  return {};
}

// SHIM_THOUGHT_LEVEL=1 advertises one thought-level select. SHIM_MODEL_OPTION=1
// adds a `category:"model"` picker that rejects unknown values and resets on
// session/new.
let model = "default";
const MODEL_VALUES = ["default", "opus", "sonnet"];
// SHIM_RENUMBER_ON_MODEL=1 renames the thought-level option after a model
// switch, as an adapter that rebuilds its option set around the switch does. A
// client that re-reads the id from the set-model response follows it; one that
// kept the establish-time id addresses an option that no longer exists.
let thoughtLevelId = "thought_level";

function configOptions() {
  const options = [];
  if (process.env.SHIM_MODEL_OPTION === "1") {
    options.push({
      id: "model",
      name: "Model",
      category: "model",
      type: "select",
      currentValue: model,
      options: MODEL_VALUES.map((value) => ({ value, name: value })),
    });
  }
  if (process.env.SHIM_THOUGHT_LEVEL === "1") {
    options.push({
      id: thoughtLevelId,
      name: "Thinking",
      category: "thought_level",
      type: "select",
      currentValue: thoughtLevel,
      options: [
        { value: "medium", name: "Medium" },
        { value: "high", name: "High" },
      ],
    });
  }
  return options.length > 0 ? options : undefined;
}

// SHIM_CONFIG_OPTION_RECORD_FILE records `<configId>=<value>` per call.
async function handleSetConfigOption(params) {
  await record("SHIM_CONFIG_OPTION_RECORD_FILE", `${params.configId}=${params.value}\n`);
  if (params.configId === thoughtLevelId) thoughtLevel = params.value;
  if (params.configId === "model") {
    if (!MODEL_VALUES.includes(params.value)) throw new Error(`unknown model ${params.value}`);
    model = params.value;
    if (process.env.SHIM_RENUMBER_ON_MODEL === "1") thoughtLevelId = "thought_level_v2";
  }
  return { configOptions: configOptions() ?? [] };
}

function withConfigOptions(response) {
  const options = configOptions();
  return options ? { ...response, configOptions: options } : response;
}

// session/load, registered only with SHIM_LOAD_SESSION=1.
function handleLoadSession(params) {
  sessions.set(params.sessionId, {});
  return withConfigOptions({});
}

// SHIM_MCP_RECORD_FILE captures the mcpServers forwarded on session/new.
async function handleNewSession(params) {
  const recordFile = process.env.SHIM_MCP_RECORD_FILE;
  if (recordFile) await writeFile(recordFile, JSON.stringify(params?.mcpServers ?? []));
  const sessionId = "shim-" + crypto.randomUUID();
  sessions.set(sessionId, {});
  // A fresh session starts on the default model, effort and option ids, as real
  // adapters do. One process serves one connection, so this is not about test
  // isolation: a conversation reset runs session/new on the SAME connection, so
  // a renamed option id or a carried-over pick would otherwise survive into the
  // fresh session and the reset path would be testing the old one.
  model = "default";
  thoughtLevel = "medium";
  thoughtLevelId = "thought_level";
  return withConfigOptions({ sessionId });
}

/** Scenarios whose turn parks until session/cancel. */
const PARKED_SCENARIOS = {
  // Wraps up its accounting (cost usage), then never answers: ends as prompt_complete.
  COST_THEN_SILENCE: [
    { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "wedged response complete" } },
    usage(300, true),
  ],
  // Goes silent with no cost marker: the watchdog orphans the turn.
  SILENCE_NO_COST: [
    { sessionUpdate: "agent_message_chunk", content: { type: "text", text: "wedged mid-response" } },
    usage(120, false),
  ],
  // A Claude async agent launch: the watchdog stays suppressed.
  ASYNC_AGENT_ORPHAN: [
    {
      sessionUpdate: "tool_call",
      toolCallId: "tc-async-agent-1",
      title: "Research async target",
      kind: "other",
      status: "pending",
      rawInput: { description: "Research target", prompt: "..." },
    },
    {
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-async-agent-1",
      status: "completed",
      content: textContent("Async agent launched successfully.\nagentId: async-test-1 (internal ID)"),
    },
  ],
  // A ScheduleWakeup whose interim update carries raw_input, then a cost marker
  // the wakeup suppression must override.
  WAKEUP_ORPHAN: [
    {
      sessionUpdate: "tool_call",
      toolCallId: "tc-wakeup-1",
      title: "ScheduleWakeup",
      kind: "other",
      status: "pending",
      rawInput: { delaySeconds: 60, reason: "test scheduled wakeup", prompt: "continue" },
    },
    {
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-wakeup-1",
      status: "in_progress",
      title: "ScheduleWakeup",
      rawInput: { delaySeconds: 60, reason: "test scheduled wakeup", prompt: "continue" },
    },
    {
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-wakeup-1",
      status: "completed",
      content: textContent("Next wakeup scheduled."),
    },
    usage(1200, true),
  ],
};

async function handlePrompt(params, client) {
  if (!sessions.has(params.sessionId)) {
    throw new Error("unknown session");
  }
  const userText = params.prompt
    .filter((c) => c.type === "text")
    .map((c) => c.text)
    .join("\n");
  const notify = (update) =>
    client.notify("session/update", { sessionId: params.sessionId, update });
  const chunk = (text) =>
    notify({ sessionUpdate: "agent_message_chunk", content: { type: "text", text } });

  // Checked in this order, as the scenarios' keywords are distinct.
  for (const keyword of ["COST_THEN_SILENCE", "SILENCE_NO_COST", "ASYNC_AGENT_ORPHAN"]) {
    if (userText.includes(keyword)) {
      for (const update of PARKED_SCENARIOS[keyword]) await notify(update);
      return park();
    }
  }

  // A run_in_background Bash; WRAP_UP adds the cost marker that ends the
  // turn's accounting, so the turn recovers instead of staying suppressed.
  if (userText.includes("BACKGROUND_BASH_ORPHAN")) {
    await notify({
      sessionUpdate: "tool_call",
      toolCallId: "tc-bg-orphan-1",
      title: "Bash",
      kind: "execute",
      status: "pending",
      rawInput: { command: "sleep 600", run_in_background: true },
    });
    await notify({
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-bg-orphan-1",
      status: "completed",
      content: textContent(
        "Command running in background with ID: btest-orphan-1. Output is being written to: /tmp/x",
      ),
    });
    if (userText.includes("WRAP_UP")) await notify(usage(1200, true));
    return park();
  }

  if (userText.includes("WAKEUP_ORPHAN")) {
    for (const update of PARKED_SCENARIOS.WAKEUP_ORPHAN) await notify(update);
    return park();
  }

  // SLOW spaces the events so a test can observe mid-turn UI.
  const pause = () => (userText.includes("SLOW") ? sleep(800) : undefined);

  // The JSON-RPC error claude-agent-acp returns on a provider quota rejection.
  if (userText.includes("RATE_LIMIT")) {
    throw acp.RequestError.internalError(
      { errorKind: "rate_limit" },
      "You've hit your limit · resets 12:10pm (Europe/Paris)",
    );
  }

  await chunk(`received: ${userText}`);
  await pause();

  if (userText.includes("USAGE_BEFORE_")) {
    await notify(usage(120, userText.includes("USAGE_BEFORE_COST")));
  }

  await notify({
    sessionUpdate: "tool_call",
    toolCallId: "tc-1",
    title: "Reading shim file",
    kind: "read",
    status: "pending",
    locations: [{ path: "/tmp/shim.txt" }],
    rawInput: { path: "/tmp/shim.txt" },
  });
  await pause();

  await notify({
    sessionUpdate: "tool_call_update",
    toolCallId: "tc-1",
    status: "completed",
    rawOutput: { content: "shim file contents" },
  });
  await pause();

  if (userText.includes("USAGE_AFTER_NO_COST")) {
    await notify({ sessionUpdate: "usage_update", used: 300, size: 200000 });
  }
  if (userText.includes("USAGE_OBSERVATION")) {
    return park();
  }

  if (userText.includes("FS_READ_WRITE")) {
    try {
      const path = process.cwd() + "/shim-roundtrip.txt";
      await client.request("fs/write_text_file", {
        sessionId: params.sessionId,
        path,
        content: "hello from shim",
      });
      const read = await client.request("fs/read_text_file", {
        sessionId: params.sessionId,
        path,
      });
      await chunk(`fs_read=${read.content}`);
    } catch (err) {
      await chunk(`fs_error=${err.message ?? err}`);
    }
  }

  if (userText.includes("TERMINAL_RUN")) {
    try {
      const { terminalId } = await client.request("terminal/create", {
        sessionId: params.sessionId,
        command: "echo",
        args: ["terminal-roundtrip-ok"],
      });
      const terminal = { sessionId: params.sessionId, terminalId };
      const exit = await client.request("terminal/wait_for_exit", terminal);
      const out = await client.request("terminal/output", terminal);
      const code = exit.exitCode ?? exit.exit_code ?? exit.exitStatus?.exitCode ?? "?";
      await chunk(`terminal_output=${out.output.trim()};exit=${code}`);
      await client.request("terminal/release", terminal).catch(() => {});
    } catch (err) {
      await chunk(`terminal_error=${err.message ?? err}`);
    }
  }

  const askPermission = async (toolCall, options) => {
    const response = await client.request("session/request_permission", {
      sessionId: params.sessionId,
      toolCall,
      options,
    });
    return response.outcome.outcome === "selected" ? response.outcome.optionId : "cancelled";
  };

  if (userText.includes("REQUEST_PERMISSION")) {
    const verdict = await askPermission(
      {
        toolCallId: "tc-2",
        title: "Modify shim config",
        kind: "edit",
        status: "pending",
        locations: [{ path: "/tmp/shim-config.json" }],
        rawInput: { path: "/tmp/shim-config.json", content: '{"x":1}' },
      },
      [
        { kind: "allow_once", name: "Allow once", optionId: "yes" },
        { kind: "reject_once", name: "Reject", optionId: "no" },
      ],
    );
    await chunk(`permission_outcome=${verdict}`);
  }

  // Every option is allow_once, so answering by kind would always pick the first.
  if (userText.includes("REQUEST_CHOICE")) {
    const names = ["Option Alpha", "Option Bravo", "Option Charlie", "Option Delta"];
    const verdict = await askPermission(
      {
        toolCallId: "tc-choice",
        title: "Pick an option",
        kind: "other",
        status: "pending",
        rawInput: { message: "Which one?" },
      },
      names.map((name, index) => ({ kind: "allow_once", name, optionId: `choice-${index}` })),
    );
    await chunk(`choice_outcome=${verdict}`);
  }

  await chunk("done");

  // SHIM_PROMPT_COMPLETION_RELEASE_FILE holds the prompt response until the file exists.
  const completionRelease = process.env.SHIM_PROMPT_COMPLETION_RELEASE_FILE;
  if (completionRelease) await waitForFile(completionRelease);
  return {
    stopReason: userText.includes("MAX_TOKENS") ? "max_tokens" : "end_turn",
  };
}

function handleCancel() {
  if (parkedPromptResolve) {
    const resolve = parkedPromptResolve;
    parkedPromptResolve = null;
    resolve();
  }
}

async function bootstrap() {
  let inputWeb;
  let outputWeb;
  // AOE_ACP_SOCKET: use that unix socket as the transport instead of stdio.
  if (process.env.AOE_ACP_SOCKET) {
    const sock = await new Promise((resolve, reject) => {
      const s = net.createConnection(process.env.AOE_ACP_SOCKET, () => resolve(s));
      s.on("error", reject);
    });
    inputWeb = Duplex.toWeb(sock).writable;
    outputWeb = Duplex.toWeb(sock).readable;
    sock.on("end", () => process.exit(0));
  } else {
    inputWeb = Writable.toWeb(process.stdout);
    outputWeb = Readable.toWeb(process.stdin);
  }
  const stream = acp.ndJsonStream(inputWeb, outputWeb);

  const app = acp
    .agent({ name: "aoe-acp-test-shim" })
    .onRequest("initialize", ({ params }) => handleInitialize(params))
    .onRequest("authenticate", () => ({}))
    .onRequest("session/new", ({ params }) => handleNewSession(params))
    .onRequest("session/set_mode", () => ({}))
    .onRequest("session/set_config_option", ({ params }) => handleSetConfigOption(params))
    .onRequest("session/prompt", ({ params, client }) => handlePrompt(params, client))
    .onNotification("session/cancel", () => handleCancel())
    .onConnect((connection) => emitUnsolicitedNotifIfRequested(connection.client));
  if (process.env.SHIM_DELETE_CAPABILITY === "1") {
    app.onRequest("session/delete", ({ params }) => handleDeleteSession(params));
  }
  if (process.env.SHIM_LOAD_SESSION === "1") {
    app.onRequest("session/load", ({ params }) => handleLoadSession(params));
  }
  app.connect(stream);

  process.stdin.on("end", () => process.exit(0));
  process.on("SIGTERM", () => process.exit(0));
  process.on("SIGINT", () => process.exit(0));
}

bootstrap().catch((err) => {
  console.error("[shim] bootstrap failed:", err);
  process.exit(1);
});
