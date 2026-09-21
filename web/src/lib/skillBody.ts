/** Drop leading YAML frontmatter, which duplicates the detail header; keep everything when the fence never closes. */
export function skillBody(content: string): string {
  const withoutBom = content.replace(/^\uFEFF/, "");
  // The closing marker must be followed by a newline or end of input, so `----` is left alone.
  const match = /^---\r?\n[\s\S]*?\r?\n---(?:\r?\n|$)/.exec(withoutBom);
  return match ? withoutBom.slice(match[0].length) : withoutBom;
}
