// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
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
  it("runs an action toast's button once and dismisses it", () => {
    renderProvider();
    const reopen = vi.fn();
    act(() => toastBus.handler?.action("Session was not created", "Reopen", reopen));
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("Session was not created");
    fireEvent.click(screen.getByRole("button", { name: "Reopen" }));
    expect(reopen).toHaveBeenCalledOnce();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("keeps an action toast past a plain toast's lifetime until it is dismissed", () => {
    renderProvider();
    act(() => {
      toastBus.handler?.error("plain");
      toastBus.handler?.action("Session was not created", "Reopen", vi.fn());
    });
    act(() => vi.advanceTimersByTime(60_000));
    expect(screen.getAllByRole("alert").map((a) => a.textContent)).toEqual([
      expect.stringContaining("Session was not created"),
    ]);
    fireEvent.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("scrolls the column to a newly added toast, but not when one is dismissed", () => {
    renderProvider();
    const list = screen.getByTestId("toast-list");
    let top = 0;
    Object.defineProperty(list, "scrollHeight", { configurable: true, get: () => 500 });
    Object.defineProperty(list, "scrollTop", { configurable: true, get: () => top, set: (v: number) => (top = v) });
    const dismiss = (message: string) =>
      fireEvent.click(
        within(screen.getByText(message).closest("[role]") as HTMLElement).getByRole("button", { name: "Dismiss" }),
      );
    act(() => toastBus.handler?.action("first", "Reopen", vi.fn()));
    expect(top).toBe(500);
    top = 0;
    act(() => toastBus.handler?.action("second", "Reopen", vi.fn()));
    expect(top).toBe(500);
    // Removing the newest leaves an older one newest; nothing arrived.
    top = 120;
    dismiss("second");
    expect(top).toBe(120);
    act(() => toastBus.handler?.action("third", "Reopen", vi.fn()));
    expect(top).toBe(500);
    top = 120;
    dismiss("first");
    expect(top).toBe(120);
  });

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

  it("keeps the error styling of a clickable toast", () => {
    renderProvider();
    act(() => toastBus.handler?.openLink("Approve?", "/plugins/acme.kit/approvals?link=ab12", "error"));

    const toast = screen.getByText("Approve?").closest("div");
    expect(toast?.getAttribute("role")).toBe("alert");
    expect(toast?.className).toContain("cursor-pointer");
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
