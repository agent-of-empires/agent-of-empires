// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";

const settleSettingsIntrosReadOnly = vi.fn();
vi.mock("../../../lib/settingsIntros", () => ({
  settleSettingsIntrosReadOnly: (readOnly: boolean) => settleSettingsIntrosReadOnly(readOnly),
}));
vi.mock("../../../lib/api", () => ({
  fetchAbout: () => Promise.resolve({ read_only: true }),
  fetchTelemetryStatus: () => Promise.resolve(null),
  reportTelemetrySeen: vi.fn(),
  setTelemetryConsent: vi.fn(),
}));

import { useServerAbout } from "../useServerAbout";

describe("useServerAbout", () => {
  it("tells the Settings intros whether the Daemon is read-only once /api/about answers", async () => {
    const { result } = renderHook(() => useServerAbout());
    await waitFor(() => expect(result.current.serverAboutLoaded).toBe(true));
    expect(settleSettingsIntrosReadOnly).toHaveBeenCalledExactlyOnceWith(true);
  });
});
