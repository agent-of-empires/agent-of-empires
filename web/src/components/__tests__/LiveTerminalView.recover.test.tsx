// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
import { LiveSlowNotice } from "../LiveSlowNotice";
import { makeSession } from "./fixtures";

const live = vi.hoisted(() => ({
  state: {
    connected: false,
    reconnecting: false,
    retryCount: 7,
    retryCountdown: 0,
    frame: null,
    reading: false,
    isOwner: false,
    ownerKnown: false,
    ownerKind: null,
    transport: null,
    transportReason: null as string | null,
    paneDead: true,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  ensureSession: vi.fn(async () => ({ ok: true, message: null })),
  manualReconnect: vi.fn(),
  reconnectAndClaim: vi.fn(),
  onGridTransport: undefined as (() => void) | undefined,
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: (_id: unknown, _readOnly: unknown, _path: unknown, _clip: unknown, onGrid: () => void) => {
    live.onGridTransport = onGrid;
    return {
      state: live.state,
      manualReconnect: live.manualReconnect,
      reconnectAndClaim: live.reconnectAndClaim,
      sendData: vi.fn(),
      sendPaste: vi.fn(),
      maxRetries: 7,
      claim: vi.fn(),
    };
  },
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", () => ({
  ensureSession: live.ensureSession,
  ensureTerminal: async () => ({ ok: true, message: null }),
  pasteImage: vi.fn(),
  isStartRefusal: () => false,
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: ({ inputRef }: { inputRef: RefObject<HTMLTextAreaElement | null> }) => (
    <textarea aria-label="Live terminal input" ref={inputRef} />
  ),
}));

beforeEach(() => {
  live.state.paneDead = true;
  live.state.connected = false;
  live.state.transportReason = null;
  live.state.isOwner = false;
  live.state.ownerKnown = false;
  live.ensureSession.mockClear();
  live.manualReconnect.mockClear();
  live.reconnectAndClaim.mockClear();
  window.localStorage.clear();
});
afterEach(cleanup);

const mountDead = async (status: "Running" | "Stopped" = "Running") => {
  const utils = render(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status })} />);
  await screen.findByRole("button", { name: "Restart" });
  await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
  return utils;
};

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

describe("LiveTerminalView after the agent exits", () => {
  it("Restart re-runs ensure so the pane is revived and redialed", async () => {
    await mountDead();
    fireEvent.click(screen.getByRole("button", { name: "Restart" }));
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(2));
    expect(screen.queryByText("Connection lost")).toBeNull();
  });

  it("re-ensures once when the session goes from stopped to running", async () => {
    const { rerender } = await mountDead("Stopped");
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(2));
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    expect(live.ensureSession).toHaveBeenCalledTimes(2);
  });

  it("keeps the Start edge until the dead-pane close arrives", async () => {
    live.state.paneDead = false;
    live.state.connected = true;
    const { rerender } = render(
      <LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Stopped" })} />,
    );
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    expect(live.ensureSession).toHaveBeenCalledTimes(1);
    live.state.connected = false;
    live.state.paneDead = true;
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(2));
  });

  it("does not treat Idle to Running as a Start, so a later exit is not auto-restarted", async () => {
    live.state.paneDead = false;
    live.state.connected = true;
    const { rerender } = render(
      <LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Idle" })} />,
    );
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    live.state.connected = false;
    live.state.paneDead = true;
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    await screen.findByRole("button", { name: "Restart" });
    expect(live.ensureSession).toHaveBeenCalledTimes(1);
  });

  it("does not re-ensure on a status change while the pane is alive", async () => {
    live.state.paneDead = false;
    const { rerender } = render(
      <LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Stopped" })} />,
    );
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
    rerender(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status: "Running" })} />);
    expect(live.ensureSession).toHaveBeenCalledTimes(1);
  });
});
