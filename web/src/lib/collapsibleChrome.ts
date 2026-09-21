// Class recipes for collapsible chrome regions, kept out of the component file for react-refresh and testing.

/** A `0fr`/`1fr` grid row removes the row from layout without measuring, unlike visibility or opacity. */
export function collapsibleRegionClass(collapsed: boolean): string {
  return `grid shrink-0 transition-[grid-template-rows] duration-200 ease-out motion-reduce:transition-none ${
    collapsed ? "grid-rows-[0fr]" : "grid-rows-[1fr]"
  }`;
}

/** `min-h-0` lets the row reach zero; clip only while collapsed since composer menus render outside the box. */
export function collapsibleInnerClass(collapsed: boolean): string {
  return collapsed ? "min-h-0 overflow-hidden" : "min-h-0";
}
