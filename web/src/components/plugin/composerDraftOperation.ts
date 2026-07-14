import type { PluginUiEntry } from "../../lib/api";

export type ComposerDraftOperation =
  | { kind: "insert-text"; text: string }
  | { kind: "replace-selection"; text: string }
  | { kind: "set-text"; text: string };

function isObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

function str(obj: Record<string, unknown>, key: string): string | undefined {
  const v = obj[key];
  return typeof v === "string" ? v : undefined;
}

export function composerDraftOperation(entry: PluginUiEntry): { id: string; operation: ComposerDraftOperation } | null {
  const raw = entry.payload.draft_operation;
  if (!isObject(raw)) return null;
  const id = str(raw, "id");
  const text = str(raw, "text");
  const kind = str(raw, "kind");
  if (!id || text === undefined) return null;
  if (kind === "insert-text" || kind === "replace-selection" || kind === "set-text") {
    return { id, operation: { kind, text } };
  }
  return null;
}
