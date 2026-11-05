// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
import type * as ApiModule from "../../lib/api";
import type { SessionRecovery } from "../../lib/acpTypes";
import { makeSession } from "./fixtures";

const live = vi.hoisted(() => ({
  state: {
    connected: false,
    reconnecting: false,
    retryCount: 7,
    retryCountdown: 0,
    frame: null,
    reading: false,
    isOwner: true,
    ownerKnown: true,
    transport: null,
    paneDead: true,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  ensureSession: vi.fn(async () => ({ ok: true, message: null })),
  ensureTerminal: vi.fn(async () => ({ ok: true, message: null })),
  manualReconnect: vi.fn(),
  sendData: vi.fn(),
  sendPaste: vi.fn(() => true),
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: () => ({ ...live, maxRetries: 7, claim: vi.fn() }),
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof ApiModule>()),
  ensureSession: live.ensureSession,
  ensureTerminal: live.ensureTerminal,
  pasteImage: vi.fn(),
  isStartRefusal: () => false,
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: ({ inputRef }: { inputRef: RefObject<HTMLTextAreaElement | null> }) => (
    <textarea aria-label="Live terminal input" ref={inputRef} />
  ),
}));

const recovery = (over: Partial<SessionRecovery> = {}): SessionRecovery => ({
  reason: "operation_failed",
  diagnostic: "The operation failed; the original native conversation remains selected.",
  evidence: {
    source: "local_prompt_result",
    operation: "prompt",
    runtime: "unknown",
    blocking_scope: "operation",
    observed_at_ms: 1,
  },
  runtime: "acp_transport_ready",
  work_in_progress: false,
  uncertainty: null,
  guidance: "Observe the retained conversation before writing another operation.",
  actions: { attach: true, retry_operation: true, restart_same: false, stop: true, fresh: false },
  expected: "opaque-exact-binding",
  ...over,
});
const fetchSpy = vi.fn<typeof fetch>();
beforeEach(() => {
  live.state.paneDead = true;
  live.state.connected = false;
  live.ensureSession.mockClear();
  live.ensureTerminal.mockClear();
  live.manualReconnect.mockClear();
  live.sendData.mockClear();
  live.sendPaste.mockClear();
  fetchSpy.mockReset().mockResolvedValue(new Response("{}", { status: 200 }));
  vi.stubGlobal("fetch", fetchSpy);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("terminal shared recovery", () => {
  it("mounts and observes a failed operation without ensuring, replacing or resending", () => {
    const projection = recovery();
    render(
      <LiveTerminalView
        readOnly={false}
        recoveryCurrent
        session={makeSession({ id: "original", status: "Error", recovery: projection })}
      />,
    );
    expect(screen.getByText(projection.diagnostic)).toBeTruthy();
    fireEvent.click(screen.getByTestId("session-recovery-attach"));
    expect(live.manualReconnect).toHaveBeenCalledOnce();
    expect(live.ensureSession).not.toHaveBeenCalled();
    expect(live.sendData).not.toHaveBeenCalled();
    expect(live.sendPaste).not.toHaveBeenCalled();
    expect(fetchSpy).not.toHaveBeenCalled();
    expect(screen.queryByRole("button", { name: "Restart" })).toBeNull();
  });

  it("keeps server-authorized Same usable after confirmed exit even though terminal WS is closed", async () => {
    const projection = recovery({
      reason: "runtime_exited",
      runtime: { pane_exited: { exit_code: 9 } },
      actions: { attach: true, retry_operation: false, restart_same: true, stop: false, fresh: true },
    });
    render(
      <LiveTerminalView
        readOnly={false}
        recoveryCurrent
        session={makeSession({ id: "original", status: "Error", recovery: projection })}
      />,
    );
    fireEvent.click(screen.getByTestId("session-recovery-restart-same"));
    await waitFor(() => expect(live.manualReconnect).toHaveBeenCalledOnce());
    expect(fetchSpy).toHaveBeenCalledOnce();
    expect(live.ensureSession).not.toHaveBeenCalled();
    expect(live.sendPaste).not.toHaveBeenCalled();
    expect(screen.getByText(projection.diagnostic)).toBeTruthy();
  });

  it("keeps a missing or stale projection diagnostic visible without retaining permissions", () => {
    const session = makeSession({ id: "original", recovery: recovery() });
    const { rerender } = render(<LiveTerminalView readOnly={false} recoveryCurrent session={session} />);
    rerender(
      <LiveTerminalView readOnly={false} recoveryCurrent={false} session={{ ...session, recovery: undefined }} />,
    );
    expect(screen.getByText(session.recovery!.diagnostic)).toBeTruthy();
    rerender(<LiveTerminalView readOnly={false} recoveryCurrent={false} session={{ ...session, recovery: null }} />);
    expect(screen.getByText(session.recovery!.diagnostic)).toBeTruthy();
    for (const action of ["attach", "retry-operation", "restart-same", "stop", "fresh"]) {
      expect((screen.getByTestId(`session-recovery-${action}`) as HTMLButtonElement).disabled).toBe(true);
      fireEvent.click(screen.getByTestId(`session-recovery-${action}`));
    }
    expect(live.ensureSession).not.toHaveBeenCalled();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("opens an empty recovery composition instead of restoring the saved prompt, then sends only new explicit input", async () => {
    live.state.connected = true;
    live.state.paneDead = false;
    const session = makeSession({ id: "draft-recovery" });
    const { rerender } = render(<LiveTerminalView readOnly={false} session={session} />);
    fireEvent.click(await screen.findByRole("button", { name: "Compose" }));
    fireEvent.input(screen.getByLabelText("Message"), { target: { value: "possibly accepted original prompt" } });
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    const failed = { ...session, recovery: recovery() };
    rerender(<LiveTerminalView readOnly={false} recoveryCurrent session={failed} />);
    fireEvent.click(screen.getByTestId("session-recovery-retry-operation"));
    expect(screen.getByLabelText<HTMLTextAreaElement>("Message").value).toBe("");
    expect(live.sendPaste).not.toHaveBeenCalled();
    fireEvent.input(screen.getByLabelText("Message"), { target: { value: "new explicit operation" } });
    rerender(<LiveTerminalView readOnly={false} recoveryCurrent={false} session={failed} />);
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(live.sendPaste).not.toHaveBeenCalled();
    rerender(
      <LiveTerminalView
        readOnly={false}
        recoveryCurrent
        session={{ ...failed, recovery: recovery({ expected: "different-native-binding" }) }}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(live.sendPaste).not.toHaveBeenCalled();
    expect(screen.getByLabelText<HTMLTextAreaElement>("Message").value).toBe("new explicit operation");
    rerender(<LiveTerminalView readOnly={false} recoveryCurrent session={failed} />);
    fireEvent.click(screen.getByRole("button", { name: "Send" }));
    expect(live.sendPaste).toHaveBeenCalledExactlyOnceWith("new explicit operation", true);
    expect(live.ensureSession).toHaveBeenCalledOnce();
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("does not apply agent recovery to paired or tool terminals", async () => {
    render(
      <LiveTerminalView
        readOnly={false}
        surface="paired-host"
        terminalIndex={2}
        recoveryCurrent
        session={makeSession({ id: "paired", recovery: recovery() })}
      />,
    );
    await waitFor(() => expect(live.ensureTerminal).toHaveBeenCalledExactlyOnceWith("paired", 2, false));
    expect(screen.queryByTestId("session-recovery")).toBeNull();
    expect(live.ensureSession).not.toHaveBeenCalled();
    fireEvent.click(await screen.findByRole("button", { name: "Restart" }));
    await waitFor(() => expect(live.ensureTerminal).toHaveBeenCalledTimes(2));
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});
