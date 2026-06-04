import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionFileDiff } from "../lib/api";
import type { RichFileDiffResponse } from "../lib/types";

interface UseFileDiffResult {
  diff: RichFileDiffResponse | null;
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

export function useFileDiff(
  sessionId: string | null,
  filePath: string | null,
  /** Workspace repo name; pass for files belonging to a workspace
   *  member, leave undefined for single-repo sessions. See #1047. */
  repoName: string | undefined,
  /** Triggers a re-fetch when bumped (e.g. from useDiffFiles.revision). */
  externalRevision?: number,
): UseFileDiffResult {
  const [diff, setDiff] = useState<RichFileDiffResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestIdRef = useRef(0);

  const fetchDiff = useCallback(async () => {
    if (!sessionId || !filePath) {
      setDiff(null);
      return;
    }
    const reqId = ++requestIdRef.current;
    const capturedSessionId = sessionId;
    const capturedFilePath = filePath;
    const capturedRepoName = repoName;
    setLoading(true);
    setError(null);
    const resp = await getSessionFileDiff(
      capturedSessionId,
      capturedFilePath,
      capturedRepoName,
    );
    // Drop stale responses: rapid file/session switches can cause out-of-order replies
    if (
      reqId !== requestIdRef.current ||
      capturedSessionId !== sessionId ||
      capturedFilePath !== filePath ||
      capturedRepoName !== repoName
    ) {
      return;
    }
    if (resp) {
      setDiff(resp);
    } else {
      setError("Failed to load diff");
    }
    setLoading(false);
  }, [sessionId, filePath, repoName]);

  useEffect(() => {
    const timer = setTimeout(() => {
      void fetchDiff();
    }, 0);
    return () => clearTimeout(timer);
  }, [fetchDiff, externalRevision]);

  return { diff, loading, error, refresh: fetchDiff };
}
