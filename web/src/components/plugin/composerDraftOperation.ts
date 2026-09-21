import type { PluginUiEntry } from "../../lib/api";
import { isObject, str } from "./slotPayload";

export type ComposerDraftOperation =
  | { kind: "insert-text"; text: string }
  | { kind: "replace-selection"; text: string }
  | { kind: "set-text"; text: string };

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
