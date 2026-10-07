export function uniqueSlots(ui: { slot: string }[]): string {
  return [...new Set(ui.map((u) => u.slot))].join(", ");
}
