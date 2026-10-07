import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";

interface Props {
  content: string;
}

/// Rendered Markdown file. Not the structured view `<Markdown>` (needs its runtime
/// provider) and without remark-breaks, so soft wraps reflow as CommonMark.
export function MarkdownFileView({ content }: Props) {
  return (
    <div className="flex-1 min-h-0 overflow-auto px-4 py-3">
      <div className="acp-markdown text-sm leading-relaxed max-w-[80ch]">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{content}</ReactMarkdown>
      </div>
    </div>
  );
}
