// @vitest-environment jsdom

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { switchAcpProvider, reportError } = vi.hoisted(() => ({
  switchAcpProvider: vi.fn(),
  reportError: vi.fn(),
}));
vi.mock("../../../lib/api", () => ({ switchAcpProvider }));
vi.mock("../../../lib/toastBus", () => ({ reportError }));

import { useProviderSwitch } from "../useProviderSwitch";
import type { SwitchProviderResponse } from "../../../lib/api";

function setup(server: string | null) {
  return renderHook(({ server }) => useProviderSwitch("s-1", "claude", server), {
    initialProps: { server },
  });
}

beforeEach(() => {
  switchAcpProvider.mockReset();
  reportError.mockReset();
});

describe("useProviderSwitch", () => {
  // API, Bedrock from this tab, the poll confirms it, then API from the CLI:
  // the row is back at the value the echo replaced, and must still win.
  it("does not revive an acknowledged pick when the row returns to its old value", async () => {
    switchAcpProvider.mockResolvedValue({
      session_id: "s-1",
      provider: "bedrock",
      model_cleared: false,
      status: "running",
    });
    const { result, rerender } = setup("api");

    await act(() => result.current.set!("bedrock"));
    expect(result.current.current).toBe("bedrock");

    rerender({ server: "bedrock" });
    expect(result.current.current).toBe("bedrock");

    rerender({ server: "api" });
    expect(result.current.current).toBe("api");
  });

  it("reports a refused switch and keeps the server's value", async () => {
    switchAcpProvider.mockRejectedValue(new Error("the session is mid-turn"));
    const { result } = setup("api");

    await act(() => result.current.set!("vertex"));

    expect(reportError).toHaveBeenCalledWith("Provider switch failed: the session is mid-turn");
    expect(result.current).toMatchObject({ current: "api", pending: null });
  });
});
const response = (session_id: string, provider: string): SwitchProviderResponse => ({
  session_id,
  provider,
  model_cleared: true,
  status: "running",
});
const deferred = () => {
  let resolve!: (result: SwitchProviderResponse) => void;
  const promise = new Promise<SwitchProviderResponse>((done) => {
    resolve = done;
  });
  return { promise, resolve };
};

describe("provider request ownership", () => {
  it("keeps one pick pending until its reply even when the row already confirms it", async () => {
    const first = deferred();
    switchAcpProvider.mockImplementation((_id: string, provider: string) =>
      provider === "bedrock" ? first.promise : Promise.resolve(response("s1", provider)),
    );
    const { result, rerender } = renderHook(({ reported }) => useProviderSwitch("s1", "claude", reported), {
      initialProps: { reported: "api" },
    });
    let request!: Promise<void>;
    act(() => {
      request = result.current.set!("bedrock");
    });
    try {
      expect(result.current.current).toBe("api");
      expect(result.current.pending).toBe("bedrock");
      await act(async () => {
        await result.current.set!("vertex");
      });
      expect(result.current.current).toBe("api");
      expect(result.current.pending).toBe("bedrock");
      rerender({ reported: "bedrock" });
      expect(result.current.pending).toBe("bedrock");
    } finally {
      await act(async () => {
        first.resolve(response("s1", "bedrock"));
        await request;
      });
    }
    expect(result.current.current).toBe("bedrock");
    expect(result.current.pending).toBeNull();
  });

  it("does not revive a pending pick after canonical changes away and back before its reply", async () => {
    const first = deferred();
    switchAcpProvider.mockReturnValue(first.promise);
    const { result, rerender } = setup("api");
    let request!: Promise<void>;
    act(() => {
      request = result.current.set!("bedrock");
    });
    rerender({ server: "bedrock" });
    rerender({ server: "api" });
    expect(result.current.pending).toBe("bedrock");
    await act(async () => {
      first.resolve(response("s-1", "bedrock"));
      await request;
    });
    expect(result.current).toMatchObject({ current: "api", pending: null });
  });

  it("does not let another session's late reply replace the active session's confirmed pick", async () => {
    const old = deferred();
    switchAcpProvider.mockImplementation((id: string, provider: string) =>
      id === "s1" ? old.promise : Promise.resolve(response(id, provider)),
    );
    const { result, rerender } = renderHook(({ id }) => useProviderSwitch(id, "claude", "api"), {
      initialProps: { id: "s1" },
    });
    let request!: Promise<void>;
    act(() => {
      request = result.current.set!("bedrock");
    });
    rerender({ id: "s2" });
    await act(async () => {
      await result.current.set!("vertex");
    });
    try {
      expect(result.current.current).toBe("vertex");
    } finally {
      await act(async () => {
        old.resolve(response("s1", "bedrock"));
        await request;
      });
    }
    expect(result.current.current).toBe("vertex");
    expect(result.current.pending).toBeNull();
  });
  it("retains ownership across the keyed A to B to A view lifecycle", async () => {
    const first = deferred();
    switchAcpProvider.mockReturnValue(first.promise);
    const original = renderHook(() => useProviderSwitch("remount-a", "claude", "api"));
    let request!: Promise<void>;
    act(() => {
      request = original.result.current.set!("bedrock");
    });
    original.unmount();
    const other = renderHook(() => useProviderSwitch("remount-b", "claude", "api"));
    expect(other.result.current.pending).toBeNull();
    other.unmount();
    const returned = renderHook(() => useProviderSwitch("remount-a", "claude", "api"));
    try {
      expect(returned.result.current.pending).toBe("bedrock");
      await act(() => returned.result.current.set!("vertex"));
      expect(switchAcpProvider).toHaveBeenCalledTimes(1);
    } finally {
      await act(async () => {
        first.resolve(response("remount-a", "bedrock"));
        await request;
      });
    }
    expect(returned.result.current).toMatchObject({ current: "bedrock", pending: null });
  });
});
