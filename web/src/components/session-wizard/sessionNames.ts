// Mirror of `branch_name_from_title` in src/session/builder.rs: titles
// flow through this when the wizard auto-fills the branch field so the
// user sees the kebab-case slug git will accept rather than the raw
// title (which often contains spaces and would otherwise be rejected
// by libgit2 with an opaque InvalidSpec error).
const LIGATURES: Record<string, string> = {
  ß: "ss",
  æ: "ae",
  Æ: "AE",
  œ: "oe",
  Œ: "OE",
  ø: "o",
  Ø: "O",
  ł: "l",
  Ł: "L",
  đ: "d",
  Đ: "D",
  þ: "th",
  Þ: "Th",
};

export function slugifyBranch(title: string): string {
  let expanded = "";
  for (const ch of title) {
    expanded += LIGATURES[ch] ?? ch;
  }
  const stripped = expanded
    .normalize("NFKD")
    .replace(/\p{M}/gu, "")
    .toLowerCase();
  let out = "";
  let lastDash = false;
  for (const ch of stripped) {
    const code = ch.charCodeAt(0);
    const isAlnum =
      (code >= 0x30 && code <= 0x39) || (code >= 0x61 && code <= 0x7a);
    if (isAlnum || ch === "-" || ch === "_") {
      out += ch;
      lastDash = false;
    } else if (/\s/.test(ch) || /[!-/:-@[-`{-~]/.test(ch)) {
      if (out.length === 0 || lastDash) continue;
      out += "-";
      lastDash = true;
    }
  }
  while (out.endsWith("-")) out = out.slice(0, -1);
  return out.length === 0 ? "session" : out;
}

export function applyBranchOverride(
  _title: string,
  worktreeBranch: string,
): {
  worktreeBranch: string;
  worktreeBranchDirty: boolean;
} {
  // Any direct edit on the branch field, including clearing it, marks it
  // dirty so the title→branch mirror stops overwriting the user's input on
  // the next keystroke. Empty is a valid UI state; the submit path falls
  // back to the title via getSubmittedBranch.
  return {
    worktreeBranch,
    worktreeBranchDirty: true,
  };
}

export function getSubmittedBranch(
  title: string,
  worktreeBranch: string,
): string {
  return worktreeBranch || title || "";
}

export function getReviewSummary(
  title: string,
  worktreeBranch: string,
): {
  title: string;
  branch: string;
} {
  return {
    title: title || worktreeBranch || "Auto-generated",
    branch: worktreeBranch || title || "Auto-generated",
  };
}
