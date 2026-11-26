// The redesign's shared surfaces. Every panel, menu, sheet, dialog and banner draws from these rather than
// restyling its own, so a change here moves the whole app together. Elevation comes from the theme's `shadow-*`
// tokens (index.css); `float-control` and `glass-menu` are utilities there for a phone's floating controls and
// menus. Phones get roomier rows and 16px inputs (below which iOS zooms a focused field) through `max-md:`.

/** A group's caption above its card, as the session list and Settings caption theirs. */
export const CAPTION = "block px-1 mb-2 text-[12px] font-semibold uppercase tracking-[0.06em] text-text-dim";

/** A raised card holding a group of rows, or a panel's own content. */
export const CARD =
  "rounded-xl max-md:rounded-2xl border border-surface-700/50 bg-surface-850 max-md:bg-surface-800 shadow-md px-4";

/** A text input, textarea or select. */
export const INPUT =
  "w-full bg-surface-900 border border-surface-700/70 rounded-lg px-3 py-2 text-[16px] md:text-sm text-text-primary placeholder:text-text-dim focus:border-brand-600 focus:outline-none disabled:opacity-50";

/** A floating menu or popover: dropdowns, context menus, pickers. A phone's sheet variant adds SHEET. */
export const MENU = "rounded-xl border border-surface-700/60 bg-surface-800 shadow-xl py-1";

/** One row of a menu; roomier on a phone, where it is a tap target. */
export const MENU_ITEM =
  "w-full flex items-center gap-2.5 px-3 py-1.5 max-md:py-3 text-left text-[13px] max-md:text-[15px] text-text-secondary hover:bg-surface-700/60 hover:text-text-primary focus-visible:bg-surface-700/60 focus-visible:outline-none cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-not-allowed";

/** A menu row that removes or stops something. */
export const MENU_ITEM_DANGER = MENU_ITEM.replace("text-text-secondary", "text-status-error").replace(
  "hover:text-text-primary",
  "hover:text-status-error",
);

/** A rule between a menu's groups. */
export const MENU_SEPARATOR = "my-1 mx-3 border-t border-surface-700/50";

/** A small heading over a menu group. */
export const MENU_HEADING = "px-3 pt-2 pb-1 text-[11px] font-semibold uppercase tracking-[0.06em] text-text-dim";

/** A phone's bottom sheet: menus that dock there, the More panes sheet, compose. */
export const SHEET =
  "rounded-t-[28px] bg-surface-800 border-t border-surface-600/40 shadow-[0_-12px_40px_-12px_rgb(0_0_0/0.6)]";

/** The grab handle at the top of a sheet. */
export const SHEET_HANDLE = "mx-auto mt-2 h-1 w-9 shrink-0 rounded-full bg-surface-600";

/** The dim behind a dialog or sheet. */
export const BACKDROP = "bg-black/60";

/** A dialog's panel. */
export const DIALOG = "rounded-2xl border border-surface-700/60 bg-surface-850 shadow-2xl";

const BUTTON =
  "inline-flex items-center justify-center gap-1.5 h-9 max-md:h-11 px-4 rounded-lg text-[13px] max-md:text-[15px] font-medium cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-not-allowed focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500";

/** The one action a surface is for. */
export const BUTTON_PRIMARY = `${BUTTON} bg-brand-600 text-surface-950 font-semibold hover:bg-brand-500`;

/** Any other action beside it. */
export const BUTTON_SECONDARY = `${BUTTON} border border-surface-600/70 bg-surface-800 text-text-primary hover:bg-surface-700/60`;

/** A quiet action, such as Cancel. */
export const BUTTON_GHOST = `${BUTTON} text-text-secondary hover:bg-surface-700/50 hover:text-text-primary`;

/** An action that removes or stops something. */
export const BUTTON_DANGER = `${BUTTON} bg-status-error text-white hover:opacity-90`;

/** A phone's app banners (Daemon down, updates, install steps) as one card under the page's top controls. */
export const PHONE_BANNER_CARD =
  "rounded-xl overflow-hidden border border-surface-700/60 bg-surface-850 shadow-xl empty:hidden";
