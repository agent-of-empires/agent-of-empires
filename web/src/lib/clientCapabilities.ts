import type { ServerAbout } from "./api";

/** Named capabilities for CityHall client mode, which the server also enforces; these are UX only. */
export interface ClientCapabilities {
  cityhall: boolean;
  canUseTerminal: boolean;
  canUseDiff: boolean;
  canManageProjects: boolean;
  nameOnlyWizard: boolean;
}

export function getClientCapabilities(serverAbout: ServerAbout | null | undefined): ClientCapabilities {
  const cityhall = serverAbout?.cityhall_mode ?? false;
  return {
    cityhall,
    canUseTerminal: !cityhall,
    canUseDiff: !cityhall,
    canManageProjects: !cityhall,
    nameOnlyWizard: cityhall,
  };
}
