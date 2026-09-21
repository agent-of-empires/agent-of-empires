import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionFileContents } from "../lib/api";
import type { RichFileContentsResponse } from "../lib/types";

interface UseFileContentsResult {
  contents: RichFileContentsResponse | null;
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

const MAX_CACHE_ENTRIES = 60;
const MAX_CACHE_BYTES = 32 * 1024 * 1024;

interface CacheEntry {
  value: RichFileContentsResponse;
  bytes: number;
}

const contentsCache = new Map<string, CacheEntry>();
let cacheBytes = 0;

function entrySize(value: RichFileContentsResponse): number {
  return value.old_content.length + value.new_content.length + value.patch.length;
}

function cacheKeyFor(
  sessionId: string,
  filePath: string,
  repoName: string | undefined,
  revision: number | undefined,
): string {
  return JSON.stringify([sessionId, filePath, repoName ?? null, revision ?? 0]);
}

function cachePut(key: string, value: RichFileContentsResponse) {
  const existing = contentsCache.get(key);
  if (existing) cacheBytes -= existing.bytes;
  contentsCache.delete(key);
  const bytes = entrySize(value);
  contentsCache.set(key, { value, bytes });
  cacheBytes += bytes;
  while (contentsCache.size > MAX_CACHE_ENTRIES || (cacheBytes > MAX_CACHE_BYTES && contentsCache.size > 1)) {
    const oldestKey = contentsCache.keys().next().value;
    if (oldestKey === undefined) break;
    const oldest = contentsCache.get(oldestKey);
    contentsCache.delete(oldestKey);
    if (oldest) cacheBytes -= oldest.bytes;
  }
}

function cacheGet(key: string): RichFileContentsResponse | null {
  const hit = contentsCache.get(key);
  if (!hit) return null;
  contentsCache.delete(key);
  contentsCache.set(key, hit);
  return hit.value;
}

export function __resetFileContentsCache() {
  contentsCache.clear();
  cacheBytes = 0;
}

export function useFileContents(
  sessionId: string | null,
  filePath: string | null,
  repoName: string | undefined,
  externalRevision?: number,
): UseFileContentsResult {
  const key = sessionId && filePath ? cacheKeyFor(sessionId, filePath, repoName, externalRevision) : null;

  const [contents, setContents] = useState<RichFileContentsResponse | null>(() => (key ? cacheGet(key) : null));
  const [loading, setLoading] = useState(key != null && cacheGet(key) == null);
  const [error, setError] = useState<string | null>(null);
  const [handledKey, setHandledKey] = useState(key);
  const requestIdRef = useRef(0);

  if (key !== handledKey) {
    setHandledKey(key);
    setError(null);
    if (!key) {
      setContents(null);
      setLoading(false);
    } else {
      const hit = cacheGet(key);
      if (hit) {
        setContents(hit);
        setLoading(false);
      } else {
        setLoading(true);
      }
    }
  }

  const fetchContents = useCallback(
    async (force = false) => {
      if (!sessionId || !filePath) {
        setContents(null);
        setLoading(false);
        return;
      }
      const k = cacheKeyFor(sessionId, filePath, repoName, externalRevision);
      if (!force) {
        const hit = cacheGet(k);
        if (hit) {
          setContents(hit);
          setLoading(false);
          setError(null);
          return;
        }
      }
      const reqId = ++requestIdRef.current;
      setLoading(true);
      setError(null);
      const resp = await getSessionFileContents(sessionId, filePath, repoName);
      if (reqId !== requestIdRef.current) return;
      if (resp) {
        cachePut(k, resp);
        setContents(resp);
      } else {
        setError("Failed to load file contents");
      }
      setLoading(false);
    },
    [sessionId, filePath, repoName, externalRevision],
  );

  useEffect(() => {
    const timer = setTimeout(() => {
      void fetchContents();
    }, 0);
    return () => clearTimeout(timer);
  }, [fetchContents]);

  const refresh = useCallback(() => fetchContents(true), [fetchContents]);

  return { contents, loading, error, refresh };
}
