import type { ReactNode } from "react";

import { collapsibleInnerClass, collapsibleRegionClass } from "../lib/collapsibleChrome";

/** Chrome region (top bar, composer) that collapses to zero layout height.
 *
 *  `testId` lands on the *outer* row: that is the element whose measured height
 *  is the feature's contract (0 when collapsed). The collapsed child keeps a
 *  non-zero box of its own, clipped by the row, so asserting on the child would
 *  report it as visible. */
export function CollapsibleRegion({
  collapsed,
  children,
  testId,
}: {
  collapsed: boolean;
  children: ReactNode;
  testId?: string;
}) {
  return (
    <div className={collapsibleRegionClass(collapsed)} data-testid={testId}>
      {/* `inert` keeps the hidden region out of the tab order and off the
          accessibility tree; the toggle itself lives outside so it survives. */}
      <div className={collapsibleInnerClass(collapsed)} inert={collapsed}>
        {children}
      </div>
    </div>
  );
}

interface HandleProps {
  /** `top` hangs the handle below the region (collapsing top chrome),
   *  `bottom` hangs it above the region (collapsing bottom chrome). */
  edge: "top" | "bottom";
  collapsed: boolean;
  onToggle: () => void;
  collapseLabel: string;
  expandLabel: string;
  testId: string;
}

/** Persistent collapse handle for a {@link CollapsibleRegion}.
 *
 *  Rendered as a sibling of the region, never inside it: a handle nested in
 *  the collapsing element would disappear with it and strand the user in the
 *  collapsed state. The host is zero-height and the button is absolutely
 *  positioned, so the handle costs the layout nothing in either state.
 *
 *  The `<button>` itself follows the repo's 32px (`h-8 w-8`) touch-target
 *  convention, since this handle is the only way to restore a collapsed
 *  region; the visible tab stays a small `h-4 w-7` inner element so the
 *  larger hit area doesn't grow the overlay onto more of the transcript. */
export function ChromeCollapseHandle({ edge, collapsed, onToggle, collapseLabel, expandLabel, testId }: HandleProps) {
  // Expanded top chrome points up (tap to fold it away upward); expanded
  // bottom chrome points down. Collapsed flips both.
  const pointsUp = edge === "top" ? !collapsed : collapsed;
  const label = collapsed ? expandLabel : collapseLabel;
  return (
    <div className="relative z-20 h-0 shrink-0">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={!collapsed}
        aria-label={label}
        title={label}
        data-testid={testId}
        className={`absolute right-3 flex h-8 w-8 items-center justify-center text-brand-500 cursor-pointer transition-colors hover:text-brand-400 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
          edge === "top" ? "top-0 items-start" : "bottom-0 items-end"
        }`}
      >
        <span
          className={`flex h-4 w-7 items-center justify-center border-surface-700/60 bg-surface-850/95 shadow-sm ${
            edge === "top" ? "rounded-b-md border-x border-b" : "rounded-t-md border-x border-t"
          }`}
        >
          <svg
            width="7"
            height="7"
            viewBox="0 0 10 10"
            fill="currentColor"
            aria-hidden
            className={`transition-transform duration-200 motion-reduce:transition-none ${pointsUp ? "" : "rotate-180"}`}
          >
            <path d="M5 2.5 9 7.5H1z" />
          </svg>
        </span>
      </button>
    </div>
  );
}
