import type { CommandActionGroup } from "./types";

export const GROUP_ORDER: CommandActionGroup[] = ["Actions", "Sessions", "Conversations", "Settings"];

// "All" scopes to every group (the default flat view); the rest scope to a
// single CommandActionGroup. Tab order is "All" first, then GROUP_ORDER.
export type PaletteTab = "All" | CommandActionGroup;

export const TAB_ORDER: PaletteTab[] = ["All", ...GROUP_ORDER];
