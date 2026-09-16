import { useEffect, useMemo, useState } from "react";
import { highlightSnippet } from "../../lib/snippetHighlighter";
import { useShikiTheme } from "../../hooks/useShikiTheme";
import { extensionToLanguage } from "./comments/language";

interface Props {
  /** Full file text to render. */
  content: string;
  /** File path, used to pick the syntax-highlighting grammar. */
  filePath: string;
}

/**
 * Full-file viewer for an agent-cited file that has no diff against the base
 * (#1810). Syntax-highlights the whole file with the shared shiki highlighter,
 * mirroring the markdown code-block renderer, and falls back to a plain `<pre>`
 * while the grammar loads or for unknown languages.
 */
export function FullFileViewer({ content, filePath }: Props) {
  const [html, setHtml] = useState<string | null>(null);
  const shiki = useShikiTheme();

  // Drop stale highlighted markup when the rendered input changes, so a switch
  // to an unknown-language or load-failing file can't keep painting the
  // previous file's html. Synced at render time (not in an effect) to satisfy
  // the set-state-in-effect lint, mirroring DiffFileViewer's syncKey pattern.
  // NUL-delimited (as the escape sequence: a raw NUL byte in source makes
  // git treat the file as binary) so field concatenations cannot collide.
  const inputKey = `${filePath}\u0000${content.length}`;
  const [handledKey, setHandledKey] = useState(inputKey);
  if (inputKey !== handledKey) {
    setHandledKey(inputKey);
    setHtml(null);
  }

  // One number per line, ignoring the empty segment after a trailing newline
  // so a POSIX-terminated file doesn't get a numbered phantom last line.
  const gutter = useMemo(() => {
    const lines = content.split("\n");
    if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
    return Array.from({ length: lines.length }, (_, i) => i + 1).join("\n");
  }, [content]);

  useEffect(() => {
    let cancelled = false;
    const lang = extensionToLanguage(filePath);
    if (!lang) return;
    (async () => {
      try {
        const out = await highlightSnippet(content, {
          langHint: lang,
          theme: shiki.theme,
          appearance: shiki.appearance,
        });
        if (cancelled) return;
        if (out) setHtml(out);
      } catch {
        // Unknown lang or load failure: keep the plain-text fallback.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [content, filePath, shiki.theme, shiki.appearance]);

  return (
    <div className="flex-1 min-h-0 overflow-auto">
      <div className="flex items-start min-w-full w-max">
        {/* leading-4 pins the gutter to the code's text-xs line height, so the
            numbers stay aligned even though they render at the diff pane's
            smaller gutter size. Sticky so horizontal scroll keeps them visible;
            aria-hidden + select-none so copying the code skips them. */}
        <pre
          aria-hidden="true"
          className="sticky left-0 shrink-0 w-[50px] py-2 pr-2 text-right font-mono text-[11px] leading-4 text-text-dim select-none border-r border-surface-700/30 bg-surface-900"
        >
          {gutter}
        </pre>
        {html ? (
          <div
            className="flex-1 px-3 py-2 text-xs [&_pre]:!bg-transparent [&_pre]:!m-0 [&_pre]:!p-0"
            dangerouslySetInnerHTML={{ __html: html }}
          />
        ) : (
          <pre className="flex-1 px-3 py-2 text-xs font-mono text-text-primary whitespace-pre">{content}</pre>
        )}
      </div>
    </div>
  );
}
