// Helpers for tool-call `args_preview`, usually but not always a JSON object.

/** Null unless the input parses to a non-array object. */
export function parseJsonObject(s: string): Record<string, unknown> | null {
  try {
    const v = JSON.parse(s);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/** First string-valued key, since agents name the primary argument differently across versions. */
export function pickStr(o: Record<string, unknown> | null, ...keys: string[]): string | null {
  if (!o) return null;
  for (const k of keys) {
    const v = o[k];
    if (typeof v === "string") return v;
  }
  return null;
}

export function pickFirst(...candidates: Array<string | null | undefined>): string | null {
  for (const c of candidates) {
    if (typeof c === "string" && c.trim() !== "") return c;
  }
  return null;
}

/** One-line primary argument, mirroring ToolCards.tsx; null when the payload has none. */
export function previewFromArgs(argsPreview: string): string | null {
  const args = parseJsonObject(argsPreview);
  return pickFirst(
    pickStr(args, "command", "cmd", "args"),
    pickStr(args, "path", "file_path", "filePath", "filepath", "filename"),
    pickStr(args, "query", "pattern"),
    pickStr(args, "url"),
    pickStr(args, "_aoe_title"),
  );
}

/** Mirrors ArgsView's render gate: non-blank text, or an object with a non-`_aoe_` key. */
export function hasArgsBody(argsPreview: string): boolean {
  const parsed = parseJsonObject(argsPreview);
  if (!parsed) return argsPreview.trim().length > 0;
  return Object.keys(parsed).some((k) => !k.startsWith("_aoe_"));
}

/** Labels for ACP permission kinds some agents send as titles. Unknown titles pass through untouched rather than being auto-cased. */
const PERMISSION_TITLE_LABELS: Record<string, string> = {
  external_directory: "External directory access",
};

export const humanizePermissionTitle = (title: string): string => PERMISSION_TITLE_LABELS[title] ?? title;

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

/** An empty `todos: []` is a real clear-list snapshot, so array presence is the discriminator. */
export function hasTodoArrayArgsText(argsText: string): boolean {
  return Array.isArray(parseJsonObject(argsText)?.todos);
}
