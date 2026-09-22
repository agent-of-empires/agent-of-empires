// Extract the failure reason from a tool_error row, peeling a `<tool_use_error>`-style wrapper into a separate label.

export interface ParsedToolError {
  /** Empty when the adapter sent `is_error: true` with no body. */
  body: string;
  /** Wrapper tag name, or null for a bare string. */
  tag: string | null;
}

// Non-anchored and non-greedy: adapters join text blocks around the wrapper, and prose outside it is formatting noise.
const WRAPPER_RE = /<([a-zA-Z_][a-zA-Z0-9_-]*)>([\s\S]*?)<\/\1>/;

export function parseToolError(text: string | undefined | null): ParsedToolError {
  const raw = (text ?? "").trim();
  if (!raw) return { body: "", tag: null };
  const m = WRAPPER_RE.exec(raw);
  if (m && m[1] && m[2] !== undefined) {
    return { body: m[2].trim(), tag: m[1] };
  }
  return { body: raw, tag: null };
}

/** Friendly labels for common tags; unknown tags pass through. */
export function describeToolErrorTag(tag: string | null): string | null {
  if (!tag) return null;
  switch (tag.toLowerCase()) {
    case "tool_use_error":
      return "agent-reported error";
    case "tool_result_error":
      return "agent-reported error";
    case "error":
      return "error";
    default:
      return tag;
  }
}
