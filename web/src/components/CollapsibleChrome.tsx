import type { ReactNode } from "react";

import { collapsibleInnerClass, collapsibleRegionClass } from "../lib/collapsibleChrome";

/** Chrome region, such as a phone's composer, that collapses to zero layout height. */
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
  collapsed: boolean;
  onToggle: () => void;
  collapseLabel: string;
  expandLabel: string;
  /** `id` of the {@link CollapsibleRegion} this handle collapses. */
  controlsId: string;
  testId: string;
}

/** Persistent collapse handle hung above a {@link CollapsibleRegion} at the bottom of the screen. */
export function ChromeCollapseHandle({
  collapsed,
  onToggle,
  collapseLabel,
  expandLabel,
  controlsId,
  testId,
}: HandleProps) {
  // Expanded, it points down (tap to fold the region away); collapsed, up.
  const pointsUp = collapsed;
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
        className="absolute right-3 bottom-0 flex h-4 w-7 items-center justify-center rounded-t-md border-x border-t border-surface-700/60 bg-surface-850/95 text-brand-500 shadow-sm cursor-pointer transition-colors hover:text-brand-400 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-brand-600"
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
