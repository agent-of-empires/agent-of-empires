import { createElement, type ComponentType } from "react";
import type { LucideIcon } from "lucide-react";

import { useAssetFailed } from "../lib/pluginUi";

interface Props {
  /** Already-resolved fallback: a built-in pane's own icon, or a plugin pane's per-pane runtime icon / manifest
   *  icon / generic Puzzle, per `resolvePaneIcon`'s chain. */
  icon: LucideIcon;
  /** A plugin's manifest `icon_asset`, resolved to a fetchable URL. */
  iconAssetUrl?: string;
  className: string;
  testId?: string;
}

/** The activity-bar/dock-tab icon for one pane. */
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
