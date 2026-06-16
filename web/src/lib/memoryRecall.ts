// Read and validate the `_aoe_memory_recall` payload AcpRuntime smuggles
// through an assistant-ui tool-call part's args, so the structured view can
// rebuild it onto the reconstructed ToolCall and dispatch to
// MemoryRecallCard. Kept out of the React component so the parsing /
// validation logic is unit-testable in isolation. See #2142.

import type { MemoryRecall } from "./acpTypes";

/** Validate and normalize a smuggled `_aoe_memory_recall` value before it
 *  reaches MemoryRecallCard. A malformed payload (e.g. non-string
 *  `synthesized_text`) would otherwise trigger runtime type errors in the
 *  card. Returns undefined for anything that isn't shaped like a
 *  MemoryRecall. */
export function asMemoryRecall(value: unknown): MemoryRecall | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
  const obj = value as Record<string, unknown>;
  if (typeof obj.mode !== "string") return undefined;
  const paths =
    Array.isArray(obj.paths) && obj.paths.every((p) => typeof p === "string") ? (obj.paths as string[]) : undefined;
  const synthesized_text = typeof obj.synthesized_text === "string" ? obj.synthesized_text : undefined;
  return {
    mode: obj.mode,
    ...(paths ? { paths } : {}),
    ...(synthesized_text !== undefined ? { synthesized_text } : {}),
  };
}

/** Read the smuggled `_aoe_memory_recall` payload off the tool-call args
 *  (parsed object first, raw `argsText` JSON as fallback). Returns
 *  undefined when absent or malformed; the card then renders as a generic
 *  read. */
export function pickMemoryRecall(
  args: Record<string, unknown> | undefined,
  argsText: string | undefined,
): MemoryRecall | undefined {
  const fromObj = asMemoryRecall(args?._aoe_memory_recall);
  if (fromObj) return fromObj;
  if (argsText) {
    try {
      const parsed = JSON.parse(argsText) as Record<string, unknown>;
      const mr = asMemoryRecall(parsed?._aoe_memory_recall);
      if (mr) return mr;
    } catch {
      // ignore
    }
  }
  return undefined;
}
