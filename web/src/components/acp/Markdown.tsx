/* eslint-disable react-refresh/only-export-components */
// Markdown renderer for agent text. Thin wrapper around
// @assistant-ui/react-markdown's MarkdownTextPrimitive; we just plug
// in our shiki-based SyntaxHighlighter and a CodeHeader that matches
// the rest of the dashboard's styling.
//
// The primitive handles:
//   - Streaming-aware rendering (incomplete fenced code blocks during
//     streaming, partial paragraphs, etc.)
//   - Smooth char-budget reveal (built-in `smooth` prop, defaults true)
//   - Standard markdown: paragraphs, lists, headings, links, tables
//
// We previously hand-rolled all of this (~200 lines plus a custom
// useStreamReveal hook). The primitive replaces both.

import { MarkdownTextPrimitive } from "@assistant-ui/react-markdown";
import type { CodeHeaderProps, SyntaxHighlighterProps } from "@assistant-ui/react-markdown";
import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

import { ensureThemeLoaded, getHighlighter, langKeyForExt, loadLanguage } from "../../lib/highlighter";
import { useShikiTheme } from "../../hooks/useShikiTheme";
import { parseFileRef, resolveArtifactUrl, resolveToRepoRelative } from "../../lib/fileRef";
import { useAcpFileRef } from "./AcpFileRefContext";
import { openArtifactInNewTab } from "../../lib/artifacts";
import { ArtifactImage } from "./artifactMedia";

interface Props {
  text: string;
  /** Enable the char-budget reveal that paces in newly-streamed
   *  agent tokens. Default off: historical messages (loaded from the
   *  per-session persisted cache on reload, or hydrated from server
   *  replay on session switch) would otherwise type out character-by-
   *  character, which on a long transcript becomes 5-15 seconds of
   *  unusable UI. Only the live streaming tail (an assistant message
   *  whose runtime status is `running`) should pass `smooth={true}`.
   *  See #1132. */
  smooth?: boolean;
  /** Treat a single newline as a hard line break (remark-breaks). The
   *  structured view composer is a plain <textarea>, so a lone shift+enter shows
   *  as a visible break while typing; enabling this keeps the sent user
   *  bubble matching that layout. Default off so assistant text keeps
   *  CommonMark soft-break semantics: model-authored markdown is often
   *  hard-wrapped and streamed, and turning every wrap into a <br> would
   *  add jarring mid-sentence breaks and streaming reflow. See #1472. */
  breaks?: boolean;
}

/** remark plugin chain for the structured view markdown surface. `breaks` opts
 *  single newlines into hard <br> breaks; see the `breaks` prop on
 *  {@link Markdown}. Exported so the regression tests exercise the exact
 *  chain the component mounts. */
export function remarkPluginsFor(breaks: boolean) {
  return breaks ? [remarkGfm, remarkBreaks] : [remarkGfm];
}

/**
 * Render markdown text. Used for both assistant chunks and user
 * prompts; the smoothing pace and single-newline handling are the knobs
 * exposed.
 */
export function Markdown({ text, smooth = false, breaks = false }: Props) {
  const remarkPlugins = useMemo(() => remarkPluginsFor(breaks), [breaks]);
  return (
    <MarkdownTextPrimitive
      preprocess={() => text}
      smooth={smooth}
      remarkPlugins={remarkPlugins}
      className="acp-markdown text-sm leading-relaxed"
      components={{
        SyntaxHighlighter: ShikiSyntaxHighlighter,
        CodeHeader,
        table: TableWithScroll,
        blockquote: Blockquote,
        a: TranscriptLink,
        img: TranscriptImage,
      }}
    />
  );
}

/**
 * Transcript link. Three behaviors:
 *
 *  - Local file references (e.g. Codex's `[app.ts](/repo/src/app.ts:42)`)
 *    that resolve to a known repo root are intercepted: clicking opens the
 *    file in the in-app diff/file viewer via the structured view file-ref
 *    handler, keeping the current `/session/<id>` route instead of
 *    navigating the tab to a dead filesystem path. See #1718.
 *  - A local file reference that resolves to no known repo root cannot be
 *    opened in the dashboard: clicking it either dead-ends in a "not inside
 *    this session's repo" toast or routes the tab to the SPA. Render it as
 *    inert, selectable text rather than a link that lies about being
 *    openable. Only decidable when a session is present to resolve against;
 *    without one we keep the interception fallback. See #2587.
 *  - Everything else (docs, CI, repo links) keeps the same-tab-is-bad
 *    treatment from #1714: open in a new tab with the dashboard-standard
 *    safe rel (guards against tabnabbing), so following a link does not
 *    replace the live structured view session.
 *
 * Existing anchor props (href, title, className, children) are forwarded
 * untouched, and the `target`/`rel` fallback is preserved so a
 * non-intercepted link (or a middle-click / "open in new tab") still
 * behaves as before.
 */
