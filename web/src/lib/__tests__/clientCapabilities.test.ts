import { expect, it } from "vitest";
import { getClientCapabilities } from "../clientCapabilities";
import type { ServerAbout } from "../api";

it("locks down every affordance only in CityHall mode", () => {
  const open = {
    cityhall: false,
    canUseTerminal: true,
    canUseDiff: true,
    canManageProjects: true,
    nameOnlyWizard: false,
  };
  expect(getClientCapabilities({ cityhall_mode: true } as ServerAbout)).toEqual({
    cityhall: true,
    canUseTerminal: false,
    canUseDiff: false,
    canManageProjects: false,
    nameOnlyWizard: true,
  });
  for (const about of [{ cityhall_mode: false } as ServerAbout, null, undefined]) {
    expect(getClientCapabilities(about), String(about?.cityhall_mode)).toEqual(open);
  }
});
