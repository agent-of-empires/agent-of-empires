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

  it("keeps an action toast usable when its action returns false", () => {
    renderProvider();
    const reopen = vi.fn().mockReturnValueOnce(false).mockReturnValueOnce(true);
    act(() => toastBus.handler?.action("Session was not created", "Reopen", reopen));
    fireEvent.click(screen.getByRole("button", { name: "Reopen" }));
    expect(screen.getByRole("alert").textContent).toContain("Session was not created");
    fireEvent.click(screen.getByRole("button", { name: "Reopen" }));
    expect(reopen).toHaveBeenCalledTimes(2);
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

// #5344: a linked toast opened only through the outer div's pointer handler.
describe("linked toasts from the keyboard", () => {
  it("open through a focusable button once, before Dismiss in tab order; Dismiss never navigates", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onOpen = vi.fn();
    window.addEventListener(OPEN_SESSION_EVENT, onOpen);
    renderProvider();
    // (how the toast arrives, its message, what opening it must do)
    const cases: [() => void, string, () => void][] = [
      [
        () => dispatchPush({ type: "aoe-push", payload: { title: "Done", session_id: "sess-1" } }),
        "Done",
        () => expect((onOpen.mock.calls.at(-1)![0] as CustomEvent).detail).toEqual({ sessionId: "sess-1" }),
      ],
      [
        () => act(() => toastBus.handler?.openLink("Review PR", "https://example.com/pr/1")),
        "Review PR",
        () => expect(open).toHaveBeenLastCalledWith("https://example.com/pr/1", "_blank", "noopener,noreferrer"),
      ],
    ];
    for (const [arrive, message, opened] of cases) {
      const before = onOpen.mock.calls.length + open.mock.calls.length;
      arrive();
      const toast = screen.getByText(message).closest("[role]") as HTMLElement;
      const buttons = within(toast).getAllByRole("button");
      const opener = within(toast).getByRole("button", { name: message });
      expect(buttons.indexOf(opener), message).toBe(0);
      expect(buttons.at(-1)?.getAttribute("aria-label"), message).toBe("Dismiss");
      opener.focus();
      expect(document.activeElement, message).toBe(opener);
      // Enter or Space on a native button is a click.
      act(() => fireEvent.click(opener));
      expect(onOpen.mock.calls.length + open.mock.calls.length - before, message).toBe(1);
      opened();
      expect(screen.queryByText(message), message).toBeNull();

      arrive();
      const again = screen.getByText(message).closest("[role]") as HTMLElement;
      act(() => fireEvent.click(within(again).getByRole("button", { name: "Dismiss" })));
      expect(onOpen.mock.calls.length + open.mock.calls.length - before, `${message}: Dismiss`).toBe(1);
      expect(screen.queryByText(message), message).toBeNull();
    }
    window.removeEventListener(OPEN_SESSION_EVENT, onOpen);
    open.mockRestore();
  });

  it("names a blank message's opener after its target and opens it once", () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const onOpen = vi.fn();
    window.addEventListener(OPEN_SESSION_EVENT, onOpen);
    renderProvider();
    // (how the blank toast arrives, the opener's name, what opening it must do)
    const cases: [() => void, string, () => void][] = [
      [
        () => act(() => toastBus.handler?.openLink("   ", "https://example.com/pr/1")),
        "Open link",
        () => expect(open).toHaveBeenLastCalledWith("https://example.com/pr/1", "_blank", "noopener,noreferrer"),
      ],
      [
        () => dispatchPush({ type: "aoe-push", payload: { title: "  ", session_id: "sess-1" } }),
        "Open session",
        () => expect((onOpen.mock.calls.at(-1)![0] as CustomEvent).detail).toEqual({ sessionId: "sess-1" }),
      ],
    ];
    for (const [arrive, name, opened] of cases) {
      const before = onOpen.mock.calls.length + open.mock.calls.length;
      arrive();
      const opener = screen.getByRole("button", { name });
      const toast = opener.closest("[role]") as HTMLElement;
      expect(within(toast).getAllByRole("button").indexOf(opener), name).toBe(0);
      act(() => fireEvent.click(opener));
      expect(onOpen.mock.calls.length + open.mock.calls.length - before, name).toBe(1);
      opened();
      expect(screen.queryByRole("button", { name }), name).toBeNull();
    }
    window.removeEventListener(OPEN_SESSION_EVENT, onOpen);
    open.mockRestore();
  });
});

