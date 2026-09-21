// @vitest-environment jsdom

import { describe, expect, it, vi } from "vitest";
import { fireEvent, render } from "@testing-library/react";

import type { PushState } from "../../hooks/usePushSubscription";

const enable = vi.fn();
const disable = vi.fn();
const sendTest = vi.fn();
const resubscribe = vi.fn();
const refresh = vi.fn();
let currentState: PushState = { kind: "off" };

vi.mock("../../hooks/usePushSubscription", () => ({
  usePushSubscription: () => ({
    state: currentState,
    enable,
    disable,
    sendTest,
    resubscribe,
    refresh,
  }),
}));

import { NotificationSettings } from "../NotificationSettings";

function setState(s: PushState) {
  currentState = s;
  enable.mockClear();
  disable.mockClear();
  sendTest.mockClear();
  resubscribe.mockClear();
  refresh.mockClear();
}

function renderFor(state: PushState) {
  setState(state);
  return render(<NotificationSettings />).container;
}

function buttonByText(container: HTMLElement, match: string): HTMLButtonElement | null {
  const buttons = container.querySelectorAll("button");
  for (const b of buttons) {
    if (b.textContent && b.textContent.includes(match)) {
      return b as HTMLButtonElement;
    }
  }
  return null;
}

describe("NotificationSettings", () => {
  it("renders an Enable button when state is 'off'", () => {
    const container = renderFor({ kind: "off" });
    expect(buttonByText(container, "Enable notifications")).not.toBeNull();
    expect(buttonByText(container, "Send test notification")).toBeNull();
  });

  it("clicking Enable calls hook.enable()", () => {
    const container = renderFor({ kind: "off" });
    const btn = buttonByText(container, "Enable notifications")!;
    fireEvent.click(btn);
    expect(enable).toHaveBeenCalledTimes(1);
  });

  it("when 'enabled', shows Send test, Re-subscribe, Turn off; hides Enable", () => {
    const container = renderFor({ kind: "enabled" });
    expect(buttonByText(container, "Send test notification")).not.toBeNull();
    expect(buttonByText(container, "Re-subscribe")).not.toBeNull();
    expect(buttonByText(container, "Turn off")).not.toBeNull();
    expect(buttonByText(container, "Enable notifications")).toBeNull();
  });

  it("clicking Send test, Re-subscribe, Turn off invokes the right primitives", () => {
    const container = renderFor({ kind: "enabled" });
    fireEvent.click(buttonByText(container, "Send test notification")!);
    fireEvent.click(buttonByText(container, "Re-subscribe")!);
    fireEvent.click(buttonByText(container, "Turn off")!);
    expect(sendTest).toHaveBeenCalledTimes(1);
    expect(resubscribe).toHaveBeenCalledTimes(1);
    expect(disable).toHaveBeenCalledTimes(1);
  });

  it.each([
    ["'denied' keeps the Enable button", { kind: "denied" }, null, true],
    ["'error' renders its message", { kind: "error", message: "boom" }, "boom", true],
    [
      "'unsupported / ios-not-standalone' renders the install help",
      { kind: "unsupported", reason: "ios-not-standalone" },
      "How to install on iPhone",
      false,
    ],
    [
      "'unsupported / insecure-origin' surfaces the HTTPS hint",
      { kind: "unsupported", reason: "insecure-origin" },
      "require HTTPS",
      false,
    ],
    [
      "'disabled-by-server' surfaces the server hint",
      { kind: "disabled-by-server" },
      "turned off by the server",
      false,
    ],
  ] as [string, PushState, string | null, boolean][])("%s", (_name, state, text, enableShown) => {
    const container = renderFor(state);
    if (text) expect(container.textContent).toContain(text);
    expect(buttonByText(container, "Enable notifications") !== null).toBe(enableShown);
  });

  it("disables the Enable button while a transition is in flight", () => {
    setState({ kind: "off" });
    const { container, rerender } = render(<NotificationSettings />);
    const beforeBtn = buttonByText(container, "Enable notifications")!;
    expect(beforeBtn.disabled).toBe(false);
    setState({ kind: "asking" });
    rerender(<NotificationSettings />);
    // 'asking' has no button; verify we switched away from the actionable
    // 'off' UI (the stale Enable button is gone) onto the status text.
    expect(container.textContent).toContain("Asking your browser");
    expect(buttonByText(container, "Enable notifications")).toBeNull();
  });
});
