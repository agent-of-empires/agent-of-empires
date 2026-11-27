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

/** A menu row with a description line under its label. */
export const MENU_OPTION_ROW = MENU_ITEM.replace("items-center", "items-start");

/** A menu row that removes or stops something. */
export const MENU_ITEM_DANGER = MENU_ITEM.replace("text-text-secondary", "text-status-error").replace(
  "hover:text-text-primary",
  "hover:text-status-error",
);

/** A rule between a menu's groups. */
export const MENU_SEPARATOR = "my-1 mx-3 border-t border-surface-700/50";

/** A small heading's type, over a menu group or wherever a quiet label sits. */
export const MENU_HEADING_TEXT = "text-[11px] font-semibold uppercase tracking-[0.06em] text-text-dim";

/** A small heading over a menu group. */
export const MENU_HEADING = `px-3 pt-2 pb-1 ${MENU_HEADING_TEXT}`;

/** A floating menu's look on a phone: frosted, with the phone's larger radius. */
export const MENU_PHONE = "max-md:glass-menu max-md:rounded-2xl";

/** A phone's bottom sheet: menus that dock there, the More panes sheet, compose. */
export const SHEET =
  "rounded-t-[28px] bg-surface-800 chrome-drawer border-t border-surface-600/40 shadow-[0_-12px_40px_-12px_rgb(0_0_0/0.6)]";

/** The grab handle at the top of a sheet. */
export const SHEET_HANDLE = "mx-auto mt-2 h-1 w-9 shrink-0 rounded-full bg-surface-600";

/** The dim behind a dialog or sheet. */
export const BACKDROP = "bg-black/60";

/** A dialog's panel. */
export const DIALOG = "rounded-2xl border border-surface-700/60 bg-surface-850 shadow-2xl";

/** A button's shape without its colours, for a caller that brings its own. */
export const BUTTON =
  "inline-flex items-center justify-center gap-1.5 h-9 max-md:h-11 px-4 rounded-lg text-[13px] max-md:text-[15px] font-medium cursor-pointer transition-colors disabled:opacity-40 disabled:cursor-not-allowed focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500";

/** The one action a surface is for. */
export const BUTTON_PRIMARY = `${BUTTON} bg-brand-600 text-surface-950 font-semibold hover:bg-brand-500`;

/** Any other action beside it. */
export const BUTTON_SECONDARY = `${BUTTON} border border-surface-600/70 bg-surface-800 text-text-primary hover:bg-surface-700/60`;

/** A quiet action, such as Cancel. */
export const BUTTON_GHOST = `${BUTTON} text-text-secondary hover:bg-surface-700/50 hover:text-text-primary`;

/** An action that removes or stops something. */
export const BUTTON_DANGER = `${BUTTON} bg-status-error text-white hover:opacity-90`;

// Class strings are rewritten rather than appended to, because two utilities for one property
// resolve by stylesheet order, not by order in the attribute.
const compact = (button: string) =>
  button.replace("h-9", "h-7").replace("px-4", "px-2.5").replace("text-[13px]", "text-[12px]");

/** An action inside a row or card. Phones keep the full-height tap target. */
export const BUTTON_PRIMARY_SM = compact(BUTTON_PRIMARY);
export const BUTTON_SECONDARY_SM = compact(BUTTON_SECONDARY);
export const BUTTON_GHOST_SM = compact(BUTTON_GHOST);

/** A small action set in a banner, notice or sentence, held at the 32px desktop floor. */
export const BUTTON_INLINE = `${BUTTON_SECONDARY_SM} min-h-8 align-middle shrink-0 whitespace-nowrap`;

/** An action on a warning-tinted strip or card. */
export const BUTTON_WARNING_SM = BUTTON_SECONDARY_SM.replace(
  "border-surface-600/70 bg-surface-800 text-text-primary hover:bg-surface-700/60",
  "border-status-warning/40 bg-status-warning/20 text-status-warning hover:bg-status-warning/30",
);

/** A quiet action that removes something, beside others in a row. */
export const BUTTON_DANGER_GHOST_SM = compact(
  BUTTON_GHOST.replace(
    "text-text-secondary hover:bg-surface-700/50 hover:text-text-primary",
    "text-status-error hover:bg-status-error/10",
  ),
);

/** A caption over a list's rows inside a panel, aligned with their 12px inset. */
export const ROW_CAPTION = CAPTION.replace("px-1 mb-2", "px-3 pt-2.5 pb-1");

/** A caption inside a card, where its parent spaces it. */
export const CAPTION_INLINE = CAPTION.replace("px-1 mb-2", "");

/** A box set into a CARD: one item of a list editor, a nested group. */
export const INSET = "rounded-lg border border-surface-700/50 bg-surface-900";

/** Rows inside a CARD, split by hairlines. */
export const ROWS = "divide-y divide-surface-700/40";

/** A block of verbatim text: logs, output, an error, a bundle. */
export const CODE_BLOCK =
  "overflow-auto rounded-lg border border-surface-700/50 bg-surface-950 p-3 font-mono text-[12px] leading-relaxed text-text-secondary";

/** A form field's label, over its control. */
export const FIELD_LABEL = "block text-[15px] md:text-sm text-text-primary mb-1";

/** The line under a field's label that says what it does. */
export const FIELD_DESCRIPTION = "text-[13px] leading-normal text-text-secondary mb-2";

/** A shorter note beside or under a field. */
export const FIELD_HINT = "text-[13px] md:text-xs leading-normal text-text-dim";

/** A dialog's body: scrolls once the panel reaches its height cap. */
export const DIALOG_BODY = "min-h-0 flex-1 overflow-y-auto px-5 max-md:px-4 py-4";

/** A dialog's action row. A wrapped row scrolls within its bound so its last action stays reachable on short
 *  screens; on phones the actions share the row's width. */
export const DIALOG_FOOTER =
  "flex flex-wrap items-center justify-end gap-2 px-5 max-md:px-4 py-3 border-t border-surface-700/50 max-h-[40vh] overflow-y-auto max-md:[&_button]:flex-1";

/** The dim behind a full-size image, darker than a dialog's. */
export const LIGHTBOX = "fixed inset-0 flex items-center justify-center bg-black/85 p-4 animate-fade-in";

/** A CARD of rows set into a dialog, darker than the panel it sits on. */
export const INSET_CARD = `${CARD} !px-0 !bg-surface-900`;

const CHOICE_BASE =
  "w-full min-h-11 text-left rounded-xl border px-3 py-2.5 transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-brand-500";
/** One option in a list of choices (a radio card); CHOICE_SELECTED marks the picked one. */
export const CHOICE = `${CHOICE_BASE} border-surface-700/60 bg-surface-900 hover:border-surface-600`;
export const CHOICE_SELECTED = `${CHOICE_BASE} border-brand-600 bg-brand-600/10`;

/** A phone's app banners (Daemon down, updates, install steps) as one card under the page's top controls. */
export const PHONE_BANNER_CARD =
  "rounded-xl overflow-hidden border border-surface-700/60 bg-surface-850 shadow-xl empty:hidden";
