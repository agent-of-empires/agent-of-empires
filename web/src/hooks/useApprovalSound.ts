import { useEffect, useState } from "react";
import { noteApprovalChime, playSoundFor } from "../lib/soundPlayer";

// The WS replays stored events on connect; don't chime for approvals that were already pending.
const REPLAY_QUIET_MS = 1500;

// The chime only mounts for the session whose view is open, so that session is the active one.
export function useApprovalSound(pendingCount: number, sessionId?: string): void {
  const [trackedPendingCount, setTrackedPendingCount] = useState(pendingCount);
  const [quietPeriodDone, setQuietPeriodDone] = useState(false);
  const [playbackToken, setPlaybackToken] = useState(0);

  useEffect(() => {
    const timer = setTimeout(() => {
      setQuietPeriodDone(true);
    }, REPLAY_QUIET_MS);
    return () => clearTimeout(timer);
  }, []);

  useEffect(() => {
    if (playbackToken === 0) return;
    const timer = setTimeout(() => {
      void playSoundFor("approval", {
        sessionIds: sessionId ? [sessionId] : [],
        activeSessionId: () => sessionId ?? null,
      }).then((played) => {
        if (played && sessionId) noteApprovalChime(sessionId);
      });
    }, 0);
    return () => clearTimeout(timer);
  }, [playbackToken, sessionId]);

  if (pendingCount !== trackedPendingCount) {
    if (trackedPendingCount === 0 && pendingCount !== 0 && quietPeriodDone) {
      setPlaybackToken((t) => t + 1);
    }
    setTrackedPendingCount(pendingCount);
  }
}
