// The chrome an in-thread request for an answer shares: approvals and agent questions.

import { BUTTON_PRIMARY_SM, BUTTON_SECONDARY_SM, CAPTION_INLINE } from "../../lib/surfaces";

export const PROMPT_CARD = "my-2 overflow-hidden rounded-xl border shadow-md text-sm";

export const PROMPT_CARD_HEADER =
  "flex w-full items-center gap-2 px-3 py-2 max-md:py-3 text-left border-b border-surface-700/40";

/** The card's kind ("Question", "Approval needed") beside its icon. */
export const PROMPT_KIND = `shrink-0 ${CAPTION_INLINE}`;
export const PROMPT_KIND_DESTRUCTIVE = `shrink-0 ${CAPTION_INLINE.replace("text-text-dim", "text-status-error-text")}`;

export const PROMPT_ERROR = "px-3 text-xs text-status-error-text";

// Each card dims a blocked action itself, offline and in flight alike.
const unblocked = (button: string) => button.replace(" disabled:opacity-40 disabled:cursor-not-allowed", "");
export const PROMPT_PRIMARY = unblocked(BUTTON_PRIMARY_SM);
export const PROMPT_SECONDARY = unblocked(BUTTON_SECONDARY_SM);
/** Deny or cancel: quiet until hovered. */
export const PROMPT_REFUSE = `${PROMPT_SECONDARY} enabled:hover:border-status-error/50 enabled:hover:text-status-error-text`;

/** A choice the user picks: an answer option, a checkbox or radio row. */
export const PROMPT_CHOICE = "rounded-lg border cursor-pointer transition-colors max-md:py-3 max-md:text-[15px]";
export const PROMPT_CHOICE_ON = "border-brand-600 bg-brand-700/15 text-text-primary";
export const PROMPT_CHOICE_OFF = "border-surface-600/70 bg-surface-800 text-text-secondary hover:bg-surface-700/60";
