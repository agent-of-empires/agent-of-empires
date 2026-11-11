// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { BLOCKED_HOLD_MS, LiveTerminalView } from "../LiveTerminalView";
import { makeSession } from "./fixtures";

const live = vi.hoisted(() => ({
  state: {
    connected: true,
    reconnecting: false,
    retryCount: 0,
    retryCountdown: 0,
    frame: null,
    reading: false,
    isOwner: false,
    ownerKnown: true,
    ownerKind: null as "web" | "tui" | null,
    transport: null,
    transportReason: null as string | null,
    paneDead: false,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  sendData: vi.fn((_data: string) => false),
  sendPaste: vi.fn((_text: string, _submit: boolean) => false),
  claim: vi.fn(),
  claimAfterResize: vi.fn((_deadlineMs: number) => {}),
  // What LiveTerminalView handed the terminal: typed keys and pastes go through these.
  typed: null as ((data: string) => boolean) | null,
  pasted: null as ((text: string, submit: boolean) => boolean) | null,
  tabEscape: null as (() => boolean) | null,
  coarse: false,
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: () => ({
    state: live.state,
    manualReconnect: vi.fn(),
    reconnectAndClaim: vi.fn(),
    sendData: live.sendData,
    sendPaste: live.sendPaste,
    maxRetries: 7,
    claim: live.claim,
    claimAfterResize: live.claimAfterResize,
  }),
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => live.coarse }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", () => ({
  ensureSession: async () => ({ ok: true, message: null }),
  ensureTerminal: async () => ({ ok: true, message: null }),
  pasteImage: vi.fn(),
  isStartRefusal: () => false,
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: (props: {
    inputRef: RefObject<HTMLTextAreaElement | null>;
    sendData: (data: string) => boolean;
    sendPaste: (text: string, submit: boolean) => boolean;
    onTabEscape?: () => boolean;
  }) => {
    live.typed = props.sendData;
    live.pasted = props.sendPaste;
    live.tabEscape = props.onTabEscape ?? null;
    return <textarea aria-label="Live terminal input" ref={props.inputRef} />;
  },
}));

