import { useEffect, useMemo, useState } from "react";
import { fetchSessions, fetchRecentProjects, fetchProjects } from "../../../lib/api";
import type { RecentProjectEntry } from "../../../lib/api";
import type { ProjectInfo, SessionResponse } from "../../../lib/types";

export interface RecentProject {
  path: string;
  displayName: string;
  lastAccessedAt: string | null;
  tool: string;
  sessionCount: number;
}

/** How many recents render when the search box is empty. The search itself is
 *  not capped; see `useProjectPicker`. */
const RECENT_CAP = 6;

function normalizePath(p: string): string {
  return p.replace(/\/+$/, "") || "/";
}

export function collectRecentProjects(sessions: SessionResponse[]): RecentProject[] {
  const map = new Map<string, RecentProject>();
  for (const s of sessions) {
    // Scratch sessions live in transient `<app_dir>/scratch/<id>/`
    // directories that get deleted with the session (unless the user opts
    // in to keeping the dir). They must not appear in the Recent list,
    // where they would be re-selectable as a project.
    if (s.scratch) continue;
    // Multi-repo workspaces collapse to a single `main_repo_path` here, so
    // picking one from Recent would start a plain single-repo session and
    // silently drop the other repos. The project step cannot reconstruct a
    // workspace from one path, so keep them out of the list entirely.
    if (s.workspace_repos.length > 0) continue;
    // Normalize the trailing slash before keying, mirroring the backend's
    // dedup convention (`src/session/instance/tmux_session.rs` is_duplicate_session and
    // `src/server/api/sessions/list.rs` workspace_id_for_session both
    // `trim_end_matches('/')`). Without this, `/foo/bar` and `/foo/bar/`
    // become two separate entries with split session counts. `normalizePath`
    // keeps the filesystem root from collapsing to an empty string.
    const raw = s.main_repo_path || s.project_path;
    if (!raw) continue;
    const path = normalizePath(raw);
    const existing = map.get(path);
    const ts = s.last_accessed_at ?? s.created_at ?? null;
    if (existing) {
      existing.sessionCount++;
      if ((ts ?? "") > (existing.lastAccessedAt ?? "")) {
        existing.lastAccessedAt = ts;
        existing.tool = s.tool;
      }
    } else {
      map.set(path, {
        path,
        displayName: path.split("/").filter(Boolean).pop() || path,
        lastAccessedAt: ts,
        tool: s.tool,
        sessionCount: 1,
      });
    }
  }
  return Array.from(map.values()).sort((a, b) => (b.lastAccessedAt ?? "").localeCompare(a.lastAccessedAt ?? ""));
}

// Fold the persisted recent-projects store (projects whose sessions are gone,
// #2141) into the live session-derived list. Session-derived entries win on a
// normalized-path collision, so an active project keeps its real session count
// and freshness; persisted-only projects are appended with a zero count. The
// merged list is sorted newest-first; the caller still slices to the visible
// cap.
export function mergeRecentProjects(sessionDerived: RecentProject[], persisted: RecentProjectEntry[]): RecentProject[] {
  const byPath = new Map<string, RecentProject>();
  for (const r of sessionDerived) byPath.set(r.path, r);
  for (const p of persisted) {
    const path = normalizePath(p.path);
    if (byPath.has(path)) continue;
    byPath.set(path, {
      path,
      displayName: p.display_name || path.split("/").filter(Boolean).pop() || path,
      lastAccessedAt: p.last_used_at,
      tool: p.tool,
      sessionCount: 0,
    });
  }
  return Array.from(byPath.values()).sort((a, b) => (b.lastAccessedAt ?? "").localeCompare(a.lastAccessedAt ?? ""));
}

/** Saved projects are a curated registry (#2140); recents are derived from
 *  live sessions and the persisted recent-projects store. A path can be in
 *  both. Drop it from recents so it renders once, in the Saved section.
 *  Path keys are normalized the same way the recents are (trailing slashes
 *  trimmed, root kept as "/") so `/foo/bar` and `/foo/bar/` match across the
 *  two sources. */
export function splitSavedAndRecent(
  saved: ProjectInfo[],
  recent: RecentProject[],
): { saved: ProjectInfo[]; recent: RecentProject[] } {
  const savedPaths = new Set(saved.map((s) => normalizePath(s.path)));
  return { saved, recent: recent.filter((r) => !savedPaths.has(normalizePath(r.path))) };
}

/** Fetches and filters the saved + recent project lists shared by the main
 *  Project step and the extra-repos picker (#3743), so both offer the same
 *  search-over-saved-and-recent experience instead of two divergent UIs. */
export function useProjectPicker(excludePaths: string[] = []) {
  const [recent, setRecent] = useState<RecentProject[]>([]);
  const [saved, setSaved] = useState<ProjectInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState("");

  useEffect(() => {
    let cancelled = false;
    Promise.all([fetchSessions(), fetchRecentProjects(), fetchProjects()]).then(
      ([envelope, recentEnvelope, savedProjects]) => {
        if (cancelled) return;
        const sessionDerived = envelope ? collectRecentProjects(envelope.sessions) : [];
        const merged = mergeRecentProjects(sessionDerived, recentEnvelope?.projects ?? []);
        const split = splitSavedAndRecent(savedProjects, merged);
        setSaved(split.saved);
        setRecent(split.recent);
        setLoading(false);
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  const excluded = useMemo(() => new Set(excludePaths.map(normalizePath)), [excludePaths]);
  const visibleSaved = useMemo(() => saved.filter((s) => !excluded.has(normalizePath(s.path))), [saved, excluded]);
  const visibleRecent = useMemo(() => recent.filter((r) => !excluded.has(normalizePath(r.path))), [recent, excluded]);

  // #3461: with no query, recents stay capped so the list reads as a short
  // "jump back in" list. A query searches the whole visible list instead, so
  // a project sitting below the cap is still reachable by typing.
  const filteredRecent = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return visibleRecent.slice(0, RECENT_CAP);
    return visibleRecent.filter((r) => r.path.toLowerCase().includes(q) || r.displayName.toLowerCase().includes(q));
  }, [visibleRecent, query]);

  const filteredSaved = useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return visibleSaved;
    return visibleSaved.filter((s) => s.path.toLowerCase().includes(q) || s.name.toLowerCase().includes(q));
  }, [visibleSaved, query]);

  return {
    loading,
    saved: visibleSaved,
    recent: visibleRecent,
    query,
    setQuery,
    filteredSaved,
    filteredRecent,
    hasPicks: visibleSaved.length > 0 || visibleRecent.length > 0,
  };
}
