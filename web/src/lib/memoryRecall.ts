// Parse the `_aoe_memory_recall` payload AcpRuntime passes through tool-call args.

import type { MemoryRecall } from "./acpTypes";

/** Undefined for anything not shaped like a MemoryRecall, so the card never sees bad types. */
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

/** Parsed args first, raw `argsText` JSON as fallback. */
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
