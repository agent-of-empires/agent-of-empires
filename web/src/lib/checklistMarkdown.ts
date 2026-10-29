export type ChecklistStatus = "pending" | "in_progress" | "done" | "cancelled";

/** A GFM task list. Only checked and unchecked exist there, so the other two states say so in the text. */
export function checklistMarkdown(items: { text: string; status: ChecklistStatus }[]): string {
  return items
    .map(({ text, status }) => {
      switch (status) {
        case "done":
          return `- [x] ${text}`;
        case "in_progress":
          return `- [ ] ${text} (in progress)`;
        case "cancelled":
          return `- [ ] ~~${text}~~`;
        case "pending":
          return `- [ ] ${text}`;
      }
    })
    .join("\n");
}
