import { fetchAbout, fetchSettings, type SettingsResponse } from "./api";

/** The settings the app shell reads, taken from the profile the daemon is
 *  actually serving rather than the unprofiled payload.
 *
 *  The settings page writes a profile override, so a shell that reads the bare
 *  payload shows the value the user did not choose: the toggle saves, the
 *  surface it drives keeps the machine-wide value, and the two disagree with no
 *  way to tell from the screen.
 *
 *  The profile comes from the daemon rather than from whichever profile is
 *  flagged default, because `aoe --profile <name> serve` serves a named profile
 *  that need not be the default one, and reading the wrong one reintroduces
 *  exactly that disagreement. */
export async function fetchActiveProfileSettings(): Promise<SettingsResponse | null> {
  const about = await fetchAbout();
  return fetchSettings(about?.profile || undefined);
}
