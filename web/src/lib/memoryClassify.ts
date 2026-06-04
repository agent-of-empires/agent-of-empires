// Recognise Claude memory-system file ops so the structured view can render them
// as a dedicated MemoryCard rather than a generic Read/Edit/Write card.
//
// Memory lives under `~/.claude/projects/<slug>/memory/*.md` and is
// touched via the standard Read/Edit/Write tools, so the file path is
// the only reliable signal. See issue #1071.

import { parseJsonObject, pickFirst, pickStr } from "./acpArgs";
import type { ToolCall } from "./acpTypes";

export type MemoryVerb = "recalled" | "saved" | "updated";

export interface MemoryHit {
  isMemory: true;
  /** Full absolute path captured from the tool args. */
  path: string;
  /** Filename including extension, e.g. `feedback_testing.md`. */
  basename: string;
  /** Verb keyed off the underlying tool: Read/Edit/Write. */
  verb: MemoryVerb;
  /** True when the path's basename is `MEMORY.md`, the user-facing index. */
  isIndex: boolean;
}

export interface NotMemory {
  isMemory: false;
}

/** A path is a memory file when it sits inside Claude's per-project
 *  memory directory and ends with `.md`. The full segment match keeps
 *  unrelated paths that merely contain `memory` from triggering. */
export function isMemoryPath(path: string): boolean {
  if (!path.endsWith(".md")) return false;
  return /\/\.claude\/projects\/[^/]+\/memory\//.test(path);
}

function basenameOf(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash === -1 ? path : path.slice(slash + 1);
}

function verbFor(tool: ToolCall): MemoryVerb | null {
  const name = tool.name?.trim() ?? "";
  if (name === "Read" || tool.kind === "read") return "recalled";
  if (name === "Write") return "saved";
  if (name === "Edit" || name === "MultiEdit" || tool.kind === "edit") {
    return "updated";
  }
  return null;
}

export function classifyMemory(tool: ToolCall): MemoryHit | NotMemory {
  const args = parseJsonObject(tool.args_preview);
  const argPath = pickStr(args, "path", "file_path", "filePath", "filename");
  const path = pickFirst(argPath);
  if (!path || !isMemoryPath(path)) return { isMemory: false };
  const verb = verbFor(tool);
  if (!verb) return { isMemory: false };
  const basename = basenameOf(path);
  return {
    isMemory: true,
    path,
    basename,
    verb,
    isIndex: basename === "MEMORY.md",
  };
}

export interface ParsedMemory {
  name: string | null;
  description: string | null;
  type: string | null;
  body: string;
}

/** Lightweight frontmatter parser for memory files. Handles the
 *  `---`-delimited YAML-ish header documented in the memory-system
 *  prompt (`name`, `description`, `type`); any other fields are
 *  ignored. Fails soft: a file with no frontmatter (or malformed
 *  frontmatter) parses to a null header and the full text as body, so
 *  the card still renders something useful. */
export function parseMemoryFrontmatter(content: string): ParsedMemory {
  const empty: ParsedMemory = {
    name: null,
    description: null,
    type: null,
    body: content,
  };
  if (!content.startsWith("---")) return empty;
  const rest = content.slice(3);
  const newline = rest.indexOf("\n");
  if (newline === -1) return empty;
  const after = rest.slice(newline + 1);
  const closer = after.indexOf("\n---");
  if (closer === -1) return empty;
  const header = after.slice(0, closer);
  let body = after.slice(closer + 4);
  body = body.replace(/^\n+/, "");

  const fields: Record<string, string> = {};
  for (const line of header.split("\n")) {
    const m = line.match(/^([A-Za-z_][A-Za-z0-9_-]*)\s*:\s*(.*)$/);
    if (!m) continue;
    const key = m[1];
    const raw = m[2];
    if (!key || raw === undefined) continue;
    let value = raw.trim();
    if (
      value.length >= 2 &&
      ((value.startsWith('"') && value.endsWith('"')) ||
        (value.startsWith("'") && value.endsWith("'")))
    ) {
      value = value.slice(1, -1);
    }
    fields[key] = value;
  }

  return {
    name: fields.name ?? null,
    description: fields.description ?? null,
    type: fields.type ?? null,
    body,
  };
}
