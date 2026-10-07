// @vitest-environment jsdom

import { afterEach, describe, expect, it } from "vitest";
import { renderHook } from "@testing-library/react";
import { useRef } from "react";

import { useFocusTerminalTarget } from "../useFocusTerminalTarget";
import { consumePendingTerminalFocus, dispatchFocusTerminal, setPendingTerminalFocus } from "../../lib/terminalFocus";

afterEach(() => {
  consumePendingTerminalFocus("composer");
  consumePendingTerminalFocus("agent");
  document.body.replaceChildren();
});

function mountTextarea(): HTMLElement {
  const el = document.createElement("textarea");
  document.body.appendChild(el);
  return el;
}

function renderWithElement(target: "composer" | "agent", el: HTMLElement | null) {
  return renderHook(() => {
    const ref = useRef<HTMLElement | null>(el);
    useFocusTerminalTarget(target, ref);
    return ref;
  });
}

describe("useFocusTerminalTarget", () => {
  it("focuses the ref only for a focus event naming its own target", () => {
    const el = mountTextarea();
    renderWithElement("composer", el);
    expect(document.activeElement).not.toBe(el);

    dispatchFocusTerminal("agent");
    expect(document.activeElement).not.toBe(el);

    window.dispatchEvent(new CustomEvent("aoe:focus-terminal"));
    expect(document.activeElement).not.toBe(el);

    dispatchFocusTerminal("composer");
    expect(document.activeElement).toBe(el);
  });

  it("latches an event that arrives with no element, then spends the latch on mount", () => {
    renderWithElement("composer", null);
    dispatchFocusTerminal("composer");
    expect(consumePendingTerminalFocus("composer")).toBe(true);

    setPendingTerminalFocus("composer");
    const el = mountTextarea();
    renderWithElement("composer", el);
    expect(document.activeElement).toBe(el);
    expect(consumePendingTerminalFocus("composer")).toBe(false);
  });

  it("spends a mount latch even when there is no element to focus", () => {
    setPendingTerminalFocus("composer");
    renderWithElement("composer", null);
    expect(consumePendingTerminalFocus("composer")).toBe(false);
  });

  it("removes its listener on unmount", () => {
    const el = mountTextarea();
    const { unmount } = renderWithElement("composer", el);
    unmount();
    el.blur();
    dispatchFocusTerminal("composer");
    expect(document.activeElement).not.toBe(el);
  });
});
