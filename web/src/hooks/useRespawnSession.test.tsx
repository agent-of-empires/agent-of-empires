// @vitest-environment jsdom

import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useRespawnSession } from "./useRespawnSession";

beforeEach(() => {
  vi.stubGlobal(
    "fetch",
    vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      text: () => Promise.resolve(""),
    }),
  );
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("useRespawnSession resetKey", () => {
  it("starts fresh for a second incident after a successful respawn", async () => {
    const { result, rerender } = renderHook(({ resetKey }: { resetKey: string }) => useRespawnSession("s1", resetKey), {
      initialProps: { resetKey: "reset-1" },
    });

    await act(async () => {
      await result.current.respawn();
    });
    expect(result.current.state).toBe("ok");

    rerender({ resetKey: "reset-2" });

    expect(result.current.state).toBe("idle");
    expect(result.current.error).toBeNull();
  });

  it("starts fresh for a second incident after a failed respawn", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue({
        ok: false,
        status: 500,
        text: () => Promise.resolve("boom"),
      }),
    );
    const { result, rerender } = renderHook(
      ({ resetKey }: { resetKey: string | null }) => useRespawnSession("s1", resetKey),
      {
        initialProps: { resetKey: "reset-1" },
      },
    );

    await act(async () => {
      await result.current.respawn();
    });
    expect(result.current.state).toBe("failed");
    expect(result.current.error).toContain("boom");

    rerender({ resetKey: null });
    expect(result.current.state).toBe("idle");
    expect(result.current.error).toBeNull();

    rerender({ resetKey: "reset-2" });
    expect(result.current.state).toBe("idle");
    expect(result.current.error).toBeNull();
  });
});
