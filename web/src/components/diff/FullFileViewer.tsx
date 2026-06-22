import { useEffect, useState } from "react";
import { ensureThemeLoaded, getHighlighter, langKeyForExt, loadLanguage } from "../../lib/highlighter";
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

  useEffect(() => {
    let cancelled = false;
    const lang = extensionToLanguage(filePath);
    if (!lang) return;
    (async () => {
      try {
        const langKey = langKeyForExt(lang) ?? lang;
        await loadLanguage(langKey);
        const resolvedTheme = await ensureThemeLoaded(shiki.theme, shiki.appearance);
        const hl = await getHighlighter();
        if (cancelled) return;
        setHtml(hl.codeToHtml(content, { lang: langKey, theme: resolvedTheme }));
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
      {html ? (
        <div
          className="px-3 py-2 text-xs [&_pre]:!bg-transparent [&_pre]:!m-0 [&_pre]:!p-0"
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className="px-3 py-2 text-xs font-mono text-text-primary whitespace-pre">{content}</pre>
      )}
    </div>
  );
}
