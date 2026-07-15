import { describe, expect, it } from "vitest";
import { getClientCapabilities } from "../clientCapabilities";
import type { ServerAbout } from "../api";

// #7: getClientCapabilities maps serverAbout.cityhall_mode to the named UI
// gates. Both branches matter: CityHall locks everything down, normal mode
// leaves it open, and a missing/loading serverAbout must default to open.
describe("getClientCapabilities", () => {
  it("locks down every affordance in CityHall mode", () => {
    const caps = getClientCapabilities({ cityhall_mode: true } as ServerAbout);
    expect(caps).toEqual({
      cityhall: true,
      canUseTerminal: false,
      canUseDiff: false,
      canManageProjects: false,
      nameOnlyWizard: true,
    });
  });

  it("leaves everything open in normal mode", () => {
    const caps = getClientCapabilities({ cityhall_mode: false } as ServerAbout);
    expect(caps).toEqual({
      cityhall: false,
      canUseTerminal: true,
      canUseDiff: true,
      canManageProjects: true,
      nameOnlyWizard: false,
    });
  });

  it("defaults to open when serverAbout is absent", () => {
    for (const about of [null, undefined]) {
      const caps = getClientCapabilities(about);
      expect(caps.cityhall).toBe(false);
      expect(caps.canManageProjects).toBe(true);
      expect(caps.nameOnlyWizard).toBe(false);
    }
  });
});
