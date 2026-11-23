import { useLayoutEffect, type ReactNode } from "react";
import { useConnectionBanner } from "../hooks/useConnectionBanner";
import { useDashboardUpdate } from "../hooks/useDashboardUpdate";
import { useSandboxImage } from "../hooks/useSandboxImage";
import { useUpdateBanner } from "../hooks/useUpdateBanner";
import { DashboardUpdateBanner } from "./DashboardUpdateBanner";
import { DisconnectBanner } from "./DisconnectBanner";
import { PushHealthBanner } from "./PushHealthBanner";
import { SandboxImageBanner } from "./SandboxImageBanner";
import { ServerSettingsBanner } from "./ServerSettingsBanner";
import { UpdateBanner } from "./UpdateBanner";

/**
 * The connection and update banners, one at a time, most urgent first: Disconnected, server settings
 * unavailable, Dashboard updated, the Reconnected flash, Update available, then the sandbox image. The push
 * banner may sit below that one, except while Disconnected. `onRetryServerSettings` is set while /api/about
 * has no answer. The sandbox banner comes from `SandboxImageProvider`, which also learns whether a higher
 * banner is covering it.
 */
export function BannerStack({ onRetryServerSettings }: { onRetryServerSettings?: () => void } = {}) {
  const connection = useConnectionBanner();
  const dashboardVersion = useDashboardUpdate();
  const update = useUpdateBanner();
  const sandbox = useSandboxImage();
  const sandboxImage = sandbox?.banner.view ?? null;

  let top: ReactNode = null;
  if (connection === "down") top = <DisconnectBanner state="down" />;
  else if (onRetryServerSettings) top = <ServerSettingsBanner onRetry={onRetryServerSettings} />;
  else if (dashboardVersion !== null) top = <DashboardUpdateBanner version={dashboardVersion} />;
  else if (connection === "reconnected") top = <DisconnectBanner state="reconnected" />;
  else if (update) top = <UpdateBanner status={update.status} onDismiss={update.dismiss} />;
  const higherShown = top !== null;
  if (!higherShown && sandboxImage) top = <SandboxImageBanner view={sandboxImage} />;

  const setHigherBannerShown = sandbox?.setHigherBannerShown;
  // Before paint, so the chip and the banner never disagree on screen.
  useLayoutEffect(() => {
    setHigherBannerShown?.(higherShown);
  }, [higherShown, setHigherBannerShown]);

  return (
    <>
      {top}
      <PushHealthBanner hidden={connection === "down"} />
    </>
  );
}
