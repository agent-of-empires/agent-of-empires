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

describe("useRespawnSession resetKey", () => {
  it.each([
    ["a successful respawn", () => stubFetch(true, 200), "ok", "", ["reset-2"]],
    ["a failed respawn", () => stubFetch(false, 500, "boom"), "failed", "boom", [null, "reset-2"]],
    ["a second incident on the same key", () => stubFetch(false, 409, "busy"), "failed", "busy", [null, "unknown"]],
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
