import ReactMarkdown from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

interface Props {
  text: string;
}

/// Comment body Markdown. The structured view `<Markdown>` needs a provider not mounted here.
export function CommentMarkdown({ text }: Props) {
  return (
    <div className="diff-comment-md text-[13px] leading-relaxed">
      <ReactMarkdown remarkPlugins={[remarkGfm, remarkBreaks]}>{text}</ReactMarkdown>
    </div>
  );
}
