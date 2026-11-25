/** `order` with `active` moved next to `over`: after it when the drag went down the list, before it when up. */
export function moveId(order: readonly string[], active: string, over: string, after: boolean): string[] {
  const rest = order.filter((id) => id !== active);
  const at = rest.indexOf(over);
  if (at < 0) return [active, ...rest];
  rest.splice(after ? at + 1 : at, 0, active);
  return rest;
}

/** The ids of `first` in their order, each once, then those of `rest` not already listed. */
export function orderStartingWith(first: readonly string[], rest: readonly string[]): string[] {
  return [...new Set([...first, ...rest])];
}

/** `shown` with each `saved` id it lacks put back after its saved predecessor, so a header not on screen keeps
 *  its place. */
export function keepUnshown(shown: readonly string[], saved: readonly string[]): string[] {
  const out = [...shown];
  let previous: string | undefined;
  for (const id of saved) {
    if (!out.includes(id)) {
      const at = previous === undefined ? out.findIndex((x) => saved.includes(x)) : out.indexOf(previous) + 1;
      out.splice(at < 0 ? out.length : at, 0, id);
    }
    previous = id;
  }
  return out;
}