beforeEach(() => {
  Object.assign(live.state, {
    connected: true,
    reconnecting: false,
    retryCount: 0,
    isOwner: false,
    ownerKnown: true,
    ownerKind: null,
    transportReason: null,
  });
  live.sendData.mockClear();
  live.sendPaste.mockClear();
  live.claim.mockClear();
  live.claimAfterResize.mockClear();
  live.coarse = false;
  window.localStorage.clear();
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const strip = () => document.querySelector<HTMLElement>("[data-live-control-strip]");
const status = () => strip()?.querySelector('[role="status"]')?.textContent;
type Surface = "agent" | "paired-host";
const View = ({ id = "s1", readOnly = false, surface = "agent" as Surface, index = 0 }) => (
  <LiveTerminalView
    session={makeSession({ id, status: "Running" })}
    readOnly={readOnly}
    surface={surface}
    terminalIndex={index}
  />
);
const mount = async (props: Parameters<typeof View>[0] = {}) => {
  const utils = render(<View {...props} />);
  await screen.findByLabelText("Live terminal input");
  return utils;
};
const typeBlocked = () => act(() => void live.typed?.("x"));

describe("LiveTerminalView control strip", () => {
  it("names the window that has control, falls back when the Daemon doesn't say, and adds the lag only for pipe_owned", async () => {
    const cases: [typeof live.state.ownerKind, string | null, string][] = [
      ["tui", null, "Open in the TUI. Take control to type."],
      ["web", null, "Open in another Web dashboard window. Take control to type."],
      [null, null, "Open in another window. Take control to type."],
      ["tui", "pipe_owned", "Open in the TUI. Take control to type. · Updates may lag"],
    ];
    for (const [ownerKind, transportReason, text] of cases) {
      Object.assign(live.state, { ownerKind, transportReason });
      await mount();
      expect(status(), `${ownerKind} ${transportReason}`).toBe(text);
      cleanup();
    }
  });

  it("shows nothing while this tab holds control or is disconnected, and keeps the connection banner", async () => {
    live.state.isOwner = true;
    const { rerender } = await mount();
    expect(strip()).toBeNull();
    Object.assign(live.state, { isOwner: false, ownerKnown: false, connected: false, retryCount: 7 });
    rerender(<View />);
    expect(strip()).toBeNull();
    expect(screen.getByText("Connection lost")).toBeTruthy();
  });

  it("Take control hides the strip before it claims, so the claim resizes the pane once", async () => {
    await mount();
    const region = strip()!.querySelector('[role="status"]')!;
    expect(region.querySelector("button")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Take control" }));
    // The claim waits for the grid the view reports once the strip is gone.
    expect(strip()).toBeNull();
    expect(live.claimAfterResize).toHaveBeenCalledTimes(1);
    expect(live.claim).not.toHaveBeenCalled();
  });

  it("brings the strip back when the claim isn't granted, and keeps it hidden once it is", async () => {
    const { rerender } = await mount();
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Take control" }));
    act(() => vi.advanceTimersByTime(4999));
    expect(strip()).toBeNull();
    act(() => vi.advanceTimersByTime(1));
    expect(status()).toBe("Open in another window. Take control to type.");

    fireEvent.click(screen.getByRole("button", { name: "Take control" }));
    live.state.isOwner = true;
    rerender(<View />);
    // Control lost again later: the strip is back at once.
    live.state.isOwner = false;
    rerender(<View />);
    expect(status()).toBe("Open in another window. Take control to type.");
  });

  it("holds the not-sent text for 2 s after a blocked key, restarting on each one", async () => {
    await mount();
    vi.useFakeTimers();
    typeBlocked();
    expect(strip()!.dataset.liveControlStrip).toBe("hold");
    expect(status()).toBe("Not sent. Take control to type.");
    act(() => vi.advanceTimersByTime(BLOCKED_HOLD_MS - 500));
    typeBlocked();
    act(() => vi.advanceTimersByTime(BLOCKED_HOLD_MS - 1));
    expect(status()).toBe("Not sent. Take control to type.");
    act(() => vi.advanceTimersByTime(1));
    expect(status()).toBe("Open in another window. Take control to type.");
  });

  it("drops the hold at once when this tab gets control", async () => {
    const { rerender } = await mount();
    typeBlocked();
    expect(status()).toBe("Not sent. Take control to type.");
    live.state.isOwner = true;
    rerender(<View />);
    expect(strip()).toBeNull();
  });

  it("takes Tab to Take control only during the hold, and leaves it to the terminal otherwise", async () => {
    const { rerender } = await mount();
    const input = screen.getByLabelText<HTMLTextAreaElement>("Live terminal input");
    const takeControl = () => screen.getByRole("button", { name: "Take control" });
    const tab = () => {
      input.focus();
      return live.tabEscape!();
    };
    expect(tab(), "view only, no hold").toBe(false);
    expect(document.activeElement).toBe(input);

    typeBlocked();
    expect(tab(), "during the hold").toBe(true);
    expect(document.activeElement).toBe(takeControl());

    live.state.isOwner = true;
    rerender(<View />);
    expect(tab(), "holding control, right after a hold").toBe(false);
    expect(document.activeElement).toBe(input);
  });

  it("Take control gives focus back to where the Tab escape took it from, else to the input on a fine pointer only", async () => {
    const proxy = document.createElement("textarea");
    document.body.append(proxy);
    const cases: [string, boolean, "input" | "proxy" | null, "input" | "proxy" | "not input"][] = [
      ["escape from the input", false, "input", "input"],
      ["escape from App's proxy, touch", true, "proxy", "proxy"],
      ["desktop click, no escape", false, null, "input"],
      ["touch, no escape", true, null, "not input"],
    ];
    for (const [name, coarse, from, back] of cases) {
      live.coarse = coarse;
      await mount();
      const input = screen.getByLabelText<HTMLTextAreaElement>("Live terminal input");
      const button = screen.getByRole("button", { name: "Take control" });
      typeBlocked();
      if (from) {
        (from === "input" ? input : proxy).focus();
        expect(live.tabEscape!(), name).toBe(true);
      } else {
        button.focus();
      }
      expect(document.activeElement, name).toBe(button);
      fireEvent.click(button);
      expect(strip(), name).toBeNull();
      if (back === "not input") expect(document.activeElement, name).not.toBe(input);
      else expect(document.activeElement, name).toBe(back === "input" ? input : proxy);
      cleanup();
    }
    proxy.remove();
  });

  it("collapses to View only, remembered per Session, surface and pane", async () => {
    const { unmount } = await mount();
    fireEvent.click(screen.getByRole("button", { name: "Collapse notice" }));
    expect(screen.getByRole("button", { name: "View only. Show notice" })).toBeTruthy();
    expect(status()).toBeUndefined();
    unmount();

    await mount();
    expect(screen.getByRole("button", { name: "View only. Show notice" })).toBeTruthy();
    cleanup();
    for (const other of [
      { id: "s2" },
      { surface: "paired-host" as Surface },
      { surface: "paired-host" as Surface, index: 1 },
    ]) {
      await mount(other);
      expect(screen.queryByRole("button", { name: "View only. Show notice" }), JSON.stringify(other)).toBeNull();
      cleanup();
    }

    await mount();
    // A blocked key still shows its hold over the collapsed button.
    typeBlocked();
    expect(status()).toBe("Not sent. Take control to type.");
    fireEvent.click(screen.getByRole("button", { name: "Take control" }));
    expect(live.claimAfterResize).toHaveBeenCalledTimes(1);
    cleanup();

    await mount();
    fireEvent.click(screen.getByRole("button", { name: "View only. Show notice" }));
    expect(status()).toBe("Open in another window. Take control to type.");
    cleanup();
    await mount();
    expect(screen.queryByRole("button", { name: "View only. Show notice" })).toBeNull();
  });

  it("shows a steady read-only strip with no buttons, and its own hold text", async () => {
    Object.assign(live.state, { ownerKnown: false });
    await mount({ readOnly: true });
    expect(status()).toBe("This Daemon is read-only.");
    expect(strip()!.querySelectorAll("button")).toHaveLength(0);
    act(() => void live.pasted?.("x", false));
    expect(status()).toBe("Not sent: this Daemon is read-only.");
    expect(strip()!.className).not.toContain("ring-brand");
  });

  it("while disconnected, holds 'terminal not connected' for explicit sends only", async () => {
    Object.assign(live.state, { connected: false, ownerKnown: false, retryCount: 7 });
    await mount();
    typeBlocked();
    expect(strip()).toBeNull();
    act(() => void live.pasted?.("x", false));
    expect(status()).toBe("Not sent: terminal not connected.");
    await waitFor(() => expect(screen.getByText("Connection lost")).toBeTruthy());
  });
});