describe("timed toast lifetime", () => {
  const LIFETIME = 6000;
  const GRACE = 2000;

  const toastFor = (message: string) => screen.getByText(message).closest("[role]") as HTMLElement;
  const dismissOf = (message: string) => within(toastFor(message)).getByRole("button", { name: "Dismiss" });
  const advance = (ms: number) => act(() => void vi.advanceTimersByTime(ms));
  const present = (message: string) => screen.queryByText(message) !== null;

  it.each([
    ["plain", () => act(() => toastBus.handler?.info("m"))],
    ["session", () => dispatchPush({ type: "aoe-push", payload: { title: "m", session_id: "s1" } })],
    ["link", () => act(() => toastBus.handler?.openLink("m", "https://example.com"))],
  ])("expires a %s toast after its lifetime", (_name, push) => {
    renderProvider();
    push();
    advance(LIFETIME - 1);
    expect(present("m")).toBe(true);
    advance(1);
    expect(present("m")).toBe(false);
  });

  it("pauses while hovered and resumes with a grace period", () => {
    renderProvider();
    act(() => toastBus.handler?.info("hover"));
    advance(5000);
    fireEvent.pointerEnter(toastFor("hover"));
    advance(60_000);
    expect(present("hover")).toBe(true);
    fireEvent.pointerLeave(toastFor("hover"));
    advance(GRACE - 1);
    expect(present("hover")).toBe(true);
    advance(1);
    expect(present("hover")).toBe(false);
  });

  it("resumes with the remaining time when it exceeds the grace", () => {
    renderProvider();
    act(() => toastBus.handler?.info("early"));
    advance(1000);
    fireEvent.pointerEnter(toastFor("early"));
    advance(30_000);
    fireEvent.pointerLeave(toastFor("early"));
    advance(4999);
    expect(present("early")).toBe(true);
    advance(1);
    expect(present("early")).toBe(false);
  });

  it("pauses while focused", () => {
    renderProvider();
    act(() => toastBus.handler?.openLink("focus", "https://example.com"));
    advance(5000);
    fireEvent.focusIn(dismissOf("focus"));
    advance(60_000);
    expect(present("focus")).toBe(true);
    fireEvent.focusOut(dismissOf("focus"), { relatedTarget: document.body });
    advance(GRACE - 1);
    expect(present("focus")).toBe(true);
    advance(1);
    expect(present("focus")).toBe(false);
  });

  it("keeps the pause when focus moves within the toast", () => {
    renderProvider();
    act(() => toastBus.handler?.openLink("focus", "https://example.com"));
    advance(5000);
    const dismiss = dismissOf("focus");
    fireEvent.focusIn(dismiss);
    fireEvent.focusOut(dismiss, { relatedTarget: within(toastFor("focus")).getByRole("button", { name: "focus" }) });
    advance(60_000);
    expect(present("focus")).toBe(true);
    fireEvent.focusOut(dismiss, { relatedTarget: document.body });
    advance(GRACE - 1);
    expect(present("focus")).toBe(true);
    advance(1);
    expect(present("focus")).toBe(false);
  });

  it("needs both hover and focus released", () => {
    renderProvider();
    act(() => toastBus.handler?.info("both"));
    advance(5000);
    fireEvent.pointerEnter(toastFor("both"));
    fireEvent.focusIn(dismissOf("both"));
    fireEvent.pointerLeave(toastFor("both"));
    advance(60_000);
    expect(present("both")).toBe(true);
    fireEvent.focusOut(dismissOf("both"), { relatedTarget: document.body });
    advance(GRACE - 1);
    expect(present("both")).toBe(true);
    advance(1);
    expect(present("both")).toBe(false);
  });

  it("does not subtract elapsed time twice on repeated pauses", () => {
    renderProvider();
    act(() => toastBus.handler?.info("twice"));
    advance(5000);
    fireEvent.pointerEnter(toastFor("twice"));
    fireEvent.focusIn(dismissOf("twice"));
    advance(3000);
    fireEvent.pointerLeave(toastFor("twice"));
    fireEvent.focusOut(dismissOf("twice"), { relatedTarget: document.body });
    advance(GRACE - 1);
    expect(present("twice")).toBe(true);
    advance(1);
    expect(present("twice")).toBe(false);
  });

  it("keeps a separate timer for each toast", () => {
    renderProvider();
    act(() => toastBus.handler?.info("A"));
    advance(1000);
    act(() => toastBus.handler?.info("B"));
    fireEvent.pointerEnter(toastFor("A"));
    advance(LIFETIME - 1);
    expect(present("B")).toBe(true);
    advance(1);
    expect(present("B")).toBe(false);
    expect(present("A")).toBe(true);
    advance(60_000);
    expect(present("A")).toBe(true);
    fireEvent.pointerLeave(toastFor("A"));
    advance(4999);
    expect(present("A")).toBe(true);
    advance(1);
    expect(present("A")).toBe(false);
  });

  it("clears the timer when a toast is dismissed", () => {
    renderProvider();
    act(() => toastBus.handler?.info("x"));
    fireEvent.pointerEnter(toastFor("x"));
    fireEvent.click(dismissOf("x"));
    expect(present("x")).toBe(false);
    expect(vi.getTimerCount()).toBe(0);

    act(() => toastBus.handler?.info("z"));
    fireEvent.click(dismissOf("z"));
    expect(present("z")).toBe(false);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("clears timers on unmount", () => {
    const { unmount } = renderProvider();
    act(() => toastBus.handler?.info("y"));
    unmount();
    expect(vi.getTimerCount()).toBe(0);
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
