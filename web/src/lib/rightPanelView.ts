/** Which view occupies the single full-viewport main pane on mobile
 *  (below the `md` breakpoint). Desktop ignores this and renders the
 *  side-by-side ContentSplit. See #1452. A `plugin:<plugin>:<entry>` id
 *  promotes a plugin pane into the mobile main pane (#2514); the prefix
 *  matches `isPluginPaneId` in `pluginPanes.ts`. `"diff"`, `"files"`, and
 *  `"agents"` mirror the desktop `BuiltinPaneId`s of the same name and are
 *  gated the same way desktop gates them: see `availablePanes` (derived
 *  from `allPaneIds` in `App.tsx`) on `MobileRightPanelPicker`. */
export type RightPanelView = "agent" | "diff" | "files" | "paired" | "agents" | `plugin:${string}`;
