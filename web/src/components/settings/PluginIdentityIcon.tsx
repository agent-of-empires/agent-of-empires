import { createElement, useState, type ComponentType } from "react";
import { Puzzle } from "lucide-react";

import { lucideIcon } from "../../lib/pluginUi";

interface Props {
  /** Manifest `icon`: a lucide kebab-case name, or null/undefined if unset. */
  icon?: string | null;
  /** Manifest `icon_asset` resolved to a fetchable URL, or null/undefined. */
  iconAssetUrl?: string | null;
  className?: string;
  testId?: string;
}

/** A plugin's identity glyph: `icon_asset_url` if present (falls back to
 *  `icon` if the image 404s), else the manifest's lucide `icon`, else the
 *  generic `Puzzle` icon. Always rendered next to the plugin's name on every
 *  surface that uses it, so the icon itself is decorative (`alt=""
 *  aria-hidden`) rather than needing author-supplied alt text. */
export function PluginIdentityIcon({ icon, iconAssetUrl, className = "size-4", testId }: Props) {
  const [assetFailed, setAssetFailed] = useState(false);
  // Reset the failure flag whenever a new URL is supplied (e.g. the detail
  // modal swaps the local fallback route for the resolved manifest URL once
  // its gh fetch lands), so a transient failure on the first URL doesn't
  // permanently hide a later working one.
  const [trackedUrl, setTrackedUrl] = useState(iconAssetUrl);
  if (iconAssetUrl !== trackedUrl) {
    setTrackedUrl(iconAssetUrl);
    setAssetFailed(false);
  }

  if (iconAssetUrl && !assetFailed) {
    return (
      <img
        src={iconAssetUrl}
        alt=""
        aria-hidden="true"
        data-testid={testId}
        className={`${className} shrink-0 rounded-sm object-contain`}
        onError={() => setAssetFailed(true)}
      />
    );
  }

  const Icon = (icon && lucideIcon(icon)) || Puzzle;
  // `data-testid` isn't in LucideProps; widen to a generic component type
  // rather than dropping the attribute, since assertions need to reach the
  // rendered svg itself, not a wrapping element.
  return createElement(Icon as ComponentType<Record<string, unknown>>, {
    className: `${className} shrink-0`,
    "aria-hidden": true,
    "data-testid": testId,
  });
}
