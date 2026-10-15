import type { CommandActionGroup } from "./types";

export const GROUP_ORDER: CommandActionGroup[] = ["Actions", "Sessions", "Conversations", "Settings"];

export type PaletteTab = "All" | CommandActionGroup;

export const TAB_ORDER: PaletteTab[] = ["All", ...GROUP_ORDER];
