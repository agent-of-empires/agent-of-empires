/** Drop a SKILL.md's leading YAML frontmatter fence, leaving the instructions.
 *
 *  Rendered as markdown, frontmatter turns into a horizontal rule followed by a
 *  paragraph reading "name: ... description: ...", which is noise: both fields
 *  already appear in the detail header. Falls back to the whole text when there
 *  is no closing fence, so a malformed file still previews as something rather
 *  than as nothing.
 *
 *  Lives outside the component file so the preview can import it without
 *  tripping the fast-refresh rule against non-component exports.
 */
export function skillBody(content: string): string {
  const withoutBom = content.replace(/^\uFEFF/, "");
  // The closing marker must be followed by a newline or end of input.
  // A bare `\r?\n?` also matched the first three hyphens of `----`, so a
  // malformed file lost a character instead of being left alone.
  const match = /^---\r?\n[\s\S]*?\r?\n---(?:\r?\n|$)/.exec(withoutBom);
  return match ? withoutBom.slice(match[0].length) : withoutBom;
}
