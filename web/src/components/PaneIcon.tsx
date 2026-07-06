import { createElement, type ComponentType } from "react";
import type { LucideIcon } from "lucide-react";

import { useAssetFailed } from "../lib/pluginUi";

interface Props {
  /** Already-resolved fallback: a built-in pane's own icon, or a plugin
   *  pane's per-pane runtime icon / manifest icon / generic Puzzle, per
   *  `resolvePaneIcon`'s chain. Used when `iconAssetUrl` is absent or fails. */
  icon: LucideIcon;
  /** A plugin's manifest `icon_asset`, resolved to a fetchable URL. Wins over
   *  `icon` unconditionally when present and loading: a plugin's own real
   *  logo is a stronger identity signal than any lucide glyph, including one
   *  a plugin's worker deliberately chose for a specific pane before this
   *  field existed. */
  iconAssetUrl?: string;
  className: string;
  testId?: string;
}

/** The activity-bar/dock-tab icon for one pane. Mirrors
 *  `PluginIdentityIcon`'s precedence and reset-on-URL-change behavior via the
 *  shared `useAssetFailed` hook, but takes an already-resolved fallback
 *  component instead of a lucide name, since built-in panes pass a concrete
 *  icon (`FileDiff`, `SquareTerminal`, ...) rather than a manifest string. */
export function PaneIcon({ icon, iconAssetUrl, className, testId }: Props) {
  const [assetFailed, markFailed] = useAssetFailed(iconAssetUrl);

  if (iconAssetUrl && !assetFailed) {
    return (
      <img
        src={iconAssetUrl}
        alt=""
        aria-hidden="true"
        data-testid={testId}
        className={`${className} rounded-sm object-contain`}
        onError={markFailed}
      />
    );
  }

  // `data-testid` isn't in LucideProps; widen to a generic component type
  // rather than dropping the attribute.
  return createElement(icon as ComponentType<Record<string, unknown>>, {
    className,
    "aria-hidden": true,
    "data-testid": testId,
  });
}
