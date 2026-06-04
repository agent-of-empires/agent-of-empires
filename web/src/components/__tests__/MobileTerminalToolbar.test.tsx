// @vitest-environment jsdom
//
// Unit tests for MobileTerminalToolbar's keyboard wiring (#1432). The strip
// is never rendered under the chromium Playwright coverage run (pointer:coarse
// does not match there), so these exercise it directly: the parent-handles-
// inset padding switch and the keyboard-open paste fallback branch.

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
