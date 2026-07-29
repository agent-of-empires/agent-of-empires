import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

interface Props {
  /** Markdown source to render. */
  content: string;
}

/// Rendered view of a Markdown file (#3088). Mirrors the diff-comment renderer
/// (`CommentMarkdown`): plain `react-markdown` with `remark-gfm`, rendered to
/// React elements so file/agent-authored content is escaped without
/// `dangerouslySetInnerHTML`. We deliberately do NOT reuse the structured view
/// `<Markdown>`, which needs `@assistant-ui/react-markdown`'s runtime provider
/// (only mounted under the ACP panel), nor `remark-breaks`: Markdown files are
/// CommonMark, where a single newline is a soft break, so hardening every wrap
/// into a `<br>` would break paragraph reflow. Reuses the shared `acp-markdown`
/// prose styles (index.css), which are plain CSS with no runtime dependency.
export function MarkdownFileView({ content }: Props) {
  return (
    <div className="flex-1 min-h-0 overflow-auto px-4 py-3">
      <div className="acp-markdown text-sm leading-relaxed max-w-[80ch]">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{content}</ReactMarkdown>
      </div>
    </div>
  );
}
