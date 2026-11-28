import type { ElicitationAnswer } from "../../lib/acpTypes";
import { CAPTION_INLINE } from "../../lib/surfaces";
import { USER_REPLY_ITEM, UserReplyCard } from "./UserReplyCard";

interface Props {
  answers: ElicitationAnswer[];
}

/** Rich rendering of the user's reply to an AskUserQuestion / elicitation
 *  form in the structured view user-message slot. Built from the typed
 *  answers carried on the assistant-ui message metadata; falls back to raw
 *  text rendering upstream when absent. Mirrors `DiffCommentsUserCard` so a
 *  structured pick reads as a tidy card, not a flat "Q: A" line. See #2209. */
export function ElicitationAnswerCard({ answers }: Props) {
  return (
    <UserReplyCard
      testId="elicitation-answer-card"
      tag="answer"
      summary={`${answers.length} answer${answers.length === 1 ? "" : "s"}`}
      copySource={answers.map((a) => `**${a.question}**\n${a.answer}`).join("\n\n")}
      copyLabel="Copy answers"
    >
      <ul className="flex flex-col gap-2">
        {answers.map((a, i) => (
          <li key={`${i}-${a.question}`} className={`${USER_REPLY_ITEM} px-3 py-2`}>
            <div className={`mb-0.5 ${CAPTION_INLINE}`}>{a.question}</div>
            <div className="whitespace-pre-wrap text-text-primary">{a.answer}</div>
          </li>
        ))}
      </ul>
    </UserReplyCard>
  );
}
