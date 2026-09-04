/** Px range shared by every font-size control in the dashboard: the terminal
 *  sizes under `Web Dashboard -> Terminal` and the structured-view conversation
 *  sizes under `Sessions -> Structured view`. One declaration rather than a
 *  copy per panel, so the two sliders always span the same steps and a value
 *  carried between them stays in bounds. */
export const MIN_FONT_SIZE = 6;
export const MAX_FONT_SIZE = 28;
