// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    paneDead: true,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  ensureSession: vi.fn(async () => ({ ok: true, message: null })),
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: () => ({
    ...live,
    sendData: vi.fn(),
    sendPaste: vi.fn(),
    maxRetries: 7,
    manualReconnect: vi.fn(),
    claim: vi.fn(),
  }),
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
  live.ensureSession.mockClear();
});
afterEach(cleanup);

const mountDead = async (status: "Running" | "Stopped" = "Running") => {
  const utils = render(<LiveTerminalView readOnly={false} session={makeSession({ id: "dead", status })} />);
  await screen.findByRole("button", { name: "Restart" });
  await waitFor(() => expect(live.ensureSession).toHaveBeenCalledTimes(1));
  return utils;
};

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
