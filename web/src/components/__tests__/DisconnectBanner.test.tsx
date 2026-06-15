// @vitest-environment jsdom
//
// Coverage for DisconnectBanner: hidden when connected, an alert while the
// server is down, and a transient "Reconnected" status that auto-dismisses
// after the connection recovers. Drives state through the real
// connectionState pub/sub rather than mocking it.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, render, screen } from "@testing-library/react";

import { DisconnectBanner } from "../DisconnectBanner";
import { setServerDown } from "../../lib/connectionState";

beforeEach(() => {
  vi.useFakeTimers();
  setServerDown(false);
});
afterEach(() => {
  cleanup();
  setServerDown(false);
  vi.useRealTimers();
});

describe("DisconnectBanner", () => {
  it("renders nothing while connected", () => {
    const { container } = render(<DisconnectBanner />);
    expect(container.firstChild).toBeNull();
  });

  it("shows an alert when the server goes down", () => {
    render(<DisconnectBanner />);
    act(() => setServerDown(true));
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("Server unreachable");
  });

  it("flashes a Reconnected status then auto-dismisses after 3s", () => {
    render(<DisconnectBanner />);
    act(() => setServerDown(true));
    act(() => setServerDown(false));

    const status = screen.getByRole("status");
    expect(status.textContent).toContain("Reconnected");

    act(() => vi.advanceTimersByTime(3000));
    expect(screen.queryByRole("status")).toBeNull();
  });
});
