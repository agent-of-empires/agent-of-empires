import type { ServerAbout } from "./api";

/** Named capabilities for CityHall client mode and read-only servers, which the server also enforces; these are UX only. */
export interface ClientCapabilities {
  cityhall: boolean;
  /**
   * Hide write actions. Also true when /api/about hasn't answered: a read-only server would
   * refuse them, and a writable one shows them once /api/about answers.
   */
  readOnly: boolean;
  canUseTerminal: boolean;
  canUseDiff: boolean;
  canManageProjects: boolean;
  nameOnlyWizard: boolean;
}

export function getClientCapabilities(serverAbout: ServerAbout | null | undefined): ClientCapabilities {
  const cityhall = serverAbout?.cityhall_mode ?? false;
  return {
    cityhall,
    readOnly: serverAbout ? !!serverAbout.read_only : true,
    canUseTerminal: !cityhall,
    canUseDiff: !cityhall,
    canManageProjects: !cityhall,
    nameOnlyWizard: cityhall,
  };
}
