import { useEffect, useState } from "react";
import { CommentMarkdown } from "./CommentMarkdown";
import type { DiffCommentsCardPayload } from "./buildPrompt";
import type { DiffComment } from "./types";
import { ensureThemeLoaded, getHighlighter, langKeyForExt, loadLanguage } from "../../../lib/highlighter";
import { useShikiTheme } from "../../../hooks/useShikiTheme";

interface Props {
  payload: DiffCommentsCardPayload;
}

/** Rich rendering of a diff-comments prompt in the structured view user-message
 *  slot. Built from the typed `UserDiffCommentsPrompt` event (carried on
 *  the assistant-ui message metadata) or, for legacy prompts, from the
 *  decoded sentinel payload. Falls back to raw text rendering upstream
 *  when neither is present. */
export function DiffCommentsUserCard({ payload }: Props) {
  const { intro, outro, isMultiRepo, comments } = payload;
  const sorted = [...comments].sort(compareComments);
  return (
    <div className="w-full max-w-3xl rounded-2xl rounded-br-sm border border-surface-700 bg-surface-800/70 px-4 py-3 text-sm">
      <div className="mb-2 flex items-center gap-2 text-[11px] uppercase tracking-wider text-text-dim">
        <span className="rounded bg-brand-600/15 px-1.5 py-0.5 font-mono text-brand-300">diff review</span>
        <span>
          {comments.length} comment{comments.length === 1 ? "" : "s"}
        </span>
      </div>
      {intro && (
        <div className="mb-3 border-l-2 border-surface-700 pl-3 text-text-secondary">
          <CommentMarkdown text={intro} />
        </div>
      )}
      <ul className="flex flex-col gap-3">
        {sorted.map((c) => (
          <li key={c.id} className="rounded-lg border border-surface-700/60 bg-surface-900/60">
            <CommentHeader comment={c} isMultiRepo={isMultiRepo} />
            <HighlightedSnippet code={c.capturedSnippet} language={c.language} filePath={c.filePath} />
            <div className="px-3 py-2 text-text-primary">
              <CommentMarkdown text={c.body} />
            </div>
          </li>
        ))}
      </ul>
      {outro && (
        <div className="mt-3 border-l-2 border-surface-700 pl-3 text-text-secondary">
          <CommentMarkdown text={outro} />
        </div>
      )}
    </div>
  );
}

/** Shiki-backed snippet renderer matching the structured view Markdown code
 *  block style. Loads the language module on demand and falls back to
 *  plain `<pre>` while loading or when the language can't be resolved.
 *  See `lib/highlighter.ts`. */
function HighlightedSnippet({ code, language, filePath }: { code: string; language?: string; filePath: string }) {
  const [html, setHtml] = useState<string | null>(null);
  const shiki = useShikiTheme();
  useEffect(() => {
    let cancelled = false;
    const hint = language && language.length > 0 ? language : (filePath.split(".").pop() ?? "");
    if (!hint) return;
    (async () => {
      try {
        const langKey = langKeyForExt(hint) ?? hint;
        await loadLanguage(langKey);
        const resolvedTheme = await ensureThemeLoaded(shiki.theme, shiki.appearance);
        const hl = await getHighlighter();
        if (cancelled) return;
        if (!hl.getLoadedLanguages().includes(langKey)) return;
        setHtml(hl.codeToHtml(code, { lang: langKey, theme: resolvedTheme }));
      } catch {
        // Unknown lang → fall through to plain rendering.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [code, language, filePath, shiki.theme, shiki.appearance]);

  if (html) {
    // Shiki HTML-escapes the user-supplied `code` before tokenizing, so
    // the only attacker-controlled values reach the DOM as text nodes
    // inside `<span>` tags with locally-generated style attributes.
    // Same trust boundary as the structured view Markdown renderer's code blocks.
    return (
      <div
        className="overflow-x-auto border-b border-surface-700/40 bg-surface-950 px-3 py-2 text-[12px] [&_pre]:!bg-transparent [&_pre]:!m-0 [&_pre]:!p-0"
        dangerouslySetInnerHTML={{ __html: html }}
      />
    );
  }
  return (
    <pre className="overflow-x-auto border-b border-surface-700/40 bg-surface-950 px-3 py-2 font-mono text-[12px] text-text-primary">
      {code}
    </pre>
  );
}

function CommentHeader({ comment, isMultiRepo }: { comment: DiffComment; isMultiRepo: boolean }) {
  const range =
    comment.startLine === comment.endLine
      ? `line ${comment.startLine}`
      : `lines ${comment.startLine}-${comment.endLine}`;
  return (
    <div className="flex flex-wrap items-center gap-1.5 border-b border-surface-700/40 px-3 py-1.5 text-[11px] font-mono text-text-dim">
      {isMultiRepo && comment.repoName && (
        <span className="rounded bg-surface-800 px-1.5 py-0.5 text-text-secondary">{comment.repoName}</span>
      )}
      <span className="text-text-secondary">{comment.filePath}</span>
      <span>·</span>
      <span>{range}</span>
      <span>·</span>
      <span>{comment.side}</span>
    </div>
  );
}

function compareComments(a: DiffComment, b: DiffComment): number {
  const ra = a.repoName ?? "";
  const rb = b.repoName ?? "";
  if (ra !== rb) return ra.localeCompare(rb);
  if (a.filePath !== b.filePath) return a.filePath.localeCompare(b.filePath);
  if (a.startLine !== b.startLine) return a.startLine - b.startLine;
  if (a.side !== b.side) return a.side === "old" ? -1 : 1;
  return a.createdAt.localeCompare(b.createdAt);
}
