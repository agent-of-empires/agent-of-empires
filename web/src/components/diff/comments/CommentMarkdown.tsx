import { useMemo } from "react";
import { marked } from "marked";

interface Props {
  text: string;
}

/// Render markdown for diff-comment bodies. We deliberately do NOT
/// reuse the structured view `<Markdown>` component because that one depends
/// on `@assistant-ui/react-markdown`'s `<AssistantRuntimeProvider>`
/// which is only mounted under the structured view panel. The diff viewer is
/// a sibling of that panel, so calling the structured view Markdown component
/// here throws "requires an AuiProvider" and unmounts the tree.
///
/// `marked` is configured with HTML disabled (`sanitize` is dropped in
/// modern marked but `breaks: false` + no `html` extension keeps user
/// input safe enough for local-only review notes; comments never leave
/// the user's browser until they're posted as plaintext to the agent).
export function CommentMarkdown({ text }: Props) {
  const html = useMemo(() => {
    return marked.parse(text, { async: false, breaks: true }) as string;
  }, [text]);
  return <div className="diff-comment-md text-[13px] leading-relaxed" dangerouslySetInnerHTML={{ __html: html }} />;
}