function TranscriptLink({ href, onClick, children, ...rest }: React.ComponentPropsWithoutRef<"a">) {
  const { onOpenFileRef, fileRefSession } = useAcpFileRef();
  const ref = href ? parseFileRef(href) : null;
  const artifactUrl = ref && fileRefSession ? resolveArtifactUrl(ref.path, fileRefSession) : null;

  // A managed session artifact: openable via the authenticated route. Fetch
  // it through the authed global fetch and open the blob so it works in
  // token-auth mode where a bare new-tab navigation would miss the header.
  if (artifactUrl) {
    return (
      <a
        {...rest}
        href={artifactUrl}
        className="acp-artifact-link"
        onClick={(e) => {
          e.preventDefault();
          void openArtifactInNewTab(artifactUrl);
        }}
      >
        {children}
      </a>
    );
  }

  if (ref && fileRefSession && !resolveToRepoRelative(ref.path, fileRefSession)) {
    return <span className="acp-inert-path">{children}</span>;
  }

  function handleClick(e: React.MouseEvent<HTMLAnchorElement>) {
    if (ref && onOpenFileRef) {
      e.preventDefault();
      onOpenFileRef(ref);
      return;
    }
    onClick?.(e);
  }

  return (
    <a {...rest} href={href} onClick={handleClick} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  );
}

/**
 * Transcript image. An agent may embed a screenshot with markdown image
 * syntax (`![alt](/aoe/artifacts/shot.png)`); map a path under a session
 * artifact root to the authenticated route and render the fetched bytes
 * inline. A local path we cannot serve renders as inert text rather than a
 * broken image icon. Everything else keeps default <img> behavior. See #2587.
 */
function TranscriptImage({ src, alt, ...rest }: React.ComponentPropsWithoutRef<"img">) {
  const { fileRefSession } = useAcpFileRef();
  const ref = typeof src === "string" ? parseFileRef(src) : null;
  const artifactUrl = ref && fileRefSession ? resolveArtifactUrl(ref.path, fileRefSession) : null;

  if (artifactUrl) {
    return <ArtifactImage url={artifactUrl} alt={typeof alt === "string" ? alt : undefined} />;
  }
  if (ref && fileRefSession && !resolveToRepoRelative(ref.path, fileRefSession)) {
    return <span className="acp-inert-path">{alt || src}</span>;
  }
  return <img {...rest} src={src} alt={alt} />;
}

/**
 * Blockquote with a "warning callout" variant. When the rendered text
 * starts with the ⚠️ marker (used today by the structured view `context_reset`
 * synthetic message, see AcpRuntime.tsx), apply an amber-tinted
 * variant so the notice stands out from the surrounding transcript.
 * Plain agent-emitted blockquotes keep the default muted style.
 */
function Blockquote({ children, ...rest }: React.ComponentPropsWithoutRef<"blockquote">) {
  const text = childrenText(children);
  const warn = text.trimStart().startsWith("⚠️");
  return (
    <blockquote {...rest} className={warn ? "acp-callout-warn" : undefined}>
      {children}
    </blockquote>
  );
}

function childrenText(children: React.ReactNode): string {
  if (typeof children === "string") return children;
  if (typeof children === "number") return String(children);
  if (Array.isArray(children)) return children.map(childrenText).join("");
  if (React.isValidElement(children)) {
    const props = children.props as { children?: React.ReactNode };
    return childrenText(props.children);
  }
  return "";
}

/**
 * Wrap GFM tables in a scroll container so a real <table> element can
 * keep its native auto-layout (cells distribute to fill the bubble
 * width when content is short, expand and trigger horizontal scroll
 * when content is long). Doing this on the bare <table> via
 * `display: block` breaks column sizing.
 */
function TableWithScroll({ children, ...rest }: React.ComponentPropsWithoutRef<"table">) {
  return (
    <div className="acp-table-wrap">
      <table {...rest}>{children}</table>
    </div>
  );
}

/**
 * Shiki-backed code block. Loads the language module on demand the
 * first time we see it, then renders against the current resolved
 * theme (from useShikiTheme). Falls back to a plain <pre> while the
 * language is loading or for unknown languages.
 */
function ShikiSyntaxHighlighter({ language, code }: SyntaxHighlighterProps) {
  const [html, setHtml] = useState<string | null>(null);
  const shiki = useShikiTheme();
  useEffect(() => {
    let cancelled = false;
    if (!language) return;
    (async () => {
      try {
        const langKey = langKeyForExt(language) ?? language;
        await loadLanguage(langKey);
        const resolvedTheme = await ensureThemeLoaded(shiki.theme, shiki.appearance);
        const hl = await getHighlighter();
        if (cancelled) return;
        setHtml(hl.codeToHtml(code, { lang: langKey, theme: resolvedTheme }));
      } catch {
        // Unknown lang → fall through to plain rendering.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [language, code, shiki.theme, shiki.appearance]);

  if (html) {
    return (
      <div
        className="overflow-x-auto px-3 py-2 text-xs [&_pre]:!bg-transparent [&_pre]:!m-0 [&_pre]:!p-0"
        dangerouslySetInnerHTML={{ __html: html }}
      />
    );
  }
  return <pre className="overflow-x-auto px-3 py-2 text-xs font-mono text-text-primary">{code}</pre>;
}

/** Header strip above each code block: language label + copy button. */
function CodeHeader({ language, code }: CodeHeaderProps) {
  return (
    <div className="flex items-center justify-between border-b border-surface-800 bg-surface-950 px-3 py-1 text-[11px] font-mono uppercase tracking-wider text-text-dim">
      <span>{language ?? "text"}</span>
      <button
        type="button"
        className="rounded px-2 py-0.5 hover:bg-surface-800 hover:text-text-secondary"
        onClick={() => navigator.clipboard?.writeText(code).catch(() => {})}
      >
        copy
      </button>
    </div>
  );
}
