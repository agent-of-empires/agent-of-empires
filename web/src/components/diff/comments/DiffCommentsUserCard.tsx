import { useEffect, useMemo, useState } from "react";
import { USER_REPLY_ITEM, UserReplyCard } from "../../acp/UserReplyCard";
import { CommentMarkdown } from "./CommentMarkdown";
import { buildDiffCommentsPrompt, compareComments, type DiffCommentsCardPayload } from "./buildPrompt";
import type { DiffComment } from "./types";
import { highlightSnippet } from "../../../lib/snippetHighlighter";
import { useShikiTheme } from "../../../hooks/useShikiTheme";

interface Props {
  payload: DiffCommentsCardPayload;
}

/** Diff-comments prompt card for the transcript, from the typed event or a legacy sentinel. */
export function DiffCommentsUserCard({ payload }: Props) {
  const { intro, outro, isMultiRepo, comments } = payload;
  const sorted = [...comments].sort(compareComments);
  // Rebuilt from the payload; it is the text the agent received.
  const sentPrompt = useMemo(
    () => buildDiffCommentsPrompt(comments, intro, outro, { isMultiRepo }).assembledMarkdown,
    [comments, intro, outro, isMultiRepo],
  );
  return (
    <UserReplyCard
      tag="diff review"
      summary={`${comments.length} comment${comments.length === 1 ? "" : "s"}`}
      copySource={sentPrompt}
      copyLabel="Copy prompt"
    >
      {intro && (
        <div className="mb-3 border-l-2 border-surface-700 pl-3 text-text-secondary">
          <CommentMarkdown text={intro} />
        </div>
      )}
      <ul className="flex flex-col gap-3">
        {sorted.map((c) => (
          <li key={c.id} className={USER_REPLY_ITEM}>
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
    </UserReplyCard>
  );
}

/** Shiki-highlighted snippet, plain `<pre>` while loading or for unknown languages. */
function HighlightedSnippet({ code, language, filePath }: { code: string; language?: string; filePath: string }) {
  // Keyed by the inputs that produced it, so a superseded request resolving
  // before its effect cleanup renders nothing. Theme is left out so a theme
  // switch keeps the old palette until the re-highlight lands. NUL-delimited
  // (written as an escape: a raw NUL makes git treat the file as binary) so
  // field concatenations cannot collide.
  const inputKey = `${code}\u0000${language ?? ""}\u0000${filePath}`;
  const [result, setResult] = useState<{ key: string; html: string } | null>(null);
  const shiki = useShikiTheme();

  useEffect(() => {
    let cancelled = false;
    const hint = language && language.length > 0 ? language : (filePath.split(".").pop() ?? "");
    if (!hint) return;
    (async () => {
      try {
        const out = await highlightSnippet(code, { langHint: hint, theme: shiki.theme, appearance: shiki.appearance });
        if (cancelled || !out) return;
        setResult({ key: inputKey, html: out });
      } catch {
        // Unknown language: keep plain rendering.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [code, language, filePath, inputKey, shiki.theme, shiki.appearance]);

  const html = result && result.key === inputKey ? result.html : null;

  if (html) {
    // Shiki escapes `code`; the HTML carries only locally generated styles.
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
