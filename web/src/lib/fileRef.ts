// Parsing and resolution for local file references that agents emit in
// structured view transcript markdown. Codex (and similar agents) cite source
// locations as markdown links whose href is a filesystem path with an
// optional line/column suffix, e.g. `[app.ts](/Users/me/repo/src/app.ts:42)`.
// We intercept those in the markdown anchor override (see Markdown.tsx)
// and open the file in the in-app diff viewer instead of letting the
// browser navigate to a dead filesystem URL. External links (http(s),
// mailto, ...) are left untouched. See #1718.

/** A parsed local file reference. `line`/`column` are 1-based when the
 *  href carried a suffix. `line` is threaded through to the diff viewer to
 *  scroll the cited line into view (#1809); `column` is parsed but unused. */
export interface FileRef {
  path: string;
  line?: number;
  column?: number;
}

/** Minimal session shape needed to resolve an absolute agent path back
 *  to a repo-relative path. Mirrors the relevant fields of
 *  `SessionResponse`; kept structural so the pure resolver stays
 *  testable without constructing a full session object. */
export interface FileRefSession {
  id: string;
  project_path: string;
  main_repo_path: string | null;
  workspace_repos: { name: string; source_path: string }[];
  /** Absolute host path of the session's managed artifact dir. Agent paths
   *  under this root (or the fixed sandbox mount) map to the artifact route. */
  artifact_dir?: string | null;
}

// Fixed mount point for the session artifact dir inside a sandbox container.
// Mirrors CONTAINER_ARTIFACT_DIR in src/session/artifacts.rs: a sandboxed
// agent writes here, a local agent writes to `session.artifact_dir`.
const CONTAINER_ARTIFACT_DIR = "/aoe/artifacts";

/**
 * Map an agent-emitted artifact path to the authenticated artifact route
 * (`/api/sessions/<id>/artifacts/<rel>`), or null when the path is not under
 * a known artifact root. The two roots are the fixed sandbox mount and the
 * session's host artifact dir; a path under either belongs to this session's
 * served artifacts. See #2587.
 */
export function resolveArtifactUrl(rawPath: string, session: FileRefSession): string | null {
  const target = rawPath.replace(/\\/g, "/");
  const roots = [CONTAINER_ARTIFACT_DIR];
  if (session.artifact_dir) roots.push(session.artifact_dir.replace(/\\/g, "/"));
  for (const root of roots) {
    const base = root.replace(/\/+$/, "");
    if (base && target.startsWith(`${base}/`)) {
      const rel = target.slice(base.length + 1);
      if (!rel) return null;
      // Drop `.`/empty segments and refuse any `..`: an unnormalized dot path
      // would produce a URL the browser silently collapses outside /artifacts/,
      // giving a broken (or unexpected) link. The server route also confines,
      // but normalizing here keeps the emitted URL honest.
      const segments = rel.split("/").filter((seg) => seg !== "." && seg !== "");
      if (segments.length === 0 || segments.some((seg) => seg === "..")) return null;
      const encoded = segments.map((seg) => encodeURIComponent(seg)).join("/");
      return `/api/sessions/${encodeURIComponent(session.id)}/artifacts/${encoded}`;
    }
  }
  return null;
}

// Web/app URL schemes that are never local file references. Anything
// matching here keeps default (new-tab) anchor behavior.
const NON_FILE_SCHEME = /^(?:https?|mailto|tel|data|javascript|ftp|vscode|vscode-insiders|blob):/i;

/**
 * Classify an anchor href as a local file reference and split off any
 * trailing line/column suffix. Returns null for anything that is not a
 * local file path (external URLs, in-page anchors, protocol-relative
 * links), in which case the caller should fall back to normal anchor
 * behavior.
 *
 * Recognised suffixes: `:line`, `:line:col`, and `#Lline` (the GitHub
 * blob style). The suffix is stripped from `path` and surfaced as
 * `line`/`column`.
 */
