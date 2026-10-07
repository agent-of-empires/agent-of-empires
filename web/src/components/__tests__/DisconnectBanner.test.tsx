// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";

import { DisconnectBanner } from "../DisconnectBanner";
import { setServerDown } from "../../lib/connectionState";

beforeEach(() => {
  vi.useFakeTimers();
  setServerDown(false);
});
afterEach(() => {
  setServerDown(false);
  vi.useRealTimers();
});

describe("DisconnectBanner", () => {
  it("alerts while the server is down, then flashes Reconnected and auto-dismisses after 3s", () => {
    const { container } = render(<DisconnectBanner />);
    expect(container.firstChild).toBeNull();
    act(() => setServerDown(true));
    expect(screen.getByRole("alert").textContent).toContain("Server unreachable");
    act(() => setServerDown(false));

    const status = screen.getByRole("status");
    expect(status.textContent).toContain("Reconnected");

    act(() => vi.advanceTimersByTime(3000));
    expect(screen.queryByRole("status")).toBeNull();
  });
});
