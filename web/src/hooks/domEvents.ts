/** Register `handler` for every [target, type] pair and return a remover for all of them. */
export function listen(handler: EventListener, ...events: [EventTarget, string][]): () => void {
  for (const [target, type] of events) target.addEventListener(type, handler);
  return () => {
    for (const [target, type] of events) target.removeEventListener(type, handler);
  };
}
