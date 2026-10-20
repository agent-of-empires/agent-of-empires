import { fetchSettings } from "./api";

/** After a theme write with an unknown outcome: true only if a fetch shows the value saved. */
export async function confirmThemeWrite(field: string, value: unknown): Promise<boolean> {
  const theme = (await fetchSettings())?.theme as Record<string, unknown> | undefined;
  return theme?.[field] === value;
}
