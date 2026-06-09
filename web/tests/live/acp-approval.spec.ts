// Structured view approval flow.
//
// Custom FAKE_ACP_SCRIPT (written to a temp file before spawning the
// harness) emits a `permission_request` mid-turn. Seeds the session via
// `aoe add` BEFORE serve boots so the server picks it up in-memory.

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test as base, expect } from "@playwright/test";
import { spawnAoeServe, listSessions, seedSessionViaAoeAdd } from "../helpers/aoeServe";
import { enableStructuredViewAndWait } from "../helpers/acp";

const APPROVAL_SCRIPT = {
  turns: [
    {
      updates: [
        {
          sessionUpdate: "agent_message_chunk",
          content: { type: "text", text: "Considering write..." },
        },
        {
          // The fake translates this into a real
          // `session/request_permission` JSON-RPC request. ACP has no
          // `permission_request` session/update variant; aoe carries
          // permissions on a separate request that emits an
          // ApprovalRequested event server-side with a server-generated
          // nonce. The spec reads that nonce out of replay below.
          sessionUpdate: "permission_request",
          toolCall: {
            toolCallId: "fake-tool-call-1",
            title: "Write file",
            kind: "edit",
          },
        },
      ],
      stopReason: "end_turn",
    },
  ],
};

interface ReplayFrame {
  seq?: number;
  event?: {
    ApprovalRequested?: {
      approval?: { nonce?: string; tool_call?: { args_preview?: string } };
    };
    ToolCallStarted?: { tool_call?: { id?: string } };
    ToolCallCompleted?: { tool_call_id?: string; is_error?: boolean };
  };
}

async function fetchFrames(baseUrl: string, sessionId: string): Promise<ReplayFrame[]> {
  const replay = await fetch(`${baseUrl}/api/sessions/${sessionId}/acp/replay?since=0`).then((r) => r.json());
  return Array.isArray(replay) ? replay : (replay.frames ?? []);
}

base("permission_request flows through to the server", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-acp-script-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(APPROVAL_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-approval" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;

    // `structured view/enable` implicitly spawns the structured view supervisor;
    // `enableStructuredViewAndWait` POSTs it and blocks until the ACP
    // handshake (initialize + session/new) completes.
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "write a file" }),
    });

    // Poll the disk-backed replay endpoint for the ApprovalRequested
    // event aoe emits when the fake agent sends `session/request_permission`.
    // The nonce is generated server-side (src/acp/permissions.rs::
    // build_approval), so the spec must read it back instead of
    // hard-coding a value.
    let nonce: string | undefined;
    await expect
      .poll(
        async () => {
          const frames = await fetchFrames(serve.baseUrl, sessionId);
          for (const frame of frames) {
            const candidate = frame.event?.ApprovalRequested?.approval?.nonce;
            if (candidate) {
              nonce = candidate;
              return true;
            }
          }
          return false;
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBe(true);
    expect(nonce).toBeDefined();

    // #1713: the permission request ships no raw_input, so the approval
    // card's args_preview must be empty (the UI renders a clean
    // empty-state) rather than the literal string "null".
    const frames = await fetchFrames(serve.baseUrl, sessionId);
    const approvalFrame = frames.find((f) => f.event?.ApprovalRequested?.approval?.nonce === nonce);
    expect(approvalFrame?.event?.ApprovalRequested?.approval?.tool_call?.args_preview).toBe("");

    // #1713 (proposal A): the permission handler must emit a
    // ToolCallStarted for this tool BEFORE the ApprovalRequested, so the
    // approved tool has a transcript card before it completes.
    const startFrame = frames.find((f) => f.event?.ToolCallStarted?.tool_call?.id === "fake-tool-call-1");
    expect(startFrame).toBeDefined();
    expect(startFrame!.seq!).toBeLessThan(approvalFrame!.seq!);

    // Resolve via the explicit endpoint (UI click path is covered by a
    // follow-up under #1224 once structured view UI selectors are stable).
    const resolveRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/approvals/${nonce}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      // ApprovalDecisionWire (src/acp/protocol.rs) is serialized
      // as PascalCase, so "allow" deserializes as a 422 invalid body.
      body: JSON.stringify({ decision: "Allow" }),
    });
    expect(resolveRes.status).toBeGreaterThanOrEqual(200);
    expect(resolveRes.status).toBeLessThan(300);
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});

base("denied permission closes the tool card with an error completion (#1713)", async ({}, testInfo) => {
  const scriptDir = mkdtempSync(join(tmpdir(), "aoe-pw-acp-script-"));
  const scriptPath = join(scriptDir, "script.json");
  writeFileSync(scriptPath, JSON.stringify(APPROVAL_SCRIPT));

  const serve = await spawnAoeServe({
    authMode: "none",
    acp: true,
    fakeAcpScript: scriptPath,
    workerIndex: testInfo.workerIndex,
    parallelIndex: testInfo.parallelIndex,
    seedFn: seedSessionViaAoeAdd({ title: "acp-approval-deny" }),
  });

  try {
    const sessions = await listSessions(serve.baseUrl);
    const sessionId = sessions[0]!.id;
    await enableStructuredViewAndWait(serve.baseUrl, sessionId);
    await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/prompt`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ text: "write a file" }),
    });

    let nonce: string | undefined;
    await expect
      .poll(
        async () => {
          const frames = await fetchFrames(serve.baseUrl, sessionId);
          for (const frame of frames) {
            const candidate = frame.event?.ApprovalRequested?.approval?.nonce;
            if (candidate) {
              nonce = candidate;
              return true;
            }
          }
          return false;
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBe(true);
    expect(nonce).toBeDefined();

    const resolveRes = await fetch(`${serve.baseUrl}/api/sessions/${sessionId}/acp/approvals/${nonce}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ decision: "Deny" }),
    });
    expect(resolveRes.status).toBeGreaterThanOrEqual(200);
    expect(resolveRes.status).toBeLessThan(300);

    // The denied tool will never run, so the start frame emitted at
    // permission time must be closed with a terminal error completion;
    // otherwise the card hangs on "running" forever.
    await expect
      .poll(
        async () => {
          const frames = await fetchFrames(serve.baseUrl, sessionId);
          return frames.some(
            (f) =>
              f.event?.ToolCallCompleted?.tool_call_id === "fake-tool-call-1" &&
              f.event?.ToolCallCompleted?.is_error === true,
          );
        },
        { timeout: 15_000, intervals: [100, 200, 500, 1000] },
      )
      .toBe(true);
  } finally {
    await serve.stop();
    rmSync(scriptDir, { recursive: true, force: true });
  }
});
