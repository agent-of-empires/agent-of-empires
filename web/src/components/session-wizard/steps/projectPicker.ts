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

// Recents shown with an empty query; a query searches the whole list.
const RECENT_CAP = 6;

function normalizePath(p: string): string {
  return p.replace(/\/+$/, "") || "/";
}

export function collectRecentProjects(sessions: SessionResponse[]): RecentProject[] {
  const map = new Map<string, RecentProject>();
  for (const s of sessions) {
    // Scratch dirs are deleted with their session, and a workspace cannot be rebuilt from one path.
    if (s.scratch || s.workspace_repos.length > 0) continue;
    // Trailing slashes are trimmed to match the backend's session dedup.
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

/** Adds persisted projects whose sessions are gone; session-derived entries win on a path collision. */
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

/** Drops recents that are also saved projects, so each path renders once. */
export function splitSavedAndRecent(
  saved: ProjectInfo[],
  recent: RecentProject[],
): { saved: ProjectInfo[]; recent: RecentProject[] } {
  const savedPaths = new Set(saved.map((s) => normalizePath(s.path)));
  return { saved, recent: recent.filter((r) => !savedPaths.has(normalizePath(r.path))) };
}

/** Saved and recent project lists with search, shared by ProjectStep and ExtraReposPicker. */
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
    // Unfiltered, so callers can tell "none registered" from "all excluded".
    hasAnyProjects: saved.length > 0 || recent.length > 0,
  };
}
