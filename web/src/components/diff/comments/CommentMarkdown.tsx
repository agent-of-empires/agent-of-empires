import ReactMarkdown from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

interface Props {
  text: string;
}

/// Render markdown for diff-comment bodies with react-markdown, the same
/// engine the structured view's `<Markdown>` wraps. We deliberately do NOT
/// reuse that component because it depends on `@assistant-ui/react-markdown`'s
/// `<AssistantRuntimeProvider>`, which is only mounted under the structured
/// view panel; the diff viewer is a sibling, so it would throw "requires an
/// AuiProvider". react-markdown renders to React elements (no raw HTML), so
/// user input is escaped without `dangerouslySetInnerHTML`.
export function CommentMarkdown({ text }: Props) {
  return (
    <div className="diff-comment-md text-[13px] leading-relaxed">
      <ReactMarkdown remarkPlugins={[remarkGfm, remarkBreaks]}>{text}</ReactMarkdown>
    </div>
  );
}
