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
