// A thread message's parts as standalone markdown, for the message copy button.

import { parseJsonObject, pickStr } from "../../lib/acpArgs";
import type { ToolOutputBlock } from "../../lib/acpTypes";
import { diffPair } from "../../lib/diffPair";
import { pickDiffs } from "../../lib/diffPreview";
import { unifiedDiff } from "../../lib/unifiedDiff";
import { SUBAGENT_TASK_NAME, TODO_GROUP_NAME, TOOL_GROUP_NAME } from "./activityMessages";

/** The fields of an assistant-ui message part this reads; other part types are skipped. */
export interface MessagePart {
  type: string;
  text?: string;
  toolName?: string;
  argsText?: string;
  result?: unknown;
}

/** A fence longer than any backtick run in `text`, so the content cannot close it early. */
function fenced(text: string, info = ""): string {
  // A loop, not `Math.max(...runs)`: a large output can have more runs than a call accepts arguments.
  let longest = 0;
  for (const run of text.matchAll(/`+/g)) longest = Math.max(longest, run[0].length);
  const fence = "`".repeat(Math.max(3, longest + 1));
  return `${fence}${info}\n${text}\n${fence}`;
}

/** Drops leading blank lines and trailing whitespace, keeping the first line's indentation, which can mean a code block. */
const tidy = (text: string) => text.replace(/^(?:[ \t]*\r?\n)+/, "").trimEnd();

/** The synthetic group parts carry their cards in `argsText`; a plain tool call stands for itself. */
function flattenToolCalls(part: MessagePart): MessagePart[] {
  const grouped = [TOOL_GROUP_NAME, TODO_GROUP_NAME, SUBAGENT_TASK_NAME].includes(part.toolName ?? "");
  if (!grouped) return [part];
  const payload = part.argsText ? parseJsonObject(part.argsText) : null;
  const children = Array.isArray(payload?.children) ? (payload.children as MessagePart[]) : [];
  const parent = payload?.parent ? [payload.parent as MessagePart] : [];
  return [...parent, ...children];
}

/** The input without the `_aoe_` bookkeeping keys the tool cards hide too. */
function toolInput(argsText: string | undefined): string {
  const args = argsText ? parseJsonObject(argsText) : null;
  if (!args) return argsText?.trim() ?? "";
  const visible = Object.entries(args).filter(([key]) => !key.startsWith("_aoe_"));
  return visible.length > 0 ? JSON.stringify(Object.fromEntries(visible), null, 2) : "";
}

/**
 * The structured diffs a tool call carries, as unified diffs. A creation whose new text the visible input already
 * holds (a Write that passes its `content`) is skipped, and so is a diff that changes nothing.
 */
function diffBlocks(argsText: string | undefined): string[] {
  const diffs = pickDiffs(undefined, argsText);
  if (!diffs) return [];
  const args = parseJsonObject(argsText ?? "") ?? {};
  const shown = new Set(
    Object.entries(args).flatMap(([key, value]) =>
      !key.startsWith("_aoe_") && typeof value === "string" ? [value] : [],
    ),
  );
  return diffs.flatMap((d) => {
    // Only a creation: with old content, the removed lines would be lost.
    if (!d.old_text && d.new_text && shown.has(d.new_text)) return [];
    const { hunk } = diffPair(d.old_text ?? "", d.new_text ?? "");
    return hunk.lines.some((line) => line.type !== "equal") ? [fenced(unifiedDiff(d.path, hunk), "diff")] : [];
  });
}

/** A link destination markdown reads back whole: whitespace and angle brackets percent-encoded, backslashes and parentheses escaped. */
const linkDestination = (uri: string) =>
  uri.replace(/[\s<>]/g, (c) => encodeURIComponent(c)).replace(/[\\()]/g, "\\$&");

/** A link label on one line, with its brackets and backslashes escaped. */
const linkLabel = (text: string) => text.replace(/\s+/g, " ").replace(/[\\[\]]/g, "\\$&");

/** Inline code with a delimiter longer than any backtick run inside it. */
function codeSpan(text: string): string {
  let longest = 0;
  for (const run of text.matchAll(/`+/g)) longest = Math.max(longest, run[0].length);
  const tick = "`".repeat(longest + 1);
  const pad = text.startsWith("`") || text.endsWith("`") ? " " : "";
  return `${tick}${pad}${text}${pad}${tick}`;
}

/** One block of a tool's output as markdown. Inline bytes stay out of the clipboard. */
function mediaBlock(block: ToolOutputBlock): string {
  switch (block.kind) {
    case "resource_link":
      return `[${linkLabel(block.name)}](${linkDestination(block.uri)})`;
    case "resource":
      if (block.text != null) return `${codeSpan(block.uri)}\n\n${fenced(tidy(block.text))}`;
      return block.data
        ? `[resource: ${linkLabel(block.uri)}]`
        : `[${linkLabel(block.uri)}](${linkDestination(block.uri)})`;
    case "image":
      return !block.data && block.uri ? `![](${linkDestination(block.uri)})` : `[image: ${block.mime_type}]`;
    case "audio":
      return `[audio: ${block.mime_type}]`;
    case "text":
      return fenced(tidy(block.text));
  }
}

function toolBlock(call: MessagePart): string {
  const name = pickStr(call.argsText ? parseJsonObject(call.argsText) : null, "_aoe_title") ?? call.toolName ?? "tool";
  const input = toolInput(call.argsText);
  const result = call.result as { content?: unknown; output?: ToolOutputBlock[] } | null | undefined;
  const content = typeof result?.content === "string" ? result.content : "";
  const output = tidy(content);
  // A text block repeats `content`, except an embedded terminal's placeholder.
  const media = (result?.output ?? [])
    .filter((block) => block.kind !== "text" || !content.includes(block.text))
    .map(mediaBlock);
  return [
    `**${name}**`,
    input && fenced(input, "json"),
    ...diffBlocks(call.argsText),
    output && fenced(output),
    ...media,
  ]
    .filter(Boolean)
    .join("\n\n");
}

/** Text parts in order, blank-line separated; `includeTools` renders each tool call between them. */
export function messageToMarkdown(parts: readonly MessagePart[], { includeTools }: { includeTools: boolean }): string {
  const blocks: string[] = [];
  for (const part of parts) {
    if (part.type === "text") {
      const text = tidy(part.text ?? "");
      if (text) blocks.push(text);
    } else if (includeTools && part.type === "tool-call") {
      blocks.push(...flattenToolCalls(part).map(toolBlock));
    }
  }
  return blocks.join("\n\n");
}
