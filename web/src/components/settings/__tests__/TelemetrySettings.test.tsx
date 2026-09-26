// @vitest-environment jsdom

import { describe, expect, it, vi, beforeEach } from "vitest";
import { fireEvent, render, waitFor } from "@testing-library/react";

import type { TelemetryStatus } from "../../../lib/api";

const fetchTelemetryStatus = vi.fn<[], Promise<TelemetryStatus | null>>();
const setTelemetryConsent = vi.fn<[boolean], Promise<TelemetryStatus | null>>();

vi.mock("../../../lib/api", () => ({
  fetchTelemetryStatus: () => fetchTelemetryStatus(),
  setTelemetryConsent: (enabled: boolean) => setTelemetryConsent(enabled),
}));

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
  it("toggling sends the opposite consent", async () => {
    const enabled = false;
    fetchTelemetryStatus.mockResolvedValue(status({ enabled }));
    const { container } = render(<TelemetrySettings />);
    const toggle = () => container.querySelector("button[role=switch]") as HTMLButtonElement;
    await waitFor(() => expect(toggle()?.getAttribute("aria-checked")).toBe(String(enabled)));
    fireEvent.click(toggle());
    expect(setTelemetryConsent).toHaveBeenCalledWith(!enabled);
  });

  it("DO_NOT_TRACK forces the toggle off and shows a note; clicking is a no-op", async () => {
    fetchTelemetryStatus.mockResolvedValue(status({ enabled: true, do_not_track: true }));
    const { container, findByText } = render(<TelemetrySettings />);
    await findByText(/DO_NOT_TRACK is set/i);

    const toggle = container.querySelector("button[role=switch]") as HTMLButtonElement;
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);
    expect(setTelemetryConsent).not.toHaveBeenCalled();
  });
});
