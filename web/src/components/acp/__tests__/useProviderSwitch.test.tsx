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
