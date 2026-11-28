// Recognise an agent's memory file ops by the profile's `memory_path_regex`, for the MemoryCard.

import { parseJsonObject, pickFirst, pickStr } from "./acpArgs";
import type { ToolCall } from "./acpTypes";
import type { AgentProfile } from "./agentProfile";

export type MemoryVerb = "recalled" | "saved" | "updated";

export interface MemoryHit {
  isMemory: true;
  path: string;
  basename: string;
  verb: MemoryVerb;
  /** The `MEMORY.md` index. */
  isIndex: boolean;
}

export interface NotMemory {
  isMemory: false;
}

// Compiled once per profile object; null for no regex or an invalid one.
const compiled = new WeakMap<AgentProfile, RegExp | null>();

function memoryRegex(profile: AgentProfile): RegExp | null {
  if (compiled.has(profile)) return compiled.get(profile)!;
  let re: RegExp | null = null;
  if (profile.memory_path_regex) {
    try {
      re = new RegExp(profile.memory_path_regex);
    } catch {
      // An invalid regex means no memory files.
    }
  }
  compiled.set(profile, re);
  return re;
}

export function isMemoryPath(path: string, profile: AgentProfile): boolean {
  return memoryRegex(profile)?.test(path) ?? false;
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

export function classifyMemory(tool: ToolCall, profile: AgentProfile): MemoryHit | NotMemory {
  const args = parseJsonObject(tool.args_preview);
  const argPath = pickStr(args, "path", "file_path", "filePath", "filename");
  const path = pickFirst(argPath);
  if (!path || !isMemoryPath(path, profile)) return { isMemory: false };
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

/** `name`/`description`/`type` frontmatter; without a valid header the whole text is the body. */
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
      ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'")))
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

/** Strip the `<system-reminder>` envelope and tab-separated `cat -n` prefixes from recalled memory. */
export function cleanRecalledMemory(text: string): string {
  return text
    .replace(/<\/?system-reminder>/g, "")
    .replace(/^[ \t]*\d+\t/gm, "")
    .trim();
}
