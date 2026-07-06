#!/usr/bin/env node
/**
 * Minimal ACP agent shim for structured-view integration tests. Does NOT call any
 * model. Replays a scripted sequence of session updates so we can verify
 * the Rust ACP client end-to-end without API keys or network access.
 *
 * Behavior on `prompt`:
 *   1. Emit one `agent_message_chunk` text event echoing the prompt.
 *   2. Emit a `tool_call` event with kind=read, status=pending.
 *   3. Emit a matching `tool_call_update` with status=completed.
 *   4. Emit a final `agent_message_chunk` saying "done".
 *   5. Resolve with stopReason=end_turn.
 *
 * Used by `tests/acp_acp_smoke.rs`.
 */

import * as acp from "@agentclientprotocol/sdk";
import { Readable, Writable } from "node:stream";

// ponytail: one shim process serves exactly one ACP connection, so
// module-level state is equivalent to the old per-connection instance state.
const sessions = new Map();
// Resolver used by the SILENT_ORPHAN test mode: prompt() parks on a Promise
// that the session/cancel handler resolves so the test can assert the
// watchdog without waiting for CANCEL_ESCALATION_GRACE to elapse.
let silentOrphanResolve = null;

// SHIM_PRESEED_SESSION_ID lets a test attach to the shim via the socket
// transport with `ConnectMode::Resume` (which skips `session/new`) and still
// get a working `prompt`. Without it, the shim's prompt handler rejects
// unknown session ids.
const preseed = process.env.SHIM_PRESEED_SESSION_ID;
if (preseed) {
  sessions.set(preseed, {});
}

