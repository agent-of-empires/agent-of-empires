// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { ToastBusBridge, ToastProvider } from "../Toasts";
import { toastBus } from "../../lib/toastBus";
import { OPEN_SESSION_EVENT } from "../../lib/sessionRoute";

// jsdom ships no navigator.serviceWorker; the ToastProvider effect bails out without one.
const swTarget = new EventTarget();
Object.defineProperty(navigator, "serviceWorker", {
  value: swTarget,
  configurable: true,
});

function dispatchPush(data: unknown) {
  act(() => {
    swTarget.dispatchEvent(new MessageEvent("message", { data }));
  });
}

// Render the provider with the bridge so the module-level toastBus.handler
// is wired to the live React context, matching the real app composition.
function renderProvider() {
  return render(
    <ToastProvider>
      <ToastBusBridge />
    </ToastProvider>,
  );
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  act(() => {
    vi.runOnlyPendingTimers();
  });
  vi.useRealTimers();
  toastBus.handler = null;
});

describe("ToastProvider", () => {
  it("renders info, push, and error toasts with their roles and dismisses on click", () => {
    const { container } = renderProvider();
    expect(container.querySelector("div.fixed")?.children.length).toBe(0);
    act(() => {
      toastBus.handler?.info("hello info");
      toastBus.handler?.push("plain push");
      toastBus.handler?.error("boom");
    });
    const [info, push] = screen.getAllByRole("status");
    expect(info!.textContent).toContain("hello info");
    expect(push!.textContent).toContain("plain push");
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("boom");
    expect(alert.className).toContain("status-error");

    const dismiss = screen.getAllByRole("button", { name: "Dismiss" });
    expect(dismiss.length).toBe(3);
    act(() => fireEvent.click(dismiss[2]!));
    expect(screen.queryByText("boom")).toBeNull();
  });

  it("removes a toast after its 6s lifetime elapses", () => {
    renderProvider();
    act(() => toastBus.handler?.info("temporary"));
    act(() => vi.advanceTimersByTime(5999));
    expect(screen.queryByText("temporary")).toBeTruthy();
    act(() => vi.advanceTimersByTime(1));
    expect(screen.queryByText("temporary")).toBeNull();
  });
});

describe("service-worker push toasts", () => {
  it("turns an aoe-push message with a session id into a clickable toast", () => {
    renderProvider();
    dispatchPush({ type: "aoe-push", payload: { title: "Done", body: "ready", session_id: "sess-1" } });

    const toast = screen.getByText("Done: ready").closest("div");
    expect(toast).not.toBeNull();
    expect(toast?.className).toContain("cursor-pointer");

    const onOpen = vi.fn();
    window.addEventListener(OPEN_SESSION_EVENT, onOpen);
    act(() => fireEvent.click(toast as HTMLElement));
    expect(onOpen).toHaveBeenCalledOnce();
    expect((onOpen.mock.calls[0][0] as CustomEvent).detail).toEqual({ sessionId: "sess-1" });
    expect(screen.queryByText("Done: ready")).toBeNull();
    window.removeEventListener(OPEN_SESSION_EVENT, onOpen);
  });

  it("falls back to the title or default title, and ignores non-aoe-push messages", () => {
    renderProvider();
    dispatchPush({ type: "something-else", payload: { title: "nope" } });
    dispatchPush(null);
    dispatchPush({ type: "aoe-push" });
    expect(screen.queryByRole("status")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();

    dispatchPush({ type: "aoe-push", payload: { title: "Heads up", session_id: "s2" } });
    expect(screen.getByText("Heads up")).toBeTruthy();
    dispatchPush({ type: "aoe-push", payload: { body: "just a body" } });
    const toast = screen.getByText("Agent of Empires: just a body").closest("div");
    expect(toast?.getAttribute("role")).toBe("status");
    expect(toast?.className).not.toContain("cursor-pointer");
  });
});

describe("click-to-open (ui.open_url) toast", () => {
  it("opens the href in a new tab on click and dismisses", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    renderProvider();
    act(() => toastBus.handler?.openLink("Open link", "https://example.com/pr/1"));

    const toast = screen.getByText("Open link").closest("div");
    expect(toast?.className).toContain("cursor-pointer");

    act(() => fireEvent.click(toast as HTMLElement));
    expect(open).toHaveBeenCalledWith("https://example.com/pr/1", "_blank", "noopener,noreferrer");
    expect(screen.queryByText("Open link")).toBeNull();
    open.mockRestore();
  });
});

describe("ToastBusBridge", () => {
  it("wires the bus only inside a provider and unwires on unmount", () => {
    const outside = render(<ToastBusBridge />);
    expect(toastBus.handler).toBeNull();
    outside.unmount();
    const { unmount } = renderProvider();
    expect(toastBus.handler).not.toBeNull();
    unmount();
    expect(toastBus.handler).toBeNull();
  });
});
