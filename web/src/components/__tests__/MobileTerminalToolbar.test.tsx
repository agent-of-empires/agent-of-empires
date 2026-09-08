// @vitest-environment jsdom
//
// Unit tests for MobileTerminalToolbar's keyboard wiring (#1432). The strip
// is never rendered under the chromium Playwright coverage run (pointer:coarse
// does not match there), so these exercise it directly: the paste button and
// the Ctrl latch. The parent (a live terminal
// view) always owns the keyboard inset now, so the strip carries none.

import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MobileTerminalToolbar } from "../MobileTerminalToolbar";
import { toastBus } from "../../lib/toastBus";

afterEach(() => {
  cleanup();
  delete (window as { isSecureContext?: boolean }).isSecureContext;
  delete (navigator as { clipboard?: unknown }).clipboard;
});

function secureContext(value: boolean) {
  Object.defineProperty(window, "isSecureContext", { value, configurable: true });
}

interface Overrides {
  keyboardOpen?: boolean;
  sendData?: (data: string) => void;
}

function renderToolbar(overrides: Overrides = {}) {
  const sendData = overrides.sendData ?? vi.fn();
  const inputElRef = { current: null };
  const result = render(
    <MobileTerminalToolbar
      sendData={sendData}
      inputElRef={inputElRef}
      keyboardOpen={overrides.keyboardOpen ?? false}
      ctrlActive={false}
      onCtrlToggle={vi.fn()}
    />,
  );
  return { ...result, sendData };
}

describe("MobileTerminalToolbar", () => {
  it("carries no inline keyboard inset (the parent owns it)", () => {
    const { container } = renderToolbar();
    const strip = container.firstChild as HTMLElement;
    expect(strip.style.paddingBottom).toBe("");
  });

  it("renders the action buttons", () => {
    renderToolbar();
    expect(screen.getByLabelText("Paste from clipboard")).toBeTruthy();
    expect(screen.getByLabelText("Ctrl")).toBeTruthy();
  });

  it("bracket-pastes clipboard text so multi-line pastes are not per-line submits", async () => {
    secureContext(true);
    const item = {
      types: ["text/html", "text/plain"],
      getType: async () => new Blob(["line 1\nline 2"], { type: "text/plain" }),
    };
    Object.defineProperty(navigator, "clipboard", {
      value: { read: async () => [item] },
      configurable: true,
    });
    const { sendData } = renderToolbar({ keyboardOpen: true });

    fireEvent.click(screen.getByLabelText("Paste from clipboard"));
    await waitFor(() => expect(sendData).toHaveBeenCalledWith("\x1b[200~line 1\nline 2\x1b[201~"));
  });

  it("on a plain-HTTP origin pastes through execCommand into the focused input, else explains HTTPS", async () => {
    secureContext(false);
    const error = vi.fn();
    toastBus.handler = { push: vi.fn(), error, info: vi.fn(), openLink: vi.fn() };
    const editable = document.createElement("textarea");
    document.body.appendChild(editable);
    editable.focus();
    const paste = screen.getByLabelText.bind(screen);

    for (const [granted, toasts] of [
      [true, 0],
      [false, 1],
    ] as const) {
      error.mockClear();
      const execCommand = vi.fn(() => granted);
      Object.defineProperty(document, "execCommand", { value: execCommand, configurable: true });
      const { sendData, unmount } = renderToolbar({ keyboardOpen: true });
      fireEvent.click(paste("Paste from clipboard"));
      await new Promise((r) => setTimeout(r, 0));
      expect(execCommand).toHaveBeenCalledWith("paste");
      // The focused input's own paste handler sends; the toolbar never does.
      expect(sendData).not.toHaveBeenCalled();
      expect(error).toHaveBeenCalledTimes(toasts);
      unmount();
    }
    document.body.removeChild(editable);
    toastBus.handler = null;
  });

  it("reports an unreadable clipboard instead of sending anything", async () => {
    secureContext(true);
    Object.defineProperty(navigator, "clipboard", {
      value: { read: async () => Promise.reject(new Error("denied")) },
      configurable: true,
    });
    const error = vi.fn();
    toastBus.handler = { push: vi.fn(), error, info: vi.fn(), openLink: vi.fn() };
    const { sendData } = renderToolbar();

    fireEvent.click(screen.getByLabelText("Paste from clipboard"));
    await waitFor(() => expect(error).toHaveBeenCalledWith(expect.stringContaining("Couldn't read clipboard")));
    expect(sendData).not.toHaveBeenCalled();
    toastBus.handler = null;
  });
});

// User story (ported from the live Playwright acp-stories suite): the
// Ctrl toggle latches the modifier so the next keystroke combines with
// Ctrl. Tapping Ctrl flips aria-pressed to "true"; tapping again flips
// it back. The latch state lives in the parent (TerminalView /
// PairedTerminal hold a useState and pass `onCtrlToggle={() =>
// setCtrlActive(v => !v)}`); this harness mirrors that wiring so the
// toolbar's aria-pressed contract is exercised end to end. The
// modifier-applied keystroke itself is handled by the terminal helper
// textarea and is out of scope here.
function CtrlLatchHarness({ sendData }: { sendData: (data: string) => void }) {
  const [ctrlActive, setCtrlActive] = useState(false);
  return (
    <MobileTerminalToolbar
      sendData={sendData}
      inputElRef={{ current: null }}
      keyboardOpen={false}
      ctrlActive={ctrlActive}
      onCtrlToggle={() => setCtrlActive((v) => !v)}
    />
  );
}

describe("MobileTerminalToolbar Ctrl latch", () => {
  it("tapping Ctrl latches (aria-pressed true) and tapping again unlatches", () => {
    render(<CtrlLatchHarness sendData={vi.fn()} />);
    const ctrl = screen.getByRole("button", { name: "Ctrl" });
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");

    fireEvent.click(ctrl);
    expect(ctrl.getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(ctrl);
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");
  });

  it("Ctrl+C interrupt clears an active latch", () => {
    const sendData = vi.fn();
    render(<CtrlLatchHarness sendData={sendData} />);
    const ctrl = screen.getByRole("button", { name: "Ctrl" });

    fireEvent.click(ctrl);
    expect(ctrl.getAttribute("aria-pressed")).toBe("true");

    fireEvent.click(screen.getByRole("button", { name: "Ctrl+C interrupt" }));
    expect(sendData).toHaveBeenCalledWith("\x03");
    expect(ctrl.getAttribute("aria-pressed")).toBe("false");
  });
});
