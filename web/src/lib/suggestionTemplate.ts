export const VARIABLE = /\{([^{}]*)\}/g;

/** Replaces known `{name}` once, left to right; anything else stays as typed, like the server. */
export function expandTemplate(template: string, variables: Record<string, string>): string {
  return template.replace(VARIABLE, (match, name: string) => variables[name] ?? match);
}
