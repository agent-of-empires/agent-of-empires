/* eslint-disable react-refresh/only-export-components */
// Transcript markdown: assistant-ui's streaming-aware MarkdownTextPrimitive with
// shiki code blocks and transcript-aware links, images, tables, and callouts.

import { MarkdownTextPrimitive } from "@assistant-ui/react-markdown";
import type { CodeHeaderProps, SyntaxHighlighterProps } from "@assistant-ui/react-markdown";
import * as React from "react";
import { useEffect, useMemo, useState } from "react";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

import { highlightSnippet } from "../../lib/snippetHighlighter";
import { useShikiTheme } from "../../hooks/useShikiTheme";
import { parseFileRef, resolveArtifactUrl, resolveToRepoRelative } from "../../lib/fileRef";
import { useAcpFileRef } from "./AcpFileRefContext";
import { openInNewTab } from "../../lib/openInNewTab";
import { ArtifactImage } from "./artifactMedia";

interface Props {
  text: string;
  /** Paced reveal, for the live streaming message only; history would otherwise type itself out. */
  smooth?: boolean;
  /** Single newlines as hard breaks, for user prompts typed in a textarea. Assistant
   *  text keeps soft breaks because models hard-wrap their markdown. */
  breaks?: boolean;
}

export function remarkPluginsFor(breaks: boolean) {
  return breaks ? [remarkGfm, remarkBreaks] : [remarkGfm];
}

export function Markdown({ text, smooth = false, breaks = false }: Props) {
  const remarkPlugins = useMemo(() => remarkPluginsFor(breaks), [breaks]);
  return (
    <MarkdownTextPrimitive
      preprocess={() => text}
      smooth={smooth}
      remarkPlugins={remarkPlugins}
      className="acp-markdown acp-markdown-body leading-relaxed"
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

/** Session artifacts open through the authenticated route; local file references
 *  open the in-app viewer, or render inert when outside every repo root; other
 *  links open in a new tab so the session is never navigated away. */
function TranscriptLink({ href, onClick, children, ...rest }: React.ComponentPropsWithoutRef<"a">) {
  const { onOpenFileRef, fileRefSession } = useAcpFileRef();
  const ref = href ? parseFileRef(href) : null;
  const artifactUrl = ref && fileRefSession ? resolveArtifactUrl(ref.path, fileRefSession) : null;

  // Fetched with auth and opened as a blob: a bare navigation would miss the token header.
  if (artifactUrl) {
    return (
      <a
        {...rest}
        href={artifactUrl}
        className="acp-artifact-link"
        onClick={(e) => {
          e.preventDefault();
          void openInNewTab(artifactUrl);
        }}
      >
        {children}
      </a>
    );
  }

  // Out-of-repo paths may not be openable; files the agent touched open from their tool card.
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

/** Artifact images load through the authenticated route; unservable local paths render as text. */
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

/** A blockquote starting with ⚠️ (the synthetic reset/compaction callouts) gets the warning style. */
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

/** Scroll wrapper, so the table keeps native column sizing (`display: block` breaks it). */
function TableWithScroll({ children, ...rest }: React.ComponentPropsWithoutRef<"table">) {
  return (
    <div className="acp-table-wrap">
      <table {...rest}>{children}</table>
    </div>
  );
}

/** Plain <pre> until shiki loads the language; unknown languages stay plain. */
function ShikiSyntaxHighlighter({ language, code }: SyntaxHighlighterProps) {
  // Keyed by the inputs that produced it, so a superseded request resolving
  // before its effect cleanup renders nothing. Theme is left out of the key
  // so a theme switch keeps the old palette until the re-highlight lands.
  // NUL-delimited (as the escape sequence: a raw NUL byte in source makes
  // git treat the file as binary) so field concatenations cannot collide.
  const inputKey = `${language ?? ""}\u0000${code}`;
  const [result, setResult] = useState<{ key: string; html: string } | null>(null);
  const shiki = useShikiTheme();

  useEffect(() => {
    let cancelled = false;
    if (!language) return;
    (async () => {
      try {
        const out = await highlightSnippet(code, {
          langHint: language,
          theme: shiki.theme,
          appearance: shiki.appearance,
        });
        if (cancelled || !out) return;
        setResult({ key: inputKey, html: out });
      } catch {
        // Unknown language: stay plain.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [language, code, inputKey, shiki.theme, shiki.appearance]);

  const html = result && result.key === inputKey ? result.html : null;

  // The em-based size carries no line-height, so set it or code inherits the looser body leading.
  if (html) {
    return (
      <div
        className="overflow-x-auto px-3 py-2 text-[0.86em] leading-[1.3333] [&_pre]:!bg-transparent [&_pre]:!m-0 [&_pre]:!p-0"
        dangerouslySetInnerHTML={{ __html: html }}
      />
    );
  }
  return (
    <pre className="overflow-x-auto px-3 py-2 text-[0.86em] leading-[1.3333] font-mono text-text-primary">{code}</pre>
  );
}

function CodeHeader({ language, code }: CodeHeaderProps) {
  return (
    <div className="flex items-center justify-between border-b border-surface-800 bg-surface-950 px-3 py-1 text-[0.79em] font-mono uppercase tracking-wider text-text-dim">
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
