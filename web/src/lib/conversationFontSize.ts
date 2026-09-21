import { MAX_FONT_SIZE, MIN_FONT_SIZE } from "./fontSizeRange";

/** Transcript font size in px, stored separately from the terminal size over the same range. */
export const DEFAULT_CONVERSATION_FONT_SIZE = 14;

/** localStorage is user-editable, so never emit `NaNpx` or a 0px transcript. */
export function normalizeConversationFontSize(value: unknown): number {
  // Number() would turn `null` and `""` into 0 instead of falling back to the default.
  const n =
    typeof value === "number" ? value : typeof value === "string" && value.trim() !== "" ? Number(value) : Number.NaN;
  if (!Number.isFinite(n)) return DEFAULT_CONVERSATION_FONT_SIZE;
  return Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(n)));
}

/** Emitted as rem against a 16px root, so it scales with the browser's root font size. */
export function conversationFontSizeRem(px: number): string {
  return `${px / 16}rem`;
}
