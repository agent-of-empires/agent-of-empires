// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";

import { DisconnectBanner } from "../DisconnectBanner";
import { useConnectionBanner } from "../../hooks/useConnectionBanner";
import { setServerDown } from "../../lib/connectionState";

const subscriptions = vi.hoisted(() => ({ active: 0 }));
vi.mock("../../lib/connectionState", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../../lib/connectionState")>();
  return {
    ...actual,
    onServerDownChange: (fn: (down: boolean) => void) => {
      subscriptions.active++;
      const unsubscribe = actual.onServerDownChange(fn);
      return () => {
        subscriptions.active--;
        return unsubscribe();
      };
    },
  };
});

beforeEach(() => {
  vi.useFakeTimers();
  setServerDown(false);
});
afterEach(() => {
  setServerDown(false);
  vi.useRealTimers();
});

function Banner() {
  const state = useConnectionBanner();
  return state && <DisconnectBanner state={state} />;
}

describe("DisconnectBanner", () => {
  it("alerts while the server is down, then flashes Reconnected and auto-dismisses after 3s", () => {
    const { container } = render(<Banner />);
    expect(container.firstChild).toBeNull();
    act(() => setServerDown(true));
    expect(screen.getByRole("alert").textContent).toContain("Server unreachable");
    act(() => setServerDown(false));

    const status = screen.getByRole("status");
    expect(status.textContent).toContain("Reconnected");

    act(() => vi.advanceTimersByTime(3000));
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("keeps the down alert when the server drops again mid-flash, then gives a full new flash", () => {
    render(<Banner />);
    act(() => setServerDown(true));
    act(() => setServerDown(false));
    expect(screen.getByRole("status")).toBeTruthy();

    act(() => vi.advanceTimersByTime(1000));
    act(() => setServerDown(true));
    expect(screen.getByRole("alert")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();

    // Past the first flash's original 3s deadline.
    act(() => vi.advanceTimersByTime(2500));
    expect(screen.getByRole("alert")).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();

    act(() => setServerDown(false));
    expect(screen.getByRole("status")).toBeTruthy();
    act(() => vi.advanceTimersByTime(2999));
    expect(screen.getByRole("status")).toBeTruthy();
    act(() => vi.advanceTimersByTime(1));
    expect(screen.queryByRole("status")).toBeNull();
  });

  it("releases the flash timer and the subscription on unmount mid-flash", () => {
    const { unmount } = render(<Banner />);
    expect(subscriptions.active).toBe(1);
    act(() => setServerDown(true));
    act(() => setServerDown(false));
    expect(screen.getByRole("status")).toBeTruthy();
    expect(vi.getTimerCount()).toBe(1);

    unmount();

    expect(vi.getTimerCount()).toBe(0);
    expect(subscriptions.active).toBe(0);
  });
});
