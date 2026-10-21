import type { ReactNode } from "react";
import { useConnectionBanner } from "../hooks/useConnectionBanner";
import { useDashboardUpdate } from "../hooks/useDashboardUpdate";
import { useUpdateBanner } from "../hooks/useUpdateBanner";
import { DashboardUpdateBanner } from "./DashboardUpdateBanner";
import { DisconnectBanner } from "./DisconnectBanner";
import { PushHealthBanner } from "./PushHealthBanner";
import { UpdateBanner } from "./UpdateBanner";

/**
 * The connection and update banners, one at a time, most urgent first: Disconnected, Dashboard updated, the
 * Reconnected flash, then Update available. The push banner may sit below that one, except while Disconnected.
 */
export function BannerStack() {
  const connection = useConnectionBanner();
  const dashboardUpdated = useDashboardUpdate();
  const update = useUpdateBanner();

  let top: ReactNode = null;
  if (connection === "down") top = <DisconnectBanner state="down" />;
  else if (dashboardUpdated) top = <DashboardUpdateBanner />;
  else if (connection === "reconnected") top = <DisconnectBanner state="reconnected" />;
  else if (update) top = <UpdateBanner status={update.status} onDismiss={update.dismiss} />;

  return (
    <>
      {top}
      <PushHealthBanner hidden={connection === "down"} />
    </>
  );
}
