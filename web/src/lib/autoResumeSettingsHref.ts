import type { ServerAbout } from "./api";

/** The Settings row for the Session profile's auto-resume toggle, or null where this
 *  dashboard can't edit it: read-only, CityHall (no Structured view tab), or before
 *  /api/about has answered, when it can't tell. */
export function autoResumeSettingsHref(profile: string, about: ServerAbout | null | undefined): string | null {
  if (!about || about.read_only || about.cityhall_mode) return null;
  return `/settings/structured-view?profile=${encodeURIComponent(profile)}&field=acp.rate_limit_auto_resume`;
}
