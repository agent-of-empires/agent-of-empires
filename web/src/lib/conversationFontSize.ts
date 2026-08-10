import { MAX_FONT_SIZE, MIN_FONT_SIZE } from "./fontSizeRange";

/** Font size (px) for the structured-view conversation transcript. Client-local
 *  and stored separately from the terminal font sizes: the transcript is
 *  proportional prose, the terminal is a fixed-cell grid, and users size them
 *  for different reasons. The two controls span the same range
 *  ({@link MIN_FONT_SIZE}-{@link MAX_FONT_SIZE}) so they read alike. See the
 *  settings under `Settings -> Structured view`. */
export const DEFAULT_CONVERSATION_FONT_SIZE = 14;

/** localStorage is user-editable, so a stringy, fractional, NaN, or wildly
 *  out-of-range value must not reach CSS as `NaNpx` or a 0px transcript. */
export function normalizeConversationFontSize(value: unknown): number {
  // Only numbers and non-blank numeric strings are candidates. Coercing
  // everything through Number() would turn `null` (what JSON.stringify writes
  // for a NaN) and `""` into 0, which then clamps to the minimum instead of
  // falling back to the default.
  const n =
    typeof value === "number" ? value : typeof value === "string" && value.trim() !== "" ? Number(value) : Number.NaN;
  if (!Number.isFinite(n)) return DEFAULT_CONVERSATION_FONT_SIZE;
  return Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(n)));
}
