// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
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
    transport: null,
    transportReason: null as string | null,
    paneDead: true,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  ensureSession: vi.fn(async () => ({ ok: true, message: null })),
  manualReconnect: vi.fn(),
  onGridTransport: undefined as (() => void) | undefined,
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: (_id: unknown, _readOnly: unknown, _path: unknown, _clip: unknown, onGrid: () => void) => {
    live.onGridTransport = onGrid;
    return {
      state: live.state,
      manualReconnect: live.manualReconnect,
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
  live.ensureSession.mockClear();
  live.manualReconnect.mockClear();
  window.localStorage.clear();
});
afterEach(cleanup);

const mountDead = async (status: "Running" | "Stopped" = "Running") => {
  const utils = render(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status })} />);
  await screen.findByRole("button", { name: "Restart" });
  await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
  return utils;
};

describe("LiveTerminalView pipe contention notice", () => {
  const pipeNotice = () => document.querySelector("[data-live-pipe-notice]");

  it("shows only while connected on the pipe_owned fallback", async () => {
    live.state.paneDead = false;
    live.state.connected = true;
    const session = makeSession({ id: "piped", status: "Running" });
    const { rerender } = render(<LiveTerminalView session={session} />);
    await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
    expect(pipeNotice()).toBeNull();

    live.state.transportReason = "pipe_owned";
    rerender(<LiveTerminalView session={session} />);
    expect(pipeNotice()?.textContent).toContain("another client is rendering this pane");

    live.state.connected = false;
    rerender(<LiveTerminalView session={session} />);
    expect(pipeNotice()).toBeNull();
  });

  const mountPiped = async (id = "piped") => {
    live.state.paneDead = false;
    live.state.connected = true;
    live.state.transportReason = "pipe_owned";
    const utils = render(<LiveTerminalView session={makeSession({ id, status: "Running" })} />);
    await screen.findByRole("button", { name: "Dismiss slower updates notice" });
    return utils;
  };
  const dismiss = () => fireEvent.click(screen.getByRole("button", { name: "Dismiss slower updates notice" }));

  it("Retry reconnects the live terminal", async () => {
    await mountPiped();
    fireEvent.click(screen.getByRole("button", { name: "Retry faster updates" }));
    expect(live.manualReconnect).toHaveBeenCalledTimes(1);
  });

  it("remembers a dismissal per session across mounts", async () => {
    const { unmount } = await mountPiped();
    dismiss();
    expect(pipeNotice()).toBeNull();
    unmount();

    render(<LiveTerminalView session={makeSession({ id: "piped", status: "Running" })} />);
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
    rerender(<LiveTerminalView session={makeSession({ id: "piped", status: "Running" })} />);
    expect(pipeNotice()).not.toBeNull();
    unmount();

    await mountPiped();
    expect(pipeNotice()).not.toBeNull();
  });

  it("clears a dismissal another tab stored when this view resets the notice", async () => {
    const resets = {
      grid: () => act(() => live.onGridTransport?.()),
      retry: () => fireEvent.click(screen.getByRole("button", { name: "Retry faster updates" })),
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
    const session = makeSession({ id: "piped", status: "Running" });
    live.state.connected = false;
    rerender(<LiveTerminalView session={session} />);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(live.manualReconnect).toHaveBeenCalledTimes(1);

    live.state.connected = true;
    rerender(<LiveTerminalView session={session} />);
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
