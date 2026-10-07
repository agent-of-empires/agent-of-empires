import { createElement, type ComponentType } from "react";
import { Puzzle } from "lucide-react";

import { lucideIcon, useAssetFailed } from "../../lib/pluginUi";

interface Props {
  icon?: string | null;
  iconAssetUrl?: string | null;
  className?: string;
  testId?: string;
}

/** `icon_asset_url`, else the lucide `icon`, else `Puzzle`. Decorative: always shown beside the name. */
export function PluginIdentityIcon({ icon, iconAssetUrl, className = "size-4", testId }: Props) {
  const [assetFailed, markFailed] = useAssetFailed(iconAssetUrl);

  if (iconAssetUrl && !assetFailed) {
    return (
      <img
        src={iconAssetUrl}
        alt=""
        aria-hidden="true"
        data-testid={testId}
        className={`${className} shrink-0 rounded-sm object-contain`}
        onError={markFailed}
      />
    );
  }

  const Icon = (icon && lucideIcon(icon)) || Puzzle;
  // Widened so `data-testid` lands on the svg itself.
  return createElement(Icon as ComponentType<Record<string, unknown>>, {
    className: `${className} shrink-0`,
    "aria-hidden": true,
    "data-testid": testId,
  });
}
