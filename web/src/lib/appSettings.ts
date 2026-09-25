import { fetchProfiles, fetchSettings, type SettingsResponse, type SettingsScope } from "./api";

/** The layer a user's own settings live in: the profile the server flags as
 *  default. That is the profile it serves, including under
 *  `aoe --profile <name> serve`, and the one the settings page edits.
 *  Machine-wide only when there is no profile at all. */
export async function activeSettingsScope(): Promise<SettingsScope> {
  const profile = (await fetchProfiles()).find((p) => p.is_default)?.name;
  return profile ? { profile } : "machine";
}

/** Settings as the user configured them, for any feature that honors one. */
export async function fetchActiveSettings(): Promise<SettingsResponse | null> {
  return fetchSettings(await activeSettingsScope());
}
