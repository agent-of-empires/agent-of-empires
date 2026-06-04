/** Which view occupies the single full-viewport main pane on mobile
 *  (below the `md` breakpoint). Desktop ignores this and renders the
 *  side-by-side ContentSplit. See #1452. */
export type RightPanelView = "agent" | "diff" | "paired";
