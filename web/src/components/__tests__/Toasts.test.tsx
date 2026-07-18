// @vitest-environment jsdom
//
// Covers ToastProvider + ToastBusBridge: rendering info/error variants,
// the auto-dismiss timer, manual dismiss, the empty state, the
// service-worker push -> in-app toast path, and the clickable
// session-jump toast.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, render, screen } from "@testing-library/react";
import { ToastBusBridge, ToastProvider } from "../Toasts";
import { toastBus } from "../../lib/toastBus";
import { OPEN_SESSION_EVENT } from "../../lib/sessionRoute";

// jsdom ships no navigator.serviceWorker; the ToastProvider effect bails
// out without one. Install a real EventTarget once at module load so the
// SW push -> in-app toast path is exercised. It must stay installed across
// the global RTL cleanup (test-setup.ts) because React's passive unmount
// calls navigator.serviceWorker.removeEventListener.
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

describe("ToastProvider rendering", () => {
  it("renders nothing in the toast region when there are no toasts", () => {
    const { container } = renderProvider();
    const region = container.querySelector("div.fixed");
    expect(region).not.toBeNull();
    expect(region?.children.length).toBe(0);
    expect(screen.queryByRole("status")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("renders an info toast with role=status", () => {
    renderProvider();
    act(() => toastBus.handler?.info("hello info"));
    const toast = screen.getByRole("status");
    expect(toast.textContent).toContain("hello info");
  });

  it("renders an error toast with role=alert and error styling", () => {
    renderProvider();
    act(() => toastBus.handler?.error("boom"));
    const toast = screen.getByRole("alert");
    expect(toast.textContent).toContain("boom");
    expect(toast.className).toContain("status-error");
  });

  it("renders a default-kind (info) toast via push()", () => {
    renderProvider();
    act(() => toastBus.handler?.push("plain push"));
    expect(screen.getByRole("status").textContent).toContain("plain push");
  });

  it("renders multiple stacked toasts each with a dismiss button", () => {
    renderProvider();
    act(() => {
      toastBus.handler?.info("first");
      toastBus.handler?.error("second");
    });
    expect(screen.getByText("first")).toBeTruthy();
    expect(screen.getByText("second")).toBeTruthy();
    expect(screen.getAllByRole("button", { name: "Dismiss" }).length).toBe(2);
  });
});

describe("auto-dismiss timer", () => {
  it("removes a toast after its 6s lifetime elapses", () => {
    renderProvider();
    act(() => toastBus.handler?.info("temporary"));
    expect(screen.queryByText("temporary")).toBeTruthy();

    act(() => vi.advanceTimersByTime(6000));
    expect(screen.queryByText("temporary")).toBeNull();
  });

  it("keeps the toast visible before the lifetime elapses", () => {
    renderProvider();
    act(() => toastBus.handler?.info("still here"));
    act(() => vi.advanceTimersByTime(5999));
    expect(screen.queryByText("still here")).toBeTruthy();
  });
});

describe("manual dismiss", () => {
  it("removes the toast when its dismiss button is clicked", () => {
    renderProvider();
    act(() => toastBus.handler?.error("dismiss me"));
    const btn = screen.getByRole("button", { name: "Dismiss" });

    act(() => fireEvent.click(btn));
    expect(screen.queryByText("dismiss me")).toBeNull();
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

  it("falls back to the title when the push payload has no body", () => {
    renderProvider();
    dispatchPush({ type: "aoe-push", payload: { title: "Heads up", session_id: "s2" } });
    expect(screen.getByText("Heads up")).toBeTruthy();
  });

  it("uses the default title and a plain info toast when payload omits title and session", () => {
    renderProvider();
    dispatchPush({ type: "aoe-push", payload: { body: "just a body" } });
    const toast = screen.getByText("Agent of Empires: just a body").closest("div");
    expect(toast?.getAttribute("role")).toBe("status");
    expect(toast?.className).not.toContain("cursor-pointer");
  });

  it("ignores messages that are not aoe-push", () => {
    renderProvider();
    dispatchPush({ type: "something-else", payload: { title: "nope" } });
    dispatchPush(null);
    dispatchPush({ type: "aoe-push" });
    expect(screen.queryByRole("status")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
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
  it("wires and unwires the module-level toastBus handler", () => {
    const { unmount } = renderProvider();
    expect(toastBus.handler).not.toBeNull();
    unmount();
    expect(toastBus.handler).toBeNull();
  });

  it("renders as a no-op without a surrounding provider", () => {
    // Outside ToastProvider the context is null; the bridge must not touch
    // the module-level bus.
    render(<ToastBusBridge />);
    expect(toastBus.handler).toBeNull();
  });
});
