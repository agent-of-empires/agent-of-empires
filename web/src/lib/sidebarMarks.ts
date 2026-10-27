import type { StatusMeaning } from "./statusLegend";

/** What the sidebar's non-status marks mean, shared by the legend and each mark's tooltip. */
export const MARK_MEANING = {
  unread: { label: "Unread", description: "activity you haven't opened yet; replaces the status glyph" },
  pinned: { label: "Pinned Session", description: "its title is bold too" },
  favorite: { label: "Favorite", description: "a Session you marked as a favorite" },
  // The ◆ wording is the Project marker's own; the legend repeats it rather than redefining it.
  pinnedProject: { label: "Pinned project", description: "persists without sessions" },
  groupDot: {
    label: "Group dot",
    description: "green if any Session is Running, Waiting, Starting or just finished; grey otherwise",
  },
} satisfies Record<string, StatusMeaning>;

/** The Group dot's tooltip describes the state, not a count. */
export const GROUP_DOT_STATE = {
  active: "Active: a Session is Running, Waiting, Starting or just finished",
  inactive: "Inactive: no Session is Running, Waiting, Starting or just finished",
};

/** "Label: description", the tooltip form of a meaning. */
export function meaningText({ label, description }: StatusMeaning): string {
  return `${label}: ${description}`;
}
