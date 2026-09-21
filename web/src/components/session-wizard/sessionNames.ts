// Mirror of `branch_name_from_title` in src/session/builder.rs.
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
  const stripped = expanded.normalize("NFKD").replace(/\p{M}/gu, "").toLowerCase();
  let out = "";
  let lastDash = false;
  for (const ch of stripped) {
    const code = ch.charCodeAt(0);
    const isAlnum = (code >= 0x30 && code <= 0x39) || (code >= 0x61 && code <= 0x7a);
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
