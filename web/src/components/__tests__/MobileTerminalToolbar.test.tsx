// @vitest-environment jsdom
//
// Unit tests for MobileTerminalToolbar's keyboard wiring (#1432). The strip
// is never rendered under the chromium Playwright coverage run (pointer:coarse
// does not match there), so these exercise it directly: the parent-handles-
// inset padding switch and the keyboard-open paste fallback branch.

import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MobileTerminalToolbar } from "../MobileTerminalToolbar";

afterEach(() => {
  cleanup();
  // Drop the per-test isSecureContext override (set in the paste-branch test)
  // so it falls back to the default and does not leak into other tests.
  delete (window as { isSecureContext?: boolean }).isSecureContext;
});

interface Overrides {
  keyboardOpen?: boolean;
  parentHandlesKeyboardInset?: boolean;
  sendData?: (data: string) => void;
}

function renderToolbar(overrides: Overrides = {}) {
  const sendData = overrides.sendData ?? vi.fn();
  const result = render(
    <MobileTerminalToolbar
      sendData={sendData}
      termRef={{ current: null }}
      keyboardOpen={overrides.keyboardOpen ?? false}
      parentHandlesKeyboardInset={overrides.parentHandlesKeyboardInset}
      ctrlActive={false}
      onCtrlToggle={vi.fn()}
    />,
  );
  return { ...result, sendData };
}

describe("MobileTerminalToolbar keyboard inset", () => {
  it("sits flush (padding 0) when the parent already pads for the keyboard", () => {
    const { container } = renderToolbar({ parentHandlesKeyboardInset: true });
    const strip = container.firstChild as HTMLElement;
    // jsdom normalizes the "0" string to "0px".
    expect(strip.style.paddingBottom).toBe("0px");
  });

  it("does not pin to 0 when the parent does not handle the inset", () => {
    const { container } = renderToolbar({ parentHandlesKeyboardInset: false });
    const strip = container.firstChild as HTMLElement;
    // The fallback uses env(keyboard-inset-height, 0px); whatever jsdom keeps,
    // it must not be the flush "0px" the parent-handled case produces.
    expect(strip.style.paddingBottom).not.toBe("0px");
  });

  it("renders the action buttons", () => {
    renderToolbar();
    expect(screen.getByLabelText("Paste from clipboard")).toBeTruthy();
    expect(screen.getByLabelText("Ctrl")).toBeTruthy();
  });

  it("takes the keyboard-open paste branch when an editable is focused", async () => {
    // Force the execCommand fallback path: skip the Clipboard API branch.
    Object.defineProperty(window, "isSecureContext", {
      value: false,
      configurable: true,
    });
    const { sendData } = renderToolbar({ keyboardOpen: true });

    const editable = document.createElement("textarea");
    document.body.appendChild(editable);
    editable.focus();

    fireEvent.click(screen.getByLabelText("Paste from clipboard"));
    // The onClick handler is async; let its microtasks settle. With no
    // clipboard data recovered it falls through without sending anything.
    await new Promise((r) => setTimeout(r, 0));

    expect(sendData).not.toHaveBeenCalled();
    document.body.removeChild(editable);
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
      termRef={{ current: null }}
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
