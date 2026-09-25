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
  it("'off' offers Enable, which calls hook.enable()", () => {
    const container = renderFor({ kind: "off" });
    expect(buttonByText(container, "Send test notification")).toBeNull();
    fireEvent.click(buttonByText(container, "Enable notifications")!);
    expect(enable).toHaveBeenCalledTimes(1);
  });

  it("'enabled' offers Send test, Re-subscribe, and Turn off wired to their primitives", () => {
    const container = renderFor({ kind: "enabled" });
    expect(buttonByText(container, "Enable notifications")).toBeNull();
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
    ["'asking' shows status text instead of Enable", { kind: "asking" }, "Asking your browser", false],
  ] as [string, PushState, string | null, boolean][])("%s", (_name, state, text, enableShown) => {
    const container = renderFor(state);
    if (text) expect(container.textContent).toContain(text);
    expect(buttonByText(container, "Enable notifications") !== null).toBe(enableShown);
  });
});