// SHIM_EMIT_UNSOLICITED_NOTIF reproduces a still-alive runner forwarding a
// single mid-turn notification shortly after the daemon reattaches in Resume
// mode, with no prompt issued. Used by the #1216 test to confirm the
// resume-idle watchdog disarms on the first inbound notification (instead of
// firing after normal mid-turn silence). The value is the delay in ms before
// the emit (default 150); after this one notification the shim stays silent so
// the test can assert no synthetic Stopped follows. Requires
// SHIM_PRESEED_SESSION_ID so the notification carries a known session.
function emitUnsolicitedNotifIfRequested(client) {
  const raw = process.env.SHIM_EMIT_UNSOLICITED_NOTIF;
  if (raw === undefined) return;
  const sessionId = process.env.SHIM_PRESEED_SESSION_ID;
  if (!sessionId) return;
  const delayMs = Number.parseInt(raw, 10);
  setTimeout(
    () => {
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
  const agentCapabilities = {
    loadSession: false,
  };
  // SHIM_DELETE_CAPABILITY=1 advertises sessionCapabilities.delete
  // so the Rust client's session/delete dispatch path can be
  // exercised end-to-end. Tests for #1404 toggle this; default off
  // mirrors the negative-path adapters (aoe-agent, codex, opencode).
  if (process.env.SHIM_DELETE_CAPABILITY === "1") {
    agentCapabilities.sessionCapabilities = { delete: {} };
  }
  // SHIM_MCP_CAPABILITY advertises mcpCapabilities so the Rust client's
  // http/sse capability gating can be exercised. Comma list, e.g. "http",
  // "sse", or "http,sse". Absent => neither advertised (stdio only).
  if (process.env.SHIM_MCP_CAPABILITY) {
    const caps = process.env.SHIM_MCP_CAPABILITY.split(",").map((s) =>
      s.trim(),
    );
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
      // Keep at (or above) the agent_compat floor in
      // src/acp/agent_compat.rs, or the gate rejects the shim's handshake.
      version: "0.55.0",
    },
  };
}

// session/delete handler registered only when SHIM_DELETE_CAPABILITY=1.
// Without the registration, the SDK's request dispatcher returns -32601
// method_not_found, which is the negative-path expectation aoe needs to test
// against (matches aoe-agent, codex, opencode behavior).
//
// SHIM_DELETE_MODE controls the response shape so tests can drive the success
// / timeout / failure branches without spinning up distinct shim binaries.
// SHIM_DELETE_RECORD_FILE, when set, appends one line per call with the
// requested sessionId so tests assert the RPC actually fired.
async function handleDeleteSession(params) {
  const mode = process.env.SHIM_DELETE_MODE ?? "success";
  const recordFile = process.env.SHIM_DELETE_RECORD_FILE;
  if (recordFile) {
    const fs = await import("node:fs/promises");
    await fs.appendFile(recordFile, `${params.sessionId}\n`);
  }
  if (mode === "slow") {
    await new Promise((r) => setTimeout(r, 3000));
    return {};
  }
  if (mode === "error") {
    throw acp.RequestError.internalError({}, "shim deliberate failure");
  }
  return {};
}

async function handleNewSession(params) {
  // SHIM_MCP_RECORD_FILE, when set, captures the mcp_servers the client
  // forwarded on session/new so tests assert MCP forwarding end to end.
  const recordFile = process.env.SHIM_MCP_RECORD_FILE;
  if (recordFile) {
    const fs = await import("node:fs/promises");
    await fs.writeFile(recordFile, JSON.stringify(params?.mcpServers ?? []));
  }
  const sessionId = "shim-" + crypto.randomUUID();
  sessions.set(sessionId, {});
  return { sessionId };
}

async function handlePrompt(params, client) {
  if (!sessions.has(params.sessionId)) {
    throw new Error("unknown session");
  }
  const userText = params.prompt
    .filter((c) => c.type === "text")
    .map((c) => c.text)
    .join("\n");

  // SILENT_ORPHAN reproduces the upstream
  // `agentclientprotocol/claude-agent-acp#688` failure mode for the
  // silent-orphan watchdog test in
  // tests/acp_silent_orphan.rs. Sequence:
  //   1. emit one assistant chunk
  //   2. emit a cost-populated usage_update (claude-agent-acp's
  //      "wrap up accounting" marker the daemon uses as a
  //      terminal-candidate signal)
  //   3. park until cancel() resolves the promise
  // Without the cancel handler we'd hang the test for the full
  // CANCEL_ESCALATION_GRACE; the explicit resolve keeps the test
  // under a second while still exercising the watchdog. See #1240.
  if (userText.includes("SILENT_ORPHAN")) {
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text: "wedged response complete" },
      },
    });
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "usage_update",
        input_tokens: 100,
        output_tokens: 200,
        cost: { amount: 0.01, currency: "USD" },
      },
    });
    await new Promise((resolve) => {
      silentOrphanResolve = resolve;
    });
    return { stopReason: "cancelled" };
  }

  // ASYNC_AGENT_ORPHAN reproduces the Claude SDK async-agent shape
  // for the #1360 watchdog suppression test. Sequence:
  //   1. emit tool_call for an Agent invocation
  //   2. emit tool_call_update with status=completed and content text
  //      "Async agent launched successfully. agentId: ..." (the marker
  //      the Rust classifier looks for to flip async_agent_running)
  //   3. park until cancel() resolves the promise
  // The Rust test then drains for longer than the base watchdog grace
  // and asserts NO `prompt_orphaned` Stopped frame arrived. Without
  // the async detection, the watchdog would fire ~300ms after the
  // completion; with it, the effective grace is lifted to 30 minutes
  // so the test window stays silent.
  if (userText.includes("ASYNC_AGENT_ORPHAN")) {
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call",
        toolCallId: "tc-async-agent-1",
        title: "Research async target",
        kind: "other",
        status: "pending",
        rawInput: { description: "Research target", prompt: "..." },
      },
    });
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call_update",
        toolCallId: "tc-async-agent-1",
        status: "completed",
        content: [
          {
            type: "content",
            content: {
              type: "text",
              text: "Async agent launched successfully.\nagentId: async-test-1 (internal ID)",
            },
          },
        ],
      },
    });
    await new Promise((resolve) => {
      silentOrphanResolve = resolve;
    });
    return { stopReason: "cancelled" };
  }

  // BACKGROUND_BASH_ORPHAN reproduces the #1401 shape: the Claude SDK
  // `Bash` tool fired with `run_in_background: true`. The visible
  // ToolCall completes immediately with the marker
  // "Command running in background with ID: <id>", then the prompt
  // also receives a cost-populated usage_update (the production
  // false-positive's trigger). The Rust watchdog must observe the
  // off-protocol marker and stay suppressed past the fast grace.
  if (userText.includes("BACKGROUND_BASH_ORPHAN")) {
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call",
        toolCallId: "tc-bg-orphan-1",
        title: "Bash",
        kind: "execute",
        status: "pending",
        rawInput: { command: "sleep 600", run_in_background: true },
      },
    });
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call_update",
        toolCallId: "tc-bg-orphan-1",
        status: "completed",
        content: [
          {
            type: "content",
            content: {
              type: "text",
              text: "Command running in background with ID: btest-orphan-1. Output is being written to: /tmp/x",
            },
          },
        ],
      },
    });
    // Force the daemon down the fast-grace path; if off-protocol
    // suppression is wired correctly, the watchdog should still
    // stay quiet for the full test window.
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "usage_update",
        input_tokens: 10,
        output_tokens: 10,
        cost: { amount: 0.01, currency: "USD" },
      },
    });
    await new Promise((resolve) => {
      silentOrphanResolve = resolve;
    });
    return { stopReason: "cancelled" };
  }

  // WAKEUP_ORPHAN reproduces the ScheduleWakeup path from #1401: the
  // agent registers an absolute wake-at, then idles intentionally
  // waiting for the scheduled prompt to fire. A cost-populated
  // usage_update follows so the daemon would otherwise switch to the
  // fast grace; the wakeup suppression must override it until
  // `at + base_grace` passes.
  if (userText.includes("WAKEUP_ORPHAN")) {
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call",
        toolCallId: "tc-wakeup-1",
        title: "ScheduleWakeup",
        kind: "other",
        status: "pending",
        rawInput: {
          delaySeconds: 60,
          reason: "test scheduled wakeup",
          prompt: "continue",
        },
      },
    });
    // Real claude-agent-acp lands `raw_input` on an interim
    // `tool_call_update` BEFORE the final completed frame; the
    // watchdog now requires this carrier to fire `WakeupPending`
    // (so a Failed completion doesn't blindly suppress for the
    // delay window). Mirror the real shape: emit one in-progress
    // update with raw_input.delaySeconds, then a final completed
    // update.
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call_update",
        toolCallId: "tc-wakeup-1",
        status: "in_progress",
        title: "ScheduleWakeup",
        rawInput: {
          delaySeconds: 60,
          reason: "test scheduled wakeup",
          prompt: "continue",
        },
      },
    });
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "tool_call_update",
        toolCallId: "tc-wakeup-1",
        status: "completed",
        content: [
          {
            type: "content",
            content: {
              type: "text",
              text: "Next wakeup scheduled.",
            },
          },
        ],
      },
    });
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "usage_update",
        input_tokens: 10,
        output_tokens: 10,
        cost: { amount: 0.01, currency: "USD" },
      },
    });
    await new Promise((resolve) => {
      silentOrphanResolve = resolve;
    });
    return { stopReason: "cancelled" };
  }

  // Optional slow path: tests that need to observe mid-turn UI
  // (e.g. the working spinner) include "SLOW" in the prompt so the
  // shim adds a configurable delay between events.
  const slow = userText.includes("SLOW");
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // Optional rate-limit path: tests for #1281 include "RATE_LIMIT"
  // in the prompt so the shim returns the same JSON-RPC error shape
  // claude-agent-acp emits when the Anthropic API rejects a request
  // for quota reasons. Uses the SDK's RequestError so the structured
  // `data` field reaches the wire (a plain `throw new Error(...)`
  // would be stringified into the message). The Rust ACP client
  // must classify this as RateLimit + Stopped{rate_limited} instead
  // of treating it as a worker crash.
  if (userText.includes("RATE_LIMIT")) {
    throw acp.RequestError.internalError(
      { errorKind: "rate_limit" },
      "You've hit your limit · resets 12:10pm (Europe/Paris)",
    );
  }

  await client.notify("session/update", {
    sessionId: params.sessionId,
    update: {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: `received: ${userText}` },
    },
  });
  if (slow) await sleep(800);

  await client.notify("session/update", {
    sessionId: params.sessionId,
    update: {
      sessionUpdate: "tool_call",
      toolCallId: "tc-1",
      title: "Reading shim file",
      kind: "read",
      status: "pending",
      locations: [{ path: "/tmp/shim.txt" }],
      rawInput: { path: "/tmp/shim.txt" },
    },
  });
  if (slow) await sleep(800);

  await client.notify("session/update", {
    sessionId: params.sessionId,
    update: {
      sessionUpdate: "tool_call_update",
      toolCallId: "tc-1",
      status: "completed",
      rawOutput: { content: "shim file contents" },
    },
  });
  if (slow) await sleep(800);

  // Optional fs round-trip exercised by tests via prompt keywords.
  if (userText.includes("FS_READ_WRITE")) {
    try {
      // Write a fresh file inside the session cwd.
      await client.request("fs/write_text_file", {
        sessionId: params.sessionId,
        path: process.cwd() + "/shim-roundtrip.txt",
        content: "hello from shim",
      });
      // Read it back.
      const read = await client.request("fs/read_text_file", {
        sessionId: params.sessionId,
        path: process.cwd() + "/shim-roundtrip.txt",
      });
      await client.notify("session/update", {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: `fs_read=${read.content}` },
        },
      });
    } catch (err) {
      await client.notify("session/update", {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: `fs_error=${err.message ?? err}` },
        },
      });
    }
  }

  // Optional terminal round-trip exercised by tests.
  if (userText.includes("TERMINAL_RUN")) {
    try {
      const { terminalId } = await client.request("terminal/create", {
        sessionId: params.sessionId,
        command: "echo",
        args: ["terminal-roundtrip-ok"],
      });
      const exit = await client.request("terminal/wait_for_exit", {
        sessionId: params.sessionId,
        terminalId,
      });
      const out = await client.request("terminal/output", {
        sessionId: params.sessionId,
        terminalId,
      });
      // WaitForTerminalExitResponse flattens TerminalExitStatus, so
      // exitCode is at the top level. Fall back to nested in case the
      // SDK wraps it differently in a future version.
      const code =
        exit.exitCode ?? exit.exit_code ?? exit.exitStatus?.exitCode ?? "?";
      await client.notify("session/update", {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          content: {
            type: "text",
            text: `terminal_output=${out.output.trim()};exit=${code}`,
          },
        },
      });
      await client
        .request("terminal/release", {
          sessionId: params.sessionId,
          terminalId,
        })
        .catch(() => {});
    } catch (err) {
      await client.notify("session/update", {
        sessionId: params.sessionId,
        update: {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: `terminal_error=${err.message ?? err}` },
        },
      });
    }
  }

  // Optional permission request, controlled by prompt content so tests
  // can opt into exercising the approval round-trip.
  if (userText.includes("REQUEST_PERMISSION")) {
    const response = await client.request("session/request_permission", {
      sessionId: params.sessionId,
      toolCall: {
        toolCallId: "tc-2",
        title: "Modify shim config",
        kind: "edit",
        status: "pending",
        locations: [{ path: "/tmp/shim-config.json" }],
        rawInput: {
          path: "/tmp/shim-config.json",
          content: '{"x":1}',
        },
      },
      options: [
        { kind: "allow_once", name: "Allow once", optionId: "yes" },
        { kind: "reject_once", name: "Reject", optionId: "no" },
      ],
    });
    const verdict =
      response.outcome.outcome === "selected"
        ? response.outcome.optionId
        : "cancelled";
    await client.notify("session/update", {
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "agent_message_chunk",
        content: { type: "text", text: `permission_outcome=${verdict}` },
      },
    });
  }

  await client.notify("session/update", {
    sessionId: params.sessionId,
    update: {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text: "done" },
    },
  });

  return { stopReason: "end_turn" };
}

