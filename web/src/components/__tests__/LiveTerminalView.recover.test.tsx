// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
import type * as ApiModule from "../../lib/api";
import type { SessionRecovery } from "../../lib/acpTypes";
import { LiveSlowNotice } from "../LiveSlowNotice";
import { makeSession } from "./fixtures";

const live = vi.hoisted(() => ({
  state: {
    connected: false,
    reconnecting: false,
    retryCount: 7,
    frame: null,
    reading: false,
    isOwner: true,
    ownerKnown: true,
    ownerKind: null,
    transport: null,
    transportReason: null as string | null,
    paneDead: true,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  ensureSession: vi.fn(async () => ({ ok: true, message: null })),
  ensureTerminal: vi.fn(async () => ({ ok: true, message: null })),
  manualReconnect: vi.fn(),
  sendData: vi.fn(),
  sendPaste: vi.fn(() => true),
  reconnectAndClaim: vi.fn(),
  onGridTransport: undefined as (() => void) | undefined,
  observation: null as null | { id: unknown; readOnly: unknown; path: unknown },
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: (_id: unknown, _readOnly: unknown, _path: unknown, _clip: unknown, onGrid: () => void) => {
    live.observation = { id: _id, readOnly: _readOnly, path: _path };
    live.onGridTransport = onGrid;
    return { ...live, maxRetries: 7, claim: vi.fn(), reconnectAndClaim: live.reconnectAndClaim };
  },
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
  observe_expected: "opaque-exact-pane",
  ...over,
});
const fetchSpy = vi.fn<typeof fetch>();
beforeEach(() => {
  live.state.paneDead = true;
  live.state.connected = false;
  live.state.transportReason = null;
  live.state.isOwner = false;
  live.state.ownerKnown = false;
  live.ensureSession.mockClear();
  live.reconnectAndClaim.mockClear();
  window.localStorage.clear();
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
  it("observes the exact failed pane read-only and revokes stale selection without ensuring or resending", () => {
    const projection = recovery();
    const session = makeSession({ id: "original", status: "Error", recovery: projection });
    const { rerender } = render(<LiveTerminalView readOnly={false} recoveryCurrent session={session} />);
    expect(screen.getByText(projection.diagnostic)).toBeTruthy();
    expect(live.observation?.id).toBeNull();
    fireEvent.click(screen.getByTestId("session-recovery-attach"));
    expect(live.observation).toEqual({
      id: "original",
      readOnly: true,
      path: "live-ws?observe=true&expected=opaque-exact-binding&observe_expected=opaque-exact-pane",
    });
    expect(live.manualReconnect).not.toHaveBeenCalled();
    rerender(
      <LiveTerminalView
        readOnly={false}
        recoveryCurrent
        session={{
          ...session,
          recovery: recovery({ observe_expected: "replacement-pane" }),
        }}
      />,
    );
    expect(live.observation?.id).toBeNull();
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
    await waitFor(() => expect(fetchSpy).toHaveBeenCalledOnce());
    await waitFor(() =>
      expect((screen.getByTestId("session-recovery-restart-same") as HTMLButtonElement).disabled).toBe(false),
    );
    expect(live.manualReconnect).not.toHaveBeenCalled();
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
    await waitFor(() =>
      expect(live.ensureTerminal).toHaveBeenCalledExactlyOnceWith("paired", 2, false, expect.any(AbortSignal)),
    );
    expect(screen.queryByTestId("session-recovery")).toBeNull();
    expect(live.ensureSession).not.toHaveBeenCalled();
    fireEvent.click(await screen.findByRole("button", { name: "Restart" }));
    await waitFor(() => expect(live.ensureTerminal).toHaveBeenCalledTimes(2));
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});

describe("LiveTerminalView slower-updates notice in the top bar", () => {
  const pipeNotice = () => document.querySelector("[data-live-slow-notice]");
  // The notice lives in the Session's top bar, outside the pane, so it never resizes a terminal this tab sizes.
  const View = ({ id = "piped" }: { id?: string }) => {
    const session = makeSession({ id, status: "Running" });
    return (
      <>
        <LiveTerminalView session={session} />
        <LiveSlowNotice sessionId={session.id} />
      </>
    );
  };
  const holdControl = () => {
    live.state.paneDead = false;
    live.state.connected = true;
    live.state.ownerKnown = true;
    live.state.isOwner = true;
  };

  it("shows only while this tab holds control on the pipe_owned fallback", async () => {
    holdControl();
    const { rerender } = render(<View />);
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
    expect(pipeNotice()).toBeNull();

    live.state.transportReason = "pipe_owned";
    rerender(<View />);
    expect(pipeNotice()?.textContent).toContain("Updates are slower.");
    expect(document.querySelector("[data-live-control-strip]")).toBeNull();

    // View only: the docked strip says the updates lag instead.
    live.state.isOwner = false;
    rerender(<View />);
    expect(pipeNotice()).toBeNull();
    expect(document.querySelector("[data-live-control-strip]")?.textContent).toContain("Updates may lag");

    live.state.isOwner = true;
    live.state.connected = false;
    rerender(<View />);
    expect(pipeNotice()).toBeNull();
  });

  const mountPiped = async (id = "piped") => {
    holdControl();
    live.state.transportReason = "pipe_owned";
    const utils = render(<View id={id} />);
    await screen.findByRole("button", { name: "Dismiss slower updates notice" });
    return utils;
  };
  const dismiss = () => fireEvent.click(screen.getByRole("button", { name: "Dismiss slower updates notice" }));

  it("Reconnect redials with an explicit claim, not a wait for a free lock", async () => {
    await mountPiped();
    fireEvent.click(screen.getByRole("button", { name: "Reconnect" }));
    expect(live.reconnectAndClaim).toHaveBeenCalledTimes(1);
    expect(live.manualReconnect).not.toHaveBeenCalled();
  });

  it("remembers a dismissal per session across mounts", async () => {
    const { unmount } = await mountPiped();
    dismiss();
    expect(pipeNotice()).toBeNull();
    unmount();

    render(<View />);
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(2));
    expect(pipeNotice()).toBeNull();
    cleanup();

    await mountPiped("other");
    expect(pipeNotice()).not.toBeNull();
  });

  it("forgets a dismissal once a grid transport arrives", async () => {
    const { rerender, unmount } = await mountPiped();
    dismiss();
    act(() => live.onGridTransport?.());
    rerender(<View />);
    expect(pipeNotice()).not.toBeNull();
    unmount();

    await mountPiped();
    expect(pipeNotice()).not.toBeNull();
  });

  it("clears a dismissal another tab stored when this view resets the notice", async () => {
    const resets = {
      grid: () => act(() => live.onGridTransport?.()),
      reconnect: () => fireEvent.click(screen.getByRole("button", { name: "Reconnect" })),
    };
    for (const [name, reset] of Object.entries(resets)) {
      const { unmount } = await mountPiped();
      window.localStorage.setItem("aoe-live-pipe-notice-dismissed:piped:agent:0", "1");
      reset();
      unmount();

      await mountPiped();
      expect(pipeNotice(), name).not.toBeNull();
      cleanup();
    }
  });

  it("forgets a dismissal when the user reconnects from the connection banner", async () => {
    const { rerender } = await mountPiped();
    dismiss();
    live.state.connected = false;
    rerender(<View />);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(live.manualReconnect).toHaveBeenCalledTimes(1);

    live.state.connected = true;
    rerender(<View />);
    expect(pipeNotice()).not.toBeNull();
  });
});
