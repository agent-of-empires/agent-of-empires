/** Kinds whose prompt carries text written by other people. The API does not say, so the ids live here. */
const FROM_OTHER_PEOPLE = new Set(["mentioned", "mentions", "new_issues", "new-issues"]);

export const fromOtherPeople = (criterion: string) => FROM_OTHER_PEOPLE.has(criterion);

export const FROM_OTHER_PEOPLE_NOTE =
  "From other people. Start sends their text as the prompt; to read it first, use ▾ › Start with changes.";
