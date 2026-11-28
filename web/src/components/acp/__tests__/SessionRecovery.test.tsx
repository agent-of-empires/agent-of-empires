// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import type { SessionRecovery } from "../../../lib/acpTypes";
import { SessionRecoveryBanner } from "../SessionBanners";

const recovery = (over: Partial<SessionRecovery> = {}): SessionRecovery => ({
  reason: "operation_failed",
  diagnostic: "The provider could not complete this operation.",
  evidence: {
    source: "local_prompt_result",
    operation: "prompt",
    runtime: "unknown",
    blocking_scope: "operation",
    retryable: true,
    reported_code: -32603,
    observed_at_ms: 10,
  },
  runtime: "acp_transport_ready",
  work_in_progress: false,
  uncertainty: null,
  guidance: "Repair the provider configuration in the Agent before composing a new operation.",
  actions: { attach: true, retry_operation: true, restart_same: false, stop: true, fresh: false },
  expected: 'opaque/selection:{"namespace":"original","generation":7}',
  ...over,
});
const fetchSpy = vi.fn<typeof fetch>();
const noActions = { attach: true, retry_operation: false, restart_same: false, stop: false, fresh: false };
const mount = (projection = recovery(), current = true, readOnly = false) => {
  const onAttach = vi.fn();
  const onRetryOperation = vi.fn();
  return {
    onAttach,
    onRetryOperation,
    ...render(
      <SessionRecoveryBanner
        sessionId="exact-original-session"
        recovery={projection}
        current={current}
        readOnly={readOnly}
        onAttach={onAttach}
        onRetryOperation={onRetryOperation}
      />,
    ),
  };
};

beforeEach(() => {
  fetchSpy.mockReset().mockResolvedValue(new Response("{}", { status: 200 }));
  vi.stubGlobal("fetch", fetchSpy);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("shared server recovery", () => {
  it("keeps operation scope separate from channel readiness and only observes or opens explicit composition", () => {
    const { onAttach, onRetryOperation } = mount();
    expect(screen.getByText("The provider could not complete this operation.")).toBeTruthy();
    expect(screen.getByTestId("session-recovery-scope").textContent).toContain("Blocking scope: operation");
    expect(screen.getByTestId("session-recovery-runtime").textContent).toContain(
      "does not prove the OS process is alive",
    );
    expect(screen.getByTestId("session-recovery").getAttribute("data-session-id")).toBe("exact-original-session");
    fireEvent.click(screen.getByTestId("session-recovery-attach"));
    fireEvent.click(screen.getByTestId("session-recovery-retry-operation"));
    expect(onAttach).toHaveBeenCalledOnce();
    expect(onRetryOperation).toHaveBeenCalledWith();
    expect(fetchSpy).not.toHaveBeenCalled();
    expect((screen.getByTestId("session-recovery-restart-same") as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByTestId("session-recovery-fresh") as HTMLButtonElement).disabled).toBe(true);
  });

  it("keeps a refused Same selection visible without substituting Fresh", async () => {
    const projection = recovery({ actions: { ...noActions, restart_same: true } });
    fetchSpy.mockResolvedValueOnce(
      new Response(JSON.stringify({ message: "The selected native binding changed." }), { status: 409 }),
    );
    mount(projection);
    fireEvent.click(screen.getByTestId("session-recovery-restart-same"));
    await screen.findByRole("alert");
    expect(fetchSpy).toHaveBeenCalledOnce();
    expect(screen.getByText(projection.diagnostic)).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toBe("The selected native binding changed.");
  });

  it("confirms Fresh consequences, permits cancellation, and never submits a prompt", async () => {
    const projection = recovery({ actions: { ...noActions, fresh: true } });
    mount(projection);
    fireEvent.click(screen.getByTestId("session-recovery-fresh"));
    expect(fetchSpy).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    fireEvent.click(screen.getByTestId("session-recovery-fresh"));
    fireEvent.click(screen.getByTestId("session-recovery-fresh-confirm"));
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledOnce());
  });

  it("invalidates open Fresh consent when current selection changes or disconnects", () => {
    const projection = recovery({ actions: { ...noActions, fresh: true } });
    const { rerender } = mount(projection);
    fireEvent.click(screen.getByTestId("session-recovery-fresh"));
    rerender(
      <SessionRecoveryBanner
        sessionId="exact-original-session"
        recovery={{ ...projection, expected: "new-selection" }}
        current
        readOnly={false}
      />,
    );
    expect(screen.queryByRole("dialog")).toBeNull();
    rerender(
      <SessionRecoveryBanner
        sessionId="exact-original-session"
        recovery={projection}
        current={false}
        readOnly={false}
      />,
    );
    expect(screen.getByText(projection.diagnostic)).toBeTruthy();
    expect((screen.getByTestId("session-recovery-fresh") as HTMLButtonElement).disabled).toBe(true);
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("uses existing Stop endpoint and blocks duplicate local requests", async () => {
    const { promise, resolve } = Promise.withResolvers<Response>();
    fetchSpy.mockReturnValueOnce(promise);
    mount();
    const stop = screen.getByTestId("session-recovery-stop");
    fireEvent.click(stop);
    fireEvent.click(stop);
    expect(fetchSpy).toHaveBeenCalledOnce();
    expect(fetchSpy.mock.calls[0]![0]).toBe("/api/sessions/exact-original-session/stop");
    expect((screen.getByTestId("session-recovery-retry-operation") as HTMLButtonElement).disabled).toBe(true);
    await act(async () => resolve(new Response("{}", { status: 200 })));
  });

  it.each([
    [false, false],
    [true, true],
  ] as const)("keeps stale or readonly diagnostics without granting mutation (%s, %s)", (current, readOnly) => {
    mount(
      recovery({ actions: { attach: true, retry_operation: true, restart_same: true, stop: true, fresh: true } }),
      current,
      readOnly,
    );
    for (const action of ["retry-operation", "restart-same", "stop", "fresh"]) {
      expect((screen.getByTestId(`session-recovery-${action}`) as HTMLButtonElement).disabled).toBe(true);
      fireEvent.click(screen.getByTestId(`session-recovery-${action}`));
    }
    expect(screen.getByText("The provider could not complete this operation.")).toBeTruthy();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("blocks local in-flight mutation requests without inferring runtime policy", () => {
    render(<SessionRecoveryBanner sessionId="original" recovery={recovery()} current readOnly={false} requestBusy />);
    expect((screen.getByTestId("session-recovery-stop") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByTestId("session-recovery-stop"));
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});
