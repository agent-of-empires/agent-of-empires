import { parseJsonObject } from "./acpArgs";
import type { DiffPreview } from "./acpTypes";

const isTextOrNull = (v: unknown) => v === undefined || v === null || typeof v === "string";

function isDiffPreview(v: unknown): v is DiffPreview {
  if (!v || typeof v !== "object") return false;
  const d = v as Record<string, unknown>;
  return typeof d.path === "string" && isTextOrNull(d.old_text) && isTextOrNull(d.new_text);
}

/** The structured diffs a tool call carries in `_aoe_diffs`, or undefined when absent or malformed. */
export function pickDiffs(
  args: Record<string, unknown> | undefined,
  argsText: string | undefined,
): DiffPreview[] | undefined {
  for (const o of [args, argsText ? parseJsonObject(argsText) : null]) {
    const diffs = o?._aoe_diffs;
    if (Array.isArray(diffs) && diffs.length > 0 && diffs.every(isDiffPreview)) return diffs;
  }
  return undefined;
}
