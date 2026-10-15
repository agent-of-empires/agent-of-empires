// Server identity (/api/about) and the telemetry consent state that loads alongside it.

import { useCallback, useEffect, useState } from "react";
import {
  fetchAbout,
  fetchTelemetryStatus,
  reportTelemetrySeen,
  setTelemetryConsent,
  type ServerAbout,
} from "../../lib/api";

export function useServerAbout() {
  const [serverAbout, setServerAbout] = useState<ServerAbout | null>(null);
  const [serverAboutLoaded, setServerAboutLoaded] = useState(false);
  const [telemetryConsentNeeded, setTelemetryConsentNeeded] = useState(false);
  const [telemetryConsentKnown, setTelemetryConsentKnown] = useState(false);

  const loadAbout = useCallback(async (reportSeen: boolean) => {
    try {
      const about = await fetchAbout();
      if (!about) return;
      setServerAbout(about);
      if (reportSeen && !about.read_only) reportTelemetrySeen("web");
    } finally {
      setServerAboutLoaded(true);
    }
  }, []);
  const refreshServerAbout = useCallback(() => loadAbout(false), [loadAbout]);

  useEffect(() => {
    let active = true;
    void loadAbout(true);
    void fetchTelemetryStatus()
      .then((status) => {
        if (active && status && !status.responded && !status.do_not_track) setTelemetryConsentNeeded(true);
      })
      .finally(() => {
        if (active) setTelemetryConsentKnown(true);
      });
    return () => {
      active = false;
    };
  }, [loadAbout]);

  const chooseTelemetryConsent = useCallback((enabled: boolean) => {
    setTelemetryConsentNeeded(false);
    void setTelemetryConsent(enabled);
  }, []);

  return {
    serverAbout,
    serverAboutLoaded,
    refreshServerAbout,
    telemetryConsentNeeded,
    telemetryConsentKnown,
    chooseTelemetryConsent,
  };
}
