/** Class recipes for the collapsible chrome regions (mobile top bar and
 *  composer). Split out of `components/CollapsibleChrome.tsx` so the component
 *  file exports components only (react-refresh) and the collapse contract is
 *  unit-testable without a layout engine. */

/** Wrapper classes for a chrome region that can release its layout height.
 *
 *  A `0fr` / `1fr` grid row genuinely removes the row from layout (the
 *  neighbouring `flex-1` transcript grows into the freed space) while `1fr`
 *  restores the child's intrinsic height, so neither state needs a measured
 *  pixel value or a JS height calculation. `visibility`/`opacity` would keep
 *  the space reserved, which is exactly what this must not do. */
export function collapsibleRegionClass(collapsed: boolean): string {
  return `grid shrink-0 transition-[grid-template-rows] duration-200 ease-out motion-reduce:transition-none ${
    collapsed ? "grid-rows-[0fr]" : "grid-rows-[1fr]"
  }`;
}

/** Inner classes for the collapsible child. `min-h-0` is what lets the `0fr`
 *  row actually reach zero (a grid item's automatic minimum size would
 *  otherwise floor it at its content height). Clipping is applied only while
 *  collapsed: the composer's model / permission menus render `absolute
 *  bottom-full`, i.e. outside their parent box, and a permanent
 *  `overflow-hidden` would cut them off while expanded. */
export function collapsibleInnerClass(collapsed: boolean): string {
  return collapsed ? "min-h-0 overflow-hidden" : "min-h-0";
}
