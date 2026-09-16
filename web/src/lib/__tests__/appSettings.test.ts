import { describe, it, expect, vi, beforeEach } from "vitest";
import { fetchActiveProfileSettings } from "../appSettings";
import * as api from "../api";

vi.mock("../api", () => ({
  fetchAbout: vi.fn(),
  fetchSettings: vi.fn(),
}));

describe("fetchActiveProfileSettings", () => {
  beforeEach(() => vi.clearAllMocks());

  it("reads the served profile's merged view, not the unprofiled payload", async () => {
    // The regression this pins: the settings page writes a profile override, so
    // a shell reading the bare payload keeps showing the machine-wide value
    // after the user has changed it.
    vi.mocked(api.fetchAbout).mockResolvedValue({ profile: "main" } as never);
    vi.mocked(api.fetchSettings).mockResolvedValue({
      session: { show_diagnostics_pane: false },
    } as never);

    const settings = await fetchActiveProfileSettings();

    expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith("main");
    expect((settings as { session: { show_diagnostics_pane: boolean } }).session.show_diagnostics_pane).toBe(false);
  });

  it("reads the profile the daemon serves, which need not be the default one", async () => {
    // `aoe --profile <name> serve` serves a named profile; reading whichever
    // profile is flagged default would show settings from a different one.
    vi.mocked(api.fetchAbout).mockResolvedValue({ profile: "review" } as never);
    vi.mocked(api.fetchSettings).mockResolvedValue({ session: {} } as never);

    await fetchActiveProfileSettings();

    expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith("review");
  });

  it("falls back to the unprofiled view when the daemon names no profile", async () => {
    vi.mocked(api.fetchAbout).mockResolvedValue({ profile: "" } as never);
    vi.mocked(api.fetchSettings).mockResolvedValue({ session: {} } as never);

    await fetchActiveProfileSettings();

    expect(vi.mocked(api.fetchSettings)).toHaveBeenCalledWith(undefined);
  });
});
