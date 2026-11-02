import type { ServerAbout } from "./api";

/** The Settings row for the Session profile's auto-resume toggle, or null where this
 *  dashboard can't edit it: read-only, or CityHall, which has no Structured view tab. */
export function autoResumeSettingsHref(profile: string, about: ServerAbout | null | undefined): string | null {
  if (about?.read_only || about?.cityhall_mode) return null;
  return `/settings/structured-view?profile=${encodeURIComponent(profile)}&field=acp.rate_limit_auto_resume`;
}
