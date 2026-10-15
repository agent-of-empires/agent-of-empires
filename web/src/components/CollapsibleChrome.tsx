import type { ReactNode } from "react";

import { collapsibleInnerClass, collapsibleRegionClass } from "../lib/collapsibleChrome";

/** Chrome region (top bar, composer) that collapses to zero layout height. */
export function CollapsibleRegion({
  id,
  collapsed,
  children,
}: {
  /** Stable unique id; pass the same value as the handle's `controlsId`. */
  id: string;
  collapsed: boolean;
  children: ReactNode;
}) {
  return (
    <div id={id} data-testid={id} className={collapsibleRegionClass(collapsed)}>
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
  /** `id` of the {@link CollapsibleRegion} this handle collapses. */
  controlsId: string;
  testId: string;
}

/** Persistent collapse handle for a {@link CollapsibleRegion}. */
export function ChromeCollapseHandle({
  edge,
  collapsed,
  onToggle,
  collapseLabel,
  expandLabel,
  controlsId,
  testId,
}: HandleProps) {
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
        aria-controls={controlsId}
        aria-label={label}
        title={label}
        data-testid={testId}
        className={`absolute right-3 flex h-4 w-7 items-center justify-center border-surface-700/60 bg-surface-850/95 text-brand-500 shadow-sm cursor-pointer transition-colors hover:text-brand-400 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600 ${
          edge === "top" ? "top-0 rounded-b-md border-x border-b" : "bottom-0 rounded-t-md border-x border-t"
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
      </button>
    </div>
  );
}
