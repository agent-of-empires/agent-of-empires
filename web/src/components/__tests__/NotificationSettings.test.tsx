// @vitest-environment jsdom
//
// Contract test for the NotificationSettings panel. The real Web Push
// flow is non-trivial to run in jsdom (no ServiceWorker, no PushManager,
// no Notification.requestPermission), so this suite mocks the
// usePushSubscription hook entirely and asserts the rendered UI matches
// the hook state plus that user actions invoke the corresponding hook
// primitives. Part of #1217.

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
    setState({ kind: "off" });
    const { container } = render(<NotificationSettings />);
    expect(buttonByText(container, "Enable notifications")).not.toBeNull();
    expect(buttonByText(container, "Send test notification")).toBeNull();
  });

  it("clicking Enable calls hook.enable()", () => {
    setState({ kind: "off" });
    const { container } = render(<NotificationSettings />);
    const btn = buttonByText(container, "Enable notifications")!;
    fireEvent.click(btn);
    expect(enable).toHaveBeenCalledTimes(1);
  });

  it("when 'enabled', shows Send test, Re-subscribe, Turn off; hides Enable", () => {
    setState({ kind: "enabled" });
    const { container } = render(<NotificationSettings />);
    expect(buttonByText(container, "Send test notification")).not.toBeNull();
    expect(buttonByText(container, "Re-subscribe")).not.toBeNull();
    expect(buttonByText(container, "Turn off")).not.toBeNull();
    expect(buttonByText(container, "Enable notifications")).toBeNull();
  });

  it("clicking Send test, Re-subscribe, Turn off invokes the right primitives", () => {
    setState({ kind: "enabled" });
    const { container } = render(<NotificationSettings />);
    fireEvent.click(buttonByText(container, "Send test notification")!);
    fireEvent.click(buttonByText(container, "Re-subscribe")!);
    fireEvent.click(buttonByText(container, "Turn off")!);
    expect(sendTest).toHaveBeenCalledTimes(1);
    expect(resubscribe).toHaveBeenCalledTimes(1);
    expect(disable).toHaveBeenCalledTimes(1);
  });

  it("'denied' state still renders the Enable button", () => {
    setState({ kind: "denied" });
    const { container } = render(<NotificationSettings />);
    expect(buttonByText(container, "Enable notifications")).not.toBeNull();
  });

  it("'error' state renders the message and the Enable button", () => {
    setState({ kind: "error", message: "boom" });
    const { container } = render(<NotificationSettings />);
    expect(container.textContent).toContain("boom");
    expect(buttonByText(container, "Enable notifications")).not.toBeNull();
  });

  it("'unsupported / ios-not-standalone' renders the install help block", () => {
    setState({ kind: "unsupported", reason: "ios-not-standalone" });
    const { container } = render(<NotificationSettings />);
    expect(container.textContent).toContain("How to install on iPhone");
    expect(buttonByText(container, "Enable notifications")).toBeNull();
  });

  it("'unsupported / insecure-origin' surfaces the HTTPS hint", () => {
    setState({ kind: "unsupported", reason: "insecure-origin" });
    const { container } = render(<NotificationSettings />);
    expect(container.textContent).toContain("require HTTPS");
  });

  it("'disabled-by-server' surfaces the server-disabled hint", () => {
    setState({ kind: "disabled-by-server" });
    const { container } = render(<NotificationSettings />);
    expect(container.textContent).toContain("turned off by the server");
    expect(buttonByText(container, "Enable notifications")).toBeNull();
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