function handleCancel() {
  // Unstick the SILENT_ORPHAN park so prompt() returns and the
  // daemon's prompt_fut resolves. Other prompt branches finish
  // synchronously so this is a no-op for them.
  if (silentOrphanResolve) {
    const resolve = silentOrphanResolve;
    silentOrphanResolve = null;
    resolve();
  }
}

// AOE_ACP_SOCKET: when set, connect to that unix socket as the
// transport instead of using stdio. Used by sandboxed structured-view sessions
// (Docker bind-mounts the socket into the container) and for
// integration tests that exercise the socket transport.
import net from "node:net";
import { Duplex } from "node:stream";

async function bootstrap() {
  let inputWeb;
  let outputWeb;
  if (process.env.AOE_ACP_SOCKET) {
    const sock = await new Promise((resolve, reject) => {
      const s = net.createConnection(process.env.AOE_ACP_SOCKET, () =>
        resolve(s),
      );
      s.on("error", reject);
    });
    // The unix socket is a single bidirectional stream. acp.ndJsonStream
    // expects (writable, readable) so we hand it the socket twice via
    // Duplex.toWeb on both halves.
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
    .onRequest("session/prompt", ({ params, client }) =>
      handlePrompt(params, client),
    )
    .onNotification("session/cancel", () => handleCancel())
    .onConnect((connection) => emitUnsolicitedNotifIfRequested(connection.client));
  if (process.env.SHIM_DELETE_CAPABILITY === "1") {
    app.onRequest("session/delete", ({ params }) => handleDeleteSession(params));
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
