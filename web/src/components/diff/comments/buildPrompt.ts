import type { DiffComment } from "./types";
import { isWellFormed } from "./storage";

interface BuildOpts {
  /** Prefix each heading with `[repoName]`. */
  isMultiRepo: boolean;
}

const DEFAULT_OUTRO = "Please address these comments.";

const SENTINEL_PREFIX = "<!-- aoe:diff-comments:v1 ";
const SENTINEL_SUFFIX = " -->";

/** Fields the transcript needs to render `DiffCommentsUserCard`. */
export interface DiffCommentsCardPayload {
  intro: string;
  outro: string;
  isMultiRepo: boolean;
  comments: DiffComment[];
}

/** Message metadata arrives untyped; a malformed payload falls back to plain text. */
export function isDiffCommentsCardPayload(value: unknown): value is DiffCommentsCardPayload {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  return (
    typeof v.intro === "string" &&
    typeof v.outro === "string" &&
    typeof v.isMultiRepo === "boolean" &&
    Array.isArray(v.comments)
  );
}

/** The payload plus the exact text sent to the agent, shared by preview, POST and card. */
export interface BuiltDiffCommentsPrompt extends DiffCommentsCardPayload {
  assembledMarkdown: string;
}

/** Decodes a legacy prompt that starts with the base64 sentinel; null otherwise or when malformed. */
export function parseDiffCommentsSentinel(text: string): DiffCommentsCardPayload | null {
  if (!text.startsWith(SENTINEL_PREFIX)) return null;
  const end = text.indexOf(SENTINEL_SUFFIX, SENTINEL_PREFIX.length);
  if (end < 0) return null;
  try {
    const bin = atob(text.slice(SENTINEL_PREFIX.length, end));
    const bytes = Uint8Array.from(bin, (ch) => ch.charCodeAt(0));
    const parsed = JSON.parse(new TextDecoder().decode(bytes)) as unknown;
    if (!isDiffCommentsCardPayload(parsed)) return null;
    // Drop malformed entries rather than crash the card.
    return {
      intro: parsed.intro,
      outro: parsed.outro,
      isMultiRepo: parsed.isMultiRepo,
      comments: parsed.comments.filter(isWellFormed),
    };
  } catch {
    return null;
  }
}

/** Sorted comment sections, each with a fence longer than any backtick run in its snippet. */
export function buildCommentsMarkdown(comments: DiffComment[], opts: BuildOpts): string {
  return [...comments]
    .sort(compareComments)
    .map((c) => renderComment(c, opts.isMultiRepo))
    .join("\n\n---\n\n");
}

/** A blank outro falls back to a default; the returned intro/outro are the effective values. */
export function buildDiffCommentsPrompt(
  comments: DiffComment[],
  intro: string,
  outro: string,
  opts: BuildOpts,
): BuiltDiffCommentsPrompt {
  const introText = intro.trim();
  const outroText = (outro.trim() || DEFAULT_OUTRO).trim();
  const sections: string[] = [];
  if (introText) sections.push(introText);
  const commentsBlock = buildCommentsMarkdown(comments, opts);
  if (commentsBlock) sections.push("## Diff comments", commentsBlock);
  sections.push(outroText);
  return {
    intro: introText,
    outro: outroText,
    isMultiRepo: opts.isMultiRepo,
    comments,
    assembledMarkdown: sections.join("\n\n") + "\n",
  };
}

function renderComment(c: DiffComment, isMultiRepo: boolean): string {
  const repo = isMultiRepo && c.repoName ? `[${c.repoName}] ` : "";
  const range = c.startLine === c.endLine ? `line ${c.startLine}` : `lines ${c.startLine}-${c.endLine}`;
  const longestTicks = Math.max(0, ...(c.capturedSnippet.match(/`+/g) ?? []).map((m) => m.length));
  const fence = "`".repeat(Math.max(3, longestTicks + 1));
  const codeBlock = `${fence}${c.language ?? ""}\n${c.capturedSnippet}\n${fence}`;
  return `### ${repo}\`${c.filePath}\` ${range} (${c.side})\n\n${codeBlock}\n\n${c.body.trim()}`;
}

export function compareComments(a: DiffComment, b: DiffComment): number {
  const ra = a.repoName ?? "";
  const rb = b.repoName ?? "";
  if (ra !== rb) return ra.localeCompare(rb);
  if (a.filePath !== b.filePath) return a.filePath.localeCompare(b.filePath);
  if (a.startLine !== b.startLine) return a.startLine - b.startLine;
  if (a.side !== b.side) return a.side === "old" ? -1 : 1;
  return a.createdAt.localeCompare(b.createdAt);
}
