// @vitest-environment jsdom
//
// The client half of #3417: `turnActive` is the daemon's `turn_active` OR'd
// with prompts this client has POSTed but not yet seen acknowledged. Every
// POST outcome has to settle its own id, or that id latches the spinner for
// the life of the session. Before this landed, a non-503 5xx and a network
// exception settled nothing at all.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { applyEvent, emptyAcpState, type AcpState } from "../lib/acpTypes";
import { acpHookReducer, clearAcpCache, useAcpSession } from "./useAcpSession";
import { act, renderHook } from "@testing-library/react";

describe("acpHookReducer / in-flight prompt settlement", () => {
  function sent(id: string): AcpState {
    return acpHookReducer(emptyAcpState(), { kind: "user_prompt", id, text: "hi" });
  }

  it("user_prompt records the id and shows the spinner immediately", () => {
    const state = sent("p1");
    expect(state.inflightPromptIds).toEqual(["p1"]);
    expect(state.turnActive).toBe(true);
    expect(state.serverTurnActive).toBe(false);
    expect(state.promptSeq).toBe(1);
  });

  it("every failure action settles its own id and unlocks the composer", () => {
    // One row per POST outcome that no `UserPromptSent` will follow. The
    // rejection keeps its overlay row so the user still sees what they tried
    // to send; the rollback drops it because the prompt moved to the queue.
    const cases: Array<
      [string, "prompt_send_rejected" | "settle_inflight_prompt" | "rollback_optimistic_prompt", boolean]
    > = [
      ["4xx rejection", "prompt_send_rejected", true],
      ["5xx or network exception", "settle_inflight_prompt", true],
      ["worker_not_ready 503 or queued disposition", "rollback_optimistic_prompt", false],
    ];
    for (const [label, kind, keepsRow] of cases) {
      const next = acpHookReducer(sent("p1"), { kind, id: "p1" });
      expect(next.inflightPromptIds, label).toEqual([]);
      expect(next.turnActive, label).toBe(false);
      expect(next.optimisticRows.length > 0, label).toBe(keepsRow);
    }
  });

  it("settling one prompt does not retire another still in flight", () => {
    let state = sent("p1");
    state = acpHookReducer(state, { kind: "user_prompt", id: "p2", text: "second" });
    state = acpHookReducer(state, { kind: "settle_inflight_prompt", id: "p1" });
    expect(state.inflightPromptIds).toEqual(["p2"]);
    expect(state.turnActive).toBe(true);
  });

  it("a settled prompt cannot close a turn the daemon reports as running", () => {
    let state = sent("p1");
    state = applyEvent(state, {
      session_id: "s-1",
      seq: 1,
      event: { UserPromptSent: { text: "hi", prompt_id: "p1" } },
    });
    expect(state.serverTurnActive).toBe(true);
    // A late ambiguous-failure settlement for the same id must not win over
    // the daemon's acknowledgement.
    state = acpHookReducer(state, { kind: "settle_inflight_prompt", id: "p1" });
    expect(state.turnActive).toBe(true);
  });
});

describe("useAcpSession / dispatchPromptNow settles on every POST outcome", () => {
  beforeEach(() => {
    clearAcpCache();
    window.localStorage.clear();
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  async function sendAndReadTurnActive(respond: () => Promise<Response> | never): Promise<boolean> {
    vi.spyOn(globalThis, "fetch").mockImplementation((async (input: RequestInfo | URL) => {
      const url = String(typeof input === "string" ? input : input instanceof URL ? input.href : input.url);
      if (url.includes("/acp/prompt")) return respond();
      return new Response("[]", { status: 200, headers: { "Content-Type": "application/json" } });
    }) as typeof fetch);

    const { result } = renderHook(() => useAcpSession("s-1"));
    await act(async () => {
      await result.current.sendPrompt("hello");
    });
    return result.current.state.turnActive;
  }

  it("a 500 does not leave the spinner latched", async () => {
    expect(await sendAndReadTurnActive(async () => new Response("boom", { status: 500 }))).toBe(false);
  });

  it("a network exception does not leave the spinner latched", async () => {
    expect(
      await sendAndReadTurnActive(() => {
        throw new TypeError("Failed to fetch");
      }),
    ).toBe(false);
  });
});
