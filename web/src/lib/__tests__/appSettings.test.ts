import { describe, it, expect, vi, beforeEach } from "vitest";
import { fetchActiveProfileSettings } from "../appSettings";
import * as api from "../api";

vi.mock("../api", () => ({
  fetchAbout: vi.fn(),
  fetchSettings: vi.fn(),
}));

describe("fetchActiveProfileSettings", () => {
  beforeEach(() => vi.clearAllMocks());

  it("reads the served profile's merged view, not the unprofiled or default one", async () => {
    // A shell reading the bare payload keeps showing the machine-wide value after
    // a profile override; `aoe --profile review serve` serves a non-default profile.
    vi.mocked(api.fetchAbout).mockResolvedValue({ profile: "review" } as never);
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: { show_diagnostics_pane: false },
    } as never);

    const settings = await fetchActiveProfileSettings();

    expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith("review");
    expect((settings as { session: { show_diagnostics_pane: boolean } }).session.show_diagnostics_pane).toBe(false);
  });

  it("falls back to the unprofiled view when the daemon names no profile", async () => {
    vi.mocked(api.fetchAbout).mockResolvedValue({ profile: "" } as never);
    vi.mocked(api.fetchSettings).mockResolvedValue({ session: {} } as never);

    await fetchActiveProfileSettings();

    expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith(undefined);
  });
});
