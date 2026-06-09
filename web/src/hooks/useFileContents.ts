import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionFileContents } from "../lib/api";
import type { RichFileContentsResponse } from "../lib/types";

interface UseFileContentsResult {
  contents: RichFileContentsResponse | null;
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

/**
 * Fetch raw old/new file text for the contents-based (`@pierre/diffs`)
 * renderer. Mirrors {@link useFileDiff}'s request-dedup/stale-drop behavior
 * but hits `?mode=contents`.
 */
export function useFileContents(
  sessionId: string | null,
  filePath: string | null,
  /** Workspace repo name; undefined for single-repo sessions. See #1047. */
  repoName: string | undefined,
  /** Triggers a re-fetch when bumped (e.g. from useDiffFiles.revision). */
  externalRevision?: number,
): UseFileContentsResult {
  const [contents, setContents] = useState<RichFileContentsResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestIdRef = useRef(0);

  const fetchContents = useCallback(async () => {
    if (!sessionId || !filePath) {
      setContents(null);
      return;
    }
    const reqId = ++requestIdRef.current;
    const capturedSessionId = sessionId;
    const capturedFilePath = filePath;
    const capturedRepoName = repoName;
    setLoading(true);
    setError(null);
    const resp = await getSessionFileContents(capturedSessionId, capturedFilePath, capturedRepoName);
    // Drop stale responses from rapid file/session switches.
    if (
      reqId !== requestIdRef.current ||
      capturedSessionId !== sessionId ||
      capturedFilePath !== filePath ||
      capturedRepoName !== repoName
    ) {
      return;
    }
    if (resp) {
      setContents(resp);
    } else {
      setError("Failed to load file contents");
    }
    setLoading(false);
  }, [sessionId, filePath, repoName]);

  useEffect(() => {
    const timer = setTimeout(() => {
      void fetchContents();
    }, 0);
    return () => clearTimeout(timer);
  }, [fetchContents, externalRevision]);

  return { contents, loading, error, refresh: fetchContents };
}
