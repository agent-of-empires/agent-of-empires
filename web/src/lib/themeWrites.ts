import { fetchSettings } from "./api";

// The theme is global, so the latest write of a field is tracked outside any profile or component.
const latest = new Map<string, number>();

export function claimThemeWrite(field: string): number {
  const claim = (latest.get(field) ?? 0) + 1;
  latest.set(field, claim);
  return claim;
}

/** False once a newer pick of the field, from any profile or remount, has superseded this one. */
export const isLatestThemeWrite = (field: string, claim: number) => latest.get(field) === claim;

/** After a theme write with an unknown outcome: true only if a fetch shows the value saved. */
export async function confirmThemeWrite(field: string, value: unknown): Promise<boolean> {
  const theme = (await fetchSettings())?.theme as Record<string, unknown> | undefined;
  return theme?.[field] === value;
}
