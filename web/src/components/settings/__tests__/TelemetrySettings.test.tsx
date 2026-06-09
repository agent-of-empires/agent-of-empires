// @vitest-environment jsdom
//
// Contract test for the TelemetrySettings panel. Unlike the other settings
// panels it talks to the dedicated telemetry endpoints directly (the daemon
// owns the install id; the browser never posts to the telemetry backend), so
// this mocks the api module and asserts the toggle calls setTelemetryConsent
// with the right value, and that DO_NOT_TRACK forces the toggle off.

import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";

import type { TelemetryStatus } from "../../../lib/api";

const fetchTelemetryStatus = vi.fn<[], Promise<TelemetryStatus | null>>();
const setTelemetryConsent = vi.fn<[boolean], Promise<TelemetryStatus | null>>();

vi.mock("../../../lib/api", () => ({
  fetchTelemetryStatus: () => fetchTelemetryStatus(),
  setTelemetryConsent: (enabled: boolean) => setTelemetryConsent(enabled),
}));

// Imported after the mock is registered.
import { TelemetrySettings } from "../TelemetrySettings";

function status(overrides: Partial<TelemetryStatus> = {}): TelemetryStatus {
  return { enabled: false, responded: true, do_not_track: false, ...overrides };
}

beforeEach(() => {
  fetchTelemetryStatus.mockReset();
  setTelemetryConsent.mockReset();
  setTelemetryConsent.mockResolvedValue(status({ enabled: true }));
});

describe("TelemetrySettings contract", () => {
  it("toggling on calls setTelemetryConsent(true)", async () => {
    fetchTelemetryStatus.mockResolvedValue(status({ enabled: false }));
    const { container } = render(<TelemetrySettings />);
    await waitFor(() => {
      expect(fetchTelemetryStatus).toHaveBeenCalled();
    });

    const toggle = container.querySelector("button[role=switch]") as HTMLButtonElement;
    fireEvent.click(toggle);
    expect(setTelemetryConsent).toHaveBeenCalledWith(true);
  });

  it("toggling off calls setTelemetryConsent(false)", async () => {
    fetchTelemetryStatus.mockResolvedValue(status({ enabled: true }));
    const { container } = render(<TelemetrySettings />);
    await waitFor(() => {
      const t = container.querySelector("button[role=switch]") as HTMLButtonElement | null;
      expect(t?.getAttribute("aria-checked")).toBe("true");
    });

    const toggle = container.querySelector("button[role=switch]") as HTMLButtonElement;
    fireEvent.click(toggle);
    expect(setTelemetryConsent).toHaveBeenCalledWith(false);
  });

  it("DO_NOT_TRACK forces the toggle off and shows a note; clicking is a no-op", async () => {
    fetchTelemetryStatus.mockResolvedValue(status({ enabled: true, do_not_track: true }));
    const { container, findByText } = render(<TelemetrySettings />);
    await findByText(/DO_NOT_TRACK is set/i);

    const toggle = container.querySelector("button[role=switch]") as HTMLButtonElement;
    // Forced off despite enabled=true in config.
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);
    expect(setTelemetryConsent).not.toHaveBeenCalled();
  });
});
