// @vitest-environment jsdom

import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useRespawnSession } from "./useRespawnSession";

function stubFetch(ok: boolean, status: number, text = "") {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue({ ok, status, text: () => Promise.resolve(text) }));
}

function renderRespawn(resetKey: string | null = "unknown") {
  return renderHook(({ resetKey }: { resetKey: string | null }) => useRespawnSession("s1", resetKey), {
    initialProps: { resetKey },
  });
}

beforeEach(() => {
  stubFetch(true, 200);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("useRespawnSession request", () => {
  it.each([
    ["a plain respawn to /acp/spawn", undefined, "/api/sessions/s1/acp/spawn"],
    ["a restart to /acp/restart", { restart: true }, "/api/sessions/s1/acp/restart"],
  ])("posts %s", async (_label, options, path) => {
    const { result } = renderHook(() => useRespawnSession("s1", null, options));
    await act(async () => {
      await result.current.respawn();
    });
    const [url, init] = vi.mocked(fetch).mock.calls[0]!;
    expect(url).toBe(path);
    expect(JSON.parse(String(init?.body))).toEqual({});
  });

  it.each([
    ["a restart", true, "ok"],
    ["a spawn", false, "failed"],
  ])("settles %s refused by another Device's restart in progress as %s", async (_label, restart, state) => {
    stubFetch(false, 409, "restart_in_progress: a restart is already in progress for this session");
    const { result } = renderHook(() => useRespawnSession("s1", null, { restart }));
    await act(async () => {
      await result.current.respawn();
    });
    expect(result.current.state).toBe(state);
  });
});

describe("useRespawnSession failure text", () => {
  it.each([
    ["plain text", "spawn failed: boom", "spawn failed: boom"],
    [
      "a JSON refusal",
      JSON.stringify({ error: "session_archived", message: "session is archived; unarchive it first" }),
      "session is archived; unarchive it first",
    ],
    ["JSON without a message", JSON.stringify({ error: "not_found" }), JSON.stringify({ error: "not_found" })],
    ["an empty body", "", ""],
  ])("keeps %s readable", async (_label, body, detail) => {
    stubFetch(false, 409, body);
    const { result } = renderRespawn();
    await act(async () => {
      await result.current.respawn();
    });
    expect(result.current.detail).toBe(detail);
    expect(result.current.error).toBe(`Server returned 409. ${detail}`.trim());
  });
});

describe("useRespawnSession resetKey", () => {
  it.each([
    ["a successful respawn", () => stubFetch(true, 200), "ok", "", ["reset-2"]],
    ["a failed respawn", () => stubFetch(false, 500, "boom"), "failed", "boom", [null, "reset-2"]],
  ] as [string, () => void, string, string, (string | null)[]][])(
    "starts fresh for the next incident after %s",
    async (_label, stub, settled, errorText, keysAfter) => {
      stub();
      const { result, rerender } = renderRespawn();

      await act(async () => {
        await result.current.respawn();
      });
      expect(result.current.state).toBe(settled);
      expect(result.current.error ?? "").toContain(errorText);

      for (const resetKey of keysAfter) {
        rerender({ resetKey });
        expect(result.current.state).toBe("idle");
        expect(result.current.error).toBeNull();
      }
    },
  );

  it.each([
    ["a null in between", [null, "unknown"] as (string | null)[]],
    ["another reset in between", ["2099-01-01T09:30:00Z", "unknown"] as (string | null)[]],
  ])("ignores a request that completed after its incident ended, %s", async (_label, keysAfter) => {
    let release: (() => void) | undefined;
    vi.stubGlobal(
      "fetch",
      vi.fn().mockImplementation(
        () =>
          new Promise((resolve) => {
            release = () => resolve({ ok: true, status: 200, text: () => Promise.resolve("") });
          }),
      ),
    );
    const { result, rerender } = renderRespawn();

    let pending: Promise<boolean> | undefined;
    await act(async () => {
      pending = result.current.respawn();
    });
    expect(result.current.state).toBe("retrying");

    for (const resetKey of keysAfter) rerender({ resetKey });
    expect(result.current.state).toBe("idle");

    await act(async () => {
      release?.();
      await pending;
    });

    expect(result.current.state).toBe("idle");
    expect(result.current.error).toBeNull();
  });
});
