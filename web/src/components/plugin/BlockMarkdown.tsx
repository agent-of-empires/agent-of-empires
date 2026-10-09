import type { ComponentPropsWithoutRef } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

import { toneTextClass, validTone } from "../../lib/pluginUi";
import { pluginLinkProps, safeHref, str, type Obj } from "./slotPayload";

const REMARK_PLUGINS = [remarkGfm];

// Plugin markdown is untrusted: a link must pass the `row.href` policy or collapses to its text.
function MarkdownLink({ href, children }: ComponentPropsWithoutRef<"a">) {
  const safe = safeHref(href);
  return safe ? <a {...pluginLinkProps(safe)}>{children}</a> : <>{children}</>;
}

// Never load a plugin-chosen URL: an image degrades to its alt text.
function MarkdownImage({ alt }: ComponentPropsWithoutRef<"img">) {
  return alt ? <span>{alt}</span> : null;
}

const COMPONENTS = { a: MarkdownLink, img: MarkdownImage };

/** `skipHtml` drops raw HTML, comments and `<details>` tags; inner markdown still renders only after a blank line, as CommonMark ends an HTML block there. */
export function BlockMarkdown({ block }: { block: Obj }) {
  const text = str(block, "text");
  if (!text?.trim()) return null;
  return (
    <div
      className={`acp-markdown text-xs leading-relaxed ${toneTextClass(validTone(block.tone))}`}
      data-testid="plugin-pane-markdown"
    >
      <ReactMarkdown remarkPlugins={REMARK_PLUGINS} skipHtml components={COMPONENTS}>
        {text}
      </ReactMarkdown>
    </div>
  );
}
