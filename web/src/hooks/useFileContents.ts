import { useCallback, useEffect, useRef, useState } from "react";
import { getSessionFileContents } from "../lib/api";
import type { RichFileContentsResponse } from "../lib/types";

interface UseFileContentsResult {
  contents: RichFileContentsResponse | null;
  loading: boolean;
  error: string | null;
  refresh: () => void;
}

/** Module-level LRU of fetched contents so revisiting a file (or returning to
 *  one after a switch) is instant instead of re-hitting the server. Keyed by
 *  the full request identity including revision, so a file-list change (bumped
 *  revision) misses and re-fetches fresh contents.
 *
 *  Eviction is bounded by two independent caps because contents payloads vary
 *  wildly: most are tiny, but a single one can approach the server's
 *  ~5MB truncation threshold. {@link MAX_CACHE_ENTRIES} caps how many small
 *  files we keep; {@link MAX_CACHE_BYTES} caps total memory so a few large
 *  files can't blow the budget. Map insertion order is the LRU order; the
 *  oldest entries are evicted until both caps are satisfied. */
const MAX_CACHE_ENTRIES = 60;
const MAX_CACHE_BYTES = 32 * 1024 * 1024;

interface CacheEntry {
  value: RichFileContentsResponse;
  bytes: number;
}

const contentsCache = new Map<string, CacheEntry>();
let cacheBytes = 0;

function entrySize(value: RichFileContentsResponse): number {
  // Char length is a good-enough proxy for payload bytes here; exact UTF-8
  // sizing isn't worth the cost and the budget has generous headroom.
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
  // Delete + re-insert moves the key to the most-recently-used end.
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
  // Promote to most-recently-used so eviction is true LRU, not FIFO: a
  // frequently revisited file shouldn't be evicted ahead of a stale one just
  // because it was inserted earlier.
  contentsCache.delete(key);
  contentsCache.set(key, hit);
  return hit.value;
}

/** Test-only: reset the module-level cache between cases. */
export function __resetFileContentsCache() {
  contentsCache.clear();
  cacheBytes = 0;
}

/**
 * Fetch raw old/new file text for the contents-based (`@pierre/diffs`)
 * renderer. Caches by request identity so switching back to a file is instant.
 * On a switch that misses the cache it keeps the previous file's `contents`
 * painted and flips `loading=true`, so the viewer can scrim the stale diff
 * while the new one loads instead of flashing a blank screen. Only the very
 * first load (no prior `contents`) shows the full loading state.
 */
export function useFileContents(
  sessionId: string | null,
  filePath: string | null,
  /** Workspace repo name; undefined for single-repo sessions. See #1047. */
  repoName: string | undefined,
  /** Triggers a re-fetch when bumped (e.g. from useDiffFiles.revision). */
  externalRevision?: number,
): UseFileContentsResult {
  const key = sessionId && filePath ? cacheKeyFor(sessionId, filePath, repoName, externalRevision) : null;

  const [contents, setContents] = useState<RichFileContentsResponse | null>(() => (key ? cacheGet(key) : null));
  const [loading, setLoading] = useState(key != null && cacheGet(key) == null);
  const [error, setError] = useState<string | null>(null);
  const [handledKey, setHandledKey] = useState(key);
  const requestIdRef = useRef(0);

  // Sync transient state to the active file at render time (not in an effect).
  // A cache hit resolves instantly. A miss keeps the previous file's contents
  // painted (the viewer scrims them) and flips loading on; only a truly empty
  // viewer (no prior contents) falls through to the full loading screen.
  // In-flight requests for the previous file are dropped by the `reqId` guard
  // in `fetchContents` (and the deferred fetch below is cancelled outright on a
  // same-tick switch).
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
        // Keep stale contents under a loading scrim; don't blank the viewer.
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
      // Drop stale responses from rapid file/session switches.
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

  // Defer the fetch a tick so a same-tick switch (A -> B -> C) cancels the
  // superseded fetches via cleanup instead of firing them; this also keeps the
  // synchronous `setLoading` out of the effect body.
  useEffect(() => {
    const timer = setTimeout(() => {
      void fetchContents();
    }, 0);
    return () => clearTimeout(timer);
  }, [fetchContents]);

  const refresh = useCallback(() => fetchContents(true), [fetchContents]);

  return { contents, loading, error, refresh };
}
