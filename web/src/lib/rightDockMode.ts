// How the right dock appears at a viewport width. Below `md` one pane fills the screen and a picker chooses it;
// up to the inline threshold a third column would squeeze the agent, so the dock overlays it as a sheet.

export type RightDockMode = "picker" | "sheet" | "inline";

export const MD_MIN_WIDTH = 768;
export const INLINE_MIN_WIDTH = 1100;

export function rightDockMode(viewportWidth: number): RightDockMode {
  if (viewportWidth < MD_MIN_WIDTH) return "picker";
  return viewportWidth < INLINE_MIN_WIDTH ? "sheet" : "inline";
}

// Range syntax keeps fractional widths just under the inline threshold (zoom, display scaling) in the sheet.
export const SHEET_MEDIA_QUERY = `(${MD_MIN_WIDTH}px <= width < ${INLINE_MIN_WIDTH}px)`;
