import { describe, it, expect, vi, beforeEach } from "vitest";
import { activeSettingsScope, fetchActiveSettings } from "../appSettings";
import * as api from "../api";

vi.mock("../api", () => ({
  fetchProfiles: vi.fn(),
  fetchSettings: vi.fn(),
}));

describe("fetchActiveSettings", () => {
  beforeEach(() => vi.clearAllMocks());

  it("reads the profile the settings page edits", async () => {
    // A plain `aoe serve` names no profile, but the settings page still writes
    // to the one flagged default; reading the bare payload hid a saved toggle.
    const cases: { profiles: { name: string; is_default: boolean }[]; expected: api.SettingsScope }[] = [
      { profiles: [{ name: "main", is_default: true }], expected: { profile: "main" } },
      {
        profiles: [
          { name: "main", is_default: false },
          { name: "review", is_default: true },
        ],
        expected: { profile: "review" },
      },
      { profiles: [], expected: "machine" },
    ];
    for (const { profiles, expected } of cases) {
      vi.mocked(api.fetchProfiles).mockResolvedValue(profiles as never);
      vi.mocked(api.fetchSettings).mockResolvedValueOnce({ session: {} } as never);

      expect(await activeSettingsScope()).toEqual(expected);
      expect(await fetchActiveSettings()).toEqual({ session: {} });
      expect(vi.mocked(api.fetchSettings)).toHaveBeenLastCalledWith(expected);
    }
  });
});
