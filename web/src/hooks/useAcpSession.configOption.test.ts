// @vitest-environment jsdom
//
// Hook tests for the structured view config-option (model picker + reasoning
// effort) feature (#1403). Covers the new internal reducer actions
// that drive pending state and the dismissable failure notice, plus
// the async setConfigOption hook callback wired up to a stubbed
// fetch. End-to-end UI flows are exercised by the mocked Playwright
// specs under web/tests/.

import { act, renderHook } from "@testing-library/react";
import { createElement, type ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { emptyAcpState } from "../lib/acpTypes";
import { AgentProfileProvider } from "../lib/agentProfileContext";
import { acpHookReducer, useAcpSession } from "./useAcpSession";

describe("acpHookReducer / config option actions", () => {
  it("set_pending_config_option records the requested click", () => {
    const next = acpHookReducer(emptyAcpState(), {
      kind: "set_pending_config_option",
      configId: "model",
      value: "claude-sonnet-4-6",
    });
    expect(next.pendingConfigOption).toEqual({
      configId: "model",
      value: "claude-sonnet-4-6",
    });
  });

  it("clear_pending_config_option drops the in-flight record", () => {
    const seeded = acpHookReducer(emptyAcpState(), {
      kind: "set_pending_config_option",
      configId: "effort",
      value: "high",
    });
    const next = acpHookReducer(seeded, {
      kind: "clear_pending_config_option",
    });
    expect(next.pendingConfigOption).toBeNull();
  });

  it("dismiss_config_option_switch_failed clears the notice", () => {
    const seeded = {
      ...emptyAcpState(),
      configOptionSwitchFailed: {
        configId: "model",
        value: "claude-sonnet-4-6",
        reason: "rate limited",
        at: new Date().toISOString(),
      },
    };
    const next = acpHookReducer(seeded, {
      kind: "dismiss_config_option_switch_failed",
    });
    expect(next.configOptionSwitchFailed).toBeNull();
  });

  it("set_pending overrides any previous pending click on the same option", () => {
    let state = acpHookReducer(emptyAcpState(), {
      kind: "set_pending_config_option",
      configId: "model",
      value: "claude-opus-4-7",
    });
    state = acpHookReducer(state, {
      kind: "set_pending_config_option",
      configId: "model",
      value: "claude-sonnet-4-6",
    });
    expect(state.pendingConfigOption?.value).toBe("claude-sonnet-4-6");
  });

  it("clear_pending_config_option_if_match clears when the request matches the current pending", () => {
    const seeded = acpHookReducer(emptyAcpState(), {
      kind: "set_pending_config_option",
      configId: "model",
      value: "claude-sonnet-4-6",
    });
    const next = acpHookReducer(seeded, {
      kind: "clear_pending_config_option_if_match",
      configId: "model",
      value: "claude-sonnet-4-6",
    });
    expect(next.pendingConfigOption).toBeNull();
  });

  it("clear_pending_config_option_if_match leaves a newer pending intact when a stale request fails", () => {
    // Request A for model=opus dispatched, then request B for
    // model=sonnet replaced pending. A's failure must NOT wipe B.
    let state = acpHookReducer(emptyAcpState(), {
      kind: "set_pending_config_option",
      configId: "model",
      value: "claude-opus-4-7",
    });
    state = acpHookReducer(state, {
      kind: "set_pending_config_option",
      configId: "model",
      value: "claude-sonnet-4-6",
    });
    const next = acpHookReducer(state, {
      kind: "clear_pending_config_option_if_match",
      configId: "model",
      value: "claude-opus-4-7",
    });
    expect(next.pendingConfigOption).toEqual({
      configId: "model",
      value: "claude-sonnet-4-6",
    });
  });

  it("clear_pending_config_option_if_match is a no-op when pending is already cleared", () => {
    const next = acpHookReducer(emptyAcpState(), {
      kind: "clear_pending_config_option_if_match",
      configId: "model",
      value: "claude-opus-4-7",
    });
    expect(next.pendingConfigOption).toBeNull();
  });
});

// ---- Hook async tests ---------------------------------------------------

interface FakeSocket {
  url: string;
  readyState: number;
  onopen: ((ev: Event) => void) | null;
  onclose: ((ev: CloseEvent) => void) | null;
  onerror: ((ev: Event) => void) | null;
  onmessage: ((ev: MessageEvent) => void) | null;
  close: () => void;
  send: (data: string | ArrayBufferLike | Blob | ArrayBufferView) => void;
}

const sockets: FakeSocket[] = [];
let originalWebSocket: typeof WebSocket;

class FakeWebSocket implements FakeSocket {
  url: string;
  readyState: number = 0;
  onopen: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  constructor(url: string) {
    this.url = url;
    sockets.push(this);
  }
  close(): void {
    this.readyState = FakeWebSocket.CLOSED;
    if (this.onclose) {
      this.onclose({
        code: 1000,
        reason: "test close",
        wasClean: true,
      } as CloseEvent);
    }
  }
  send(): void {
    /* no-op */
  }
}

async function flushAsync(): Promise<void> {
  await act(async () => {
    for (let i = 0; i < 8; i++) {
      await Promise.resolve();
    }
  });
}

const wrapper = ({ children }: { children: ReactNode }): ReactNode =>
  createElement(AgentProfileProvider, { toolKey: "claude" }, children);

describe("useAcpSession / setConfigOption", () => {
  let postBodies: Array<{ url: string; body: unknown }>;
  let postShouldFail: number | "throw" | null;

  beforeEach(() => {
    sockets.length = 0;
    postBodies = [];
    postShouldFail = null;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = typeof input === "string" ? input : input.toString();
        if (url.includes("/acp/replay")) {
          return new Response(
            JSON.stringify({ frames: [], lost: false, highest_seq: 0 }),
            { status: 200 },
          );
        }
        if (url.includes("/acp/config-option")) {
          if (postShouldFail === "throw") {
            throw new TypeError("network down");
          }
          postBodies.push({
            url,
            body: typeof init?.body === "string"
              ? JSON.parse(init.body)
              : init?.body,
          });
          if (typeof postShouldFail === "number") {
            return new Response("simulated failure", {
              status: postShouldFail,
            });
          }
          return new Response("{}", { status: 202 });
        }
        return new Response("{}", { status: 200 });
      }),
    );
    originalWebSocket = global.WebSocket;
    global.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
  });

  afterEach(() => {
    global.WebSocket = originalWebSocket;
    vi.unstubAllGlobals();
  });

  it("setConfigOption posts to the structured view config-option endpoint with the right body shape", async () => {
    const { result } = renderHook(() => useAcpSession("sess-cfg-1"), { wrapper });
    await flushAsync();
    await act(async () => {
      await result.current.setConfigOption("model", "claude-sonnet-4-6");
    });
    expect(postBodies).toHaveLength(1);
    expect(postBodies[0]!.url).toContain(
      "/api/sessions/sess-cfg-1/acp/config-option",
    );
    expect(postBodies[0]!.body).toEqual({
      config_id: "model",
      value: "claude-sonnet-4-6",
    });
  });

  it("setConfigOption posts the effort body shape", async () => {
    const { result } = renderHook(() => useAcpSession("sess-cfg-2"), { wrapper });
    await flushAsync();
    await act(async () => {
      await result.current.setConfigOption("effort", "high");
    });
    expect(postBodies).toHaveLength(1);
    expect(postBodies[0]!.body).toEqual({
      config_id: "effort",
      value: "high",
    });
  });

  it("setConfigOption clears pending and records lastError on non-OK response", async () => {
    postShouldFail = 500;
    const { result } = renderHook(() => useAcpSession("sess-cfg-3"), { wrapper });
    await flushAsync();
    await act(async () => {
      await result.current.setConfigOption("model", "claude-sonnet-4-6");
    });
    expect(result.current.state.pendingConfigOption).toBeNull();
    expect(result.current.state.lastError).toMatch(/Could not set model/);
  });

  it("setConfigOption clears pending and records lastError on network failure (throw)", async () => {
    postShouldFail = "throw";
    const { result } = renderHook(() => useAcpSession("sess-cfg-4"), { wrapper });
    await flushAsync();
    await act(async () => {
      await result.current.setConfigOption("effort", "low");
    });
    expect(result.current.state.pendingConfigOption).toBeNull();
    expect(result.current.state.lastError).toMatch(/Network error setting effort/);
  });

  it("dismissConfigOptionSwitchFailed clears a populated notice", async () => {
    const { result } = renderHook(() => useAcpSession("sess-cfg-5"), { wrapper });
    await flushAsync();

    // Seed the notice through the WS broadcast path: drive a
    // ConfigOptionSwitchFailed frame into the hook's reducer so the
    // dismiss callback has something real to clear. The hook's
    // onmessage handler expects the raw AcpFrame shape (session_id
    // + seq + event); no kind/frame wrapper.
    const ws = sockets[0]!;
    await act(async () => {
      ws.readyState = FakeWebSocket.OPEN;
      ws.onopen?.(new Event("open"));
      ws.onmessage?.(
        new MessageEvent("message", {
          data: JSON.stringify({
            session_id: "sess-cfg-5",
            seq: 1,
            event: {
              ConfigOptionSwitchFailed: {
                config_id: "model",
                value: "claude-sonnet-4-6",
                reason: "rate limited",
              },
            },
          }),
        }),
      );
    });
    expect(result.current.state.configOptionSwitchFailed).not.toBeNull();
    expect(result.current.state.configOptionSwitchFailed?.reason).toBe(
      "rate limited",
    );

    await act(async () => {
      result.current.dismissConfigOptionSwitchFailed();
    });
    expect(result.current.state.configOptionSwitchFailed).toBeNull();
  });

  it("setConfigOption is a no-op when sessionId is empty", async () => {
    const { result } = renderHook(() => useAcpSession(""), { wrapper });
    await flushAsync();
    await act(async () => {
      await result.current.setConfigOption("model", "claude-opus-4-7");
    });
    expect(postBodies).toHaveLength(0);
    // No pending state set either, because the early return fires
    // before the dispatch.
    expect(result.current.state.pendingConfigOption).toBeNull();
  });
});

