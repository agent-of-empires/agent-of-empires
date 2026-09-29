// @vitest-environment jsdom

import type { RefObject } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { LiveTerminalView } from "../LiveTerminalView";
import { toastBus } from "../../lib/toastBus";
import { makeSession } from "./fixtures";

const live = vi.hoisted(() => ({
  state: {
    connected: true,
    reconnecting: false,
    retryCount: 0,
    retryCountdown: 0,
    frame: null,
    reading: false,
    isOwner: true,
    ownerKnown: true,
    transport: null,
    stats: { frames: 0, patches: 0, wireBytes: 0, resyncs: 0 },
  },
  sendData: vi.fn<(data: string) => boolean>(),
  sendPaste: vi.fn<(text: string, submit: boolean) => boolean>(),
}));

vi.mock("../../hooks/useLiveTerminal", () => ({
  useLiveTerminal: () => ({ ...live, maxRetries: 5, manualReconnect: vi.fn(), claim: vi.fn() }),
}));
vi.mock("../../hooks/useIsCoarsePointer", () => ({ useIsCoarsePointer: () => true }));
vi.mock("../../hooks/useMobileKeyboard", () => ({
  useMobileKeyboard: () => ({ keyboardHeight: 0, keyboardOpen: false }),
}));
vi.mock("../../lib/api", () => ({
  ensureSession: async () => ({ ok: true, message: null }),
  ensureTerminal: async () => true,
  pasteImage: vi.fn(),
}));
vi.mock("../MobileLiveTerminal", () => ({
  MobileLiveTerminal: ({ inputRef }: { inputRef: RefObject<HTMLTextAreaElement | null> }) => (
    <textarea aria-label="Live terminal input" ref={inputRef} />
  ),
}));

afterEach(() => {
  cleanup();
  toastBus.handler = null;
  live.state.isOwner = true;
});

async function openCompose(sessionId: string, delivered: boolean) {
  live.sendPaste.mockReset().mockReturnValue(delivered);
  const error = vi.fn();
  toastBus.handler = { push: vi.fn(), error, info: vi.fn(), openLink: vi.fn() };
  render(<LiveTerminalView session={makeSession({ id: sessionId })} />);
  fireEvent.click(await screen.findByRole("button", { name: "Compose" }));
  const text = screen.getByLabelText<HTMLTextAreaElement>("Message");
  fireEvent.input(text, { target: { value: "ship it" } });
  fireEvent.click(screen.getByRole("button", { name: "Send" }));
  return { error, text };
}

describe("LiveTerminalView compose", () => {
  it("sends the draft as a submitting paste and returns focus to the terminal input", async () => {
    const { error } = await openCompose("compose-ok", true);
    expect(live.sendPaste).toHaveBeenCalledWith("ship it", true);
    expect(screen.queryByRole("dialog", { name: "Compose" })).toBeNull();
    expect(document.activeElement).toBe(screen.getByLabelText("Live terminal input"));
    expect(error).not.toHaveBeenCalled();
  });

  it("toasts an undelivered paste and keeps the sheet and draft", async () => {
    live.state.isOwner = false;
    const { error, text } = await openCompose("compose-refused", false);
    expect(error).toHaveBeenCalledWith(expect.stringContaining("another device"));
    expect(screen.getByRole("dialog", { name: "Compose" })).toBeTruthy();
    expect(text.value).toBe("ship it");
  });
});
