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
  expect(getClientCapabilities({ cityhall_mode: true } as ServerAbout)).toMatchObject({
    cityhall: true,
    canUseTerminal: false,
    canUseDiff: false,
    canManageProjects: false,
    nameOnlyWizard: true,
  });
  for (const about of [{ cityhall_mode: false } as ServerAbout, null, undefined]) {
    expect(getClientCapabilities(about), String(about?.cityhall_mode)).toMatchObject(open);
  }
});

it("hides write actions unless the server says it is writable", () => {
  // (/api/about answer, read-only)
  const cases: [ServerAbout | null | undefined, boolean][] = [
    [{ read_only: false } as ServerAbout, false],
    [{ read_only: true } as ServerAbout, true],
    // Any answer is followed; the Daemon always sends the flag.
    [{} as ServerAbout, false],
    // A failed or missing /api/about: the server may be read-only, so fail safe.
    [null, true],
    [undefined, true],
  ];
  for (const [about, readOnly] of cases) {
    expect(getClientCapabilities(about).readOnly, JSON.stringify(about)).toBe(readOnly);
  }
});
