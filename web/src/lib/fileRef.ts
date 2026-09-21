// Parsing and resolution for local file references in structured view markdown, e.g. `[app.ts](/repo/src/app.ts:42)`.

/** `line` and `column` are 1-based; only `line` is used (to scroll the diff viewer). */
export interface FileRef {
  path: string;
  line?: number;
  column?: number;
}

export interface FileRefSession {
  id: string;
  project_path: string;
  main_repo_path: string | null;
  workspace_repos: { name: string; source_path: string }[];
  artifact_dir?: string | null;
}

// Mirrors CONTAINER_ARTIFACT_DIR in src/session/artifacts.rs.
const CONTAINER_ARTIFACT_DIR = "/aoe/artifacts";

/** The artifact route for a path under the sandbox mount or the host artifact dir, else null. */
export function resolveArtifactUrl(rawPath: string, session: FileRefSession): string | null {
  const target = rawPath.replace(/\\/g, "/");
  const roots = [CONTAINER_ARTIFACT_DIR];
  if (session.artifact_dir) roots.push(session.artifact_dir.replace(/\\/g, "/"));
  for (const root of roots) {
    const base = root.replace(/\/+$/, "");
    if (base && target.startsWith(`${base}/`)) {
      const rel = target.slice(base.length + 1);
      if (!rel) return null;
      // Refuse `..`: the browser would collapse the URL outside /artifacts/.
      const segments = rel.split("/").filter((seg) => seg !== "." && seg !== "");
      if (segments.length === 0 || segments.some((seg) => seg === "..")) return null;
      const encoded = segments.map((seg) => encodeURIComponent(seg)).join("/");
      return `/api/sessions/${encodeURIComponent(session.id)}/artifacts/${encoded}`;
    }
  }
  return null;
}

// Web/app schemes keep normal anchor behavior.
const NON_FILE_SCHEME = /^(?:https?|mailto|tel|data|javascript|ftp|vscode|vscode-insiders|blob):/i;

/** Null for anything that is not a local file path. Strips `:line`, `:line:col`, or `#Lline` into `line`/`column`. */
export function parseFileRef(href: string): FileRef | null {
  if (!href) return null;
  const trimmed = href.trim();
  if (!trimmed) return null;

  if (NON_FILE_SCHEME.test(trimmed)) return null;
  if (trimmed.startsWith("//")) return null;
  if (trimmed.startsWith("#")) return null;

  let raw = trimmed;

  // `file:///C:/foo` becomes `C:/foo`.
  if (/^file:\/\//i.test(raw)) {
    raw = raw.replace(/^file:\/\//i, "");
    if (/^\/[a-zA-Z]:[/\\]/.test(raw)) raw = raw.slice(1);
  }

  try {
    raw = decodeURIComponent(raw);
  } catch {
    // Malformed percent-encoding: keep the raw string.
  }

  let path = raw;
  let line: number | undefined;
  let column: number | undefined;

  const hashMatch = path.match(/#L(\d+)$/);
  if (hashMatch) {
    path = path.slice(0, -hashMatch[0].length);
    line = Number(hashMatch[1]);
  } else {
    // Don't mistake a bare drive colon (`C:`) for a line suffix.
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

  path = path.replace(/\\/g, "/");
  if (!path) return null;

  return { path, line, column };
}

// Forward slashes, trailing slash, and a lowercased drive letter; POSIX paths stay case-sensitive.
function normalizePathForMatch(p: string): string {
  return p.replace(/\\/g, "/").replace(/^([a-zA-Z]):\//, (_, d) => `${d.toLowerCase()}:/`);
}

function normalizeRoot(root: string): string {
  const norm = normalizePathForMatch(root);
  return norm.endsWith("/") ? norm : `${norm}/`;
}

/** The module's single absolute-path predicate (POSIX or Windows drive). */
export function isAbsolutePath(path: string): boolean {
  return path.startsWith("/") || /^[a-zA-Z]:[\\/]/.test(path);
}

/** Repo-relative path (and repo name) for the diff/file API, or null outside every repo root. Relative paths pass
 *  through; absolute ones match workspace repos, then `project_path`, then `main_repo_path`, slash-guarded. */
export function resolveToRepoRelative(
  path: string,
  session: FileRefSession,
): { relativePath: string; repoName?: string } | null {
  const target = normalizePathForMatch(path);

  if (!isAbsolutePath(target)) {
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

/** Strip the session's repo root for display, prefixing the repo name in multi-repo workspaces. */
export function relativeDisplayPath(raw: string, session: FileRefSession | null | undefined): string {
  if (!session || !raw) return raw;
  const resolved = resolveToRepoRelative(raw, session);
  if (!resolved) return raw;
  return resolved.repoName ? `${resolved.repoName}/${resolved.relativePath}` : resolved.relativePath;
}