export function parseFileRef(href: string): FileRef | null {
  if (!href) return null;
  const trimmed = href.trim();
  if (!trimmed) return null;

  // External/app schemes, protocol-relative `//host`, and bare in-page
  // `#anchor` links are not file references.
  if (NON_FILE_SCHEME.test(trimmed)) return null;
  if (trimmed.startsWith("//")) return null;
  if (trimmed.startsWith("#")) return null;

  let raw = trimmed;

  // `file://` URIs: strip the scheme. On Windows these come through as
  // `file:///C:/foo` -> `/C:/foo`; drop the leading slash so the drive
  // letter leads.
  if (/^file:\/\//i.test(raw)) {
    raw = raw.replace(/^file:\/\//i, "");
    if (/^\/[a-zA-Z]:[/\\]/.test(raw)) raw = raw.slice(1);
  }

  try {
    raw = decodeURIComponent(raw);
  } catch {
    // Malformed percent-encoding: fall through with the raw string.
  }

  let path = raw;
  let line: number | undefined;
  let column: number | undefined;

  // `#Lline` (GitHub blob fragment).
  const hashMatch = path.match(/#L(\d+)$/);
  if (hashMatch) {
    path = path.slice(0, -hashMatch[0].length);
    line = Number(hashMatch[1]);
  } else {
    // `:line` or `:line:col`. Guard against eating a bare Windows drive
    // colon (`C:`) by requiring the stripped remainder to be more than a
    // drive letter.
    const colonMatch = path.match(/:(\d+)(?::(\d+))?$/);
    if (colonMatch) {
      const stripped = path.slice(0, -colonMatch[0].length);
      if (stripped && !/^[a-zA-Z]:$/.test(stripped)) {
        path = stripped;
        line = Number(colonMatch[1]);
        if (colonMatch[2] !== undefined) column = Number(colonMatch[2]);
      }
    }
  }

  // Normalize separators so downstream prefix matching is uniform.
  path = path.replace(/\\/g, "/");
  if (!path) return null;

  return { path, line, column };
}

// Forward-slash separators, a trailing slash, and a lowercased Windows
// drive letter so prefix matching is uniform. Drive-letter case is the
// only case folding applied: POSIX paths stay case-sensitive, and the
// drive prefix is stripped from the returned relative path anyway, so
// folding it never leaks into output.
function normalizePathForMatch(p: string): string {
  return p.replace(/\\/g, "/").replace(/^([a-zA-Z]):\//, (_, d) => `${d.toLowerCase()}:/`);
}

function normalizeRoot(root: string): string {
  const norm = normalizePathForMatch(root);
  return norm.endsWith("/") ? norm : `${norm}/`;
}

/**
 * Resolve a parsed file path to the repo-relative path (and repo name
 * for multi-repo workspaces) expected by the diff/file API. Returns null
 * when the path is not inside any known repo root, so the caller can
 * surface a non-destructive toast instead of opening the wrong file.
 *
 * A relative path (no leading slash, no Windows drive) is assumed to be
 * already repo-relative and returned as-is. Absolute paths are matched
 * by exact, trailing-slash-guarded prefix against the workspace repo
 * roots, then the session worktree (`project_path`), then the source
 * repo (`main_repo_path`). The trailing-slash guard prevents
 * `/a/app` from spuriously matching `/a/app_old`.
 */
export function resolveToRepoRelative(
  path: string,
  session: FileRefSession,
): { relativePath: string; repoName?: string } | null {
  const target = normalizePathForMatch(path);

  const isAbsolute = target.startsWith("/") || /^[a-z]:\//.test(target);
  if (!isAbsolute) {
    // Already repo-relative; strip a leading `./` for cleanliness.
    const rel = target.replace(/^\.\//, "");
    return rel ? { relativePath: rel } : null;
  }

  for (const repo of session.workspace_repos) {
    const root = normalizeRoot(repo.source_path);
    if (target.startsWith(root)) {
      return { relativePath: target.slice(root.length), repoName: repo.name };
    }
  }

  const root = normalizeRoot(session.project_path);
  if (target.startsWith(root)) {
    return { relativePath: target.slice(root.length) };
  }

  if (session.main_repo_path) {
    const mainRoot = normalizeRoot(session.main_repo_path);
    if (target.startsWith(mainRoot)) {
      return { relativePath: target.slice(mainRoot.length) };
    }
  }

  return null;
}

/**
 * Display form of a tool-card file path: strip the session's repo root so
 * an edit of `/Users/me/wt/src/hooks/mod.rs` shows `src/hooks/mod.rs`. In a
 * multi-repo workspace the repo name is prefixed (`api/src/h.ts`) so paths
 * from different repos stay unambiguous. Falls back to the raw path when
 * there is no session or the path is outside every known root (e.g.
 * `/etc/hosts`), so a path is never silently mangled. See #2143.
 */
export function relativeDisplayPath(raw: string, session: FileRefSession | null | undefined): string {
  if (!session || !raw) return raw;
  const resolved = resolveToRepoRelative(raw, session);
  if (!resolved) return raw;
  return resolved.repoName ? `${resolved.repoName}/${resolved.relativePath}` : resolved.relativePath;
}
