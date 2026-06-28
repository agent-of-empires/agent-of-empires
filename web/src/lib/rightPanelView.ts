/** Which view occupies the single full-viewport main pane on mobile
 *  (below the `md` breakpoint). Desktop ignores this and renders the
 *  side-by-side ContentSplit. See #1452. A `plugin:<plugin>:<entry>` id
 *  promotes a plugin pane into the mobile main pane (#2514); the prefix
 *  matches `isPluginPaneId` in `pluginPanes.ts`. */
export type RightPanelView = "agent" | "diff" | "paired" | `plugin:${string}`;