// ---- normaliseTurnCounters backfill -------------------------------------

describe("normaliseTurnCounters / config-option backfill", () => {
  it("backfills empty configOptions when the persisted entry pre-dates #1403", async () => {
    const { normaliseTurnCounters } = await import("../lib/acpTypes");
    const stale = {
      ...emptyAcpState(),
    } as Record<string, unknown>;
    delete stale.configOptions;
    delete stale.configOptionSwitchFailed;
    delete stale.pendingConfigOption;
    const next = normaliseTurnCounters(
      stale as unknown as Parameters<typeof normaliseTurnCounters>[0],
    );
    expect(next.configOptions).toEqual([]);
    expect(next.configOptionSwitchFailed).toBeNull();
    expect(next.pendingConfigOption).toBeNull();
  });

  it("preserves a populated configOptions list across hydration", async () => {
    const { normaliseTurnCounters } = await import("../lib/acpTypes");
    const seeded = {
      ...emptyAcpState(),
      configOptions: [
        {
          id: "model",
          name: "Model",
          category: "model" as const,
          current_value: "claude-opus-4-7",
          options: [{ value: "claude-opus-4-7", name: "Claude Opus 4.7" }],
        },
      ],
    };
    const next = normaliseTurnCounters(seeded);
    expect(next.configOptions).toHaveLength(1);
    expect(next.configOptions[0]!.current_value).toBe("claude-opus-4-7");
  });
});
