// Shared helpers for parsing the JSON-shaped `args_preview` field on
// structured view tool calls (and other JSON blobs the structured view UI displays).
// The Rust side ships a string preview that's USUALLY a JSON object
// but sometimes truncated or non-object — these helpers handle both.

/** Parse a JSON object payload. Returns null when the input doesn't
 *  parse, isn't an object, or is an array (callers want
 *  field-by-field access, not array indexing). */
export function parseJsonObject(s: string): Record<string, unknown> | null {
  try {
    const v = JSON.parse(s);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/** Return the first key whose value is a string. Used to surface a
 *  tool's primary argument (path, command, query) when the agent
 *  uses different field names across versions. */
export function pickStr(o: Record<string, unknown> | null, ...keys: string[]): string | null {
  if (!o) return null;
  for (const k of keys) {
    const v = o[k];
    if (typeof v === "string") return v;
  }
  return null;
}

/** Return the first non-empty string in the chain, or null. */
export function pickFirst(...candidates: Array<string | null | undefined>): string | null {
  for (const c of candidates) {
    if (typeof c === "string" && c.trim() !== "") return c;
  }
  return null;
}

/** Derive a one-line preview from a tool call's `args_preview`, mirroring
 *  the per-card primary-arg extraction in ToolCards.tsx: command for
 *  execute, path for read/edit/delete, query/pattern for search, url for
 *  fetch, then the ACP-forwarded `_aoe_title`. Returns null when the
 *  payload carries no usable primary argument (e.g. an adapter that ships
 *  an empty `{}` for bash); callers fall back to the tool name. */
export function previewFromArgs(argsPreview: string): string | null {
  const args = parseJsonObject(argsPreview);
  return pickFirst(
    pickStr(args, "command", "cmd", "args"),
    pickStr(args, "path", "file_path", "filePath", "filename"),
    pickStr(args, "query", "pattern"),
    pickStr(args, "url"),
    pickStr(args, "_aoe_title"),
  );
}

/** Whether an `args_preview` has body content worth expanding: a
 *  non-object payload counts when non-blank; an object counts when it
 *  has at least one non-`_aoe_` key. Mirrors ArgsView's render gate so
 *  the approval card only shows an expand affordance when there is
 *  something behind it. */
export function hasArgsBody(argsPreview: string): boolean {
  const parsed = parseJsonObject(argsPreview);
  if (!parsed) return argsPreview.trim().length > 0;
  return Object.keys(parsed).some((k) => !k.startsWith("_aoe_"));
}

export interface TodoPayloadItem {
  content: string;
  status: unknown;
}

export function todoItemsFromArgs(args: Record<string, unknown> | null): TodoPayloadItem[] {
  const raw = args?.todos;
  if (!Array.isArray(raw)) return [];
  const todos: TodoPayloadItem[] = [];
  for (const entry of raw) {
    if (!entry || typeof entry !== "object" || Array.isArray(entry)) continue;
    const obj = entry as Record<string, unknown>;
    const content = typeof obj.content === "string" ? obj.content : "";
    if (content.trim() === "") continue;
    todos.push({ content, status: obj.status });
  }
  return todos;
}

export function hasTodoItemsArgsText(argsText: string): boolean {
  return todoItemsFromArgs(parseJsonObject(argsText)).length > 0;
}
