// Small client-side 5-field cron validator for the scheduled-jobs widget
// (#2886). The server does not re-validate cron, so an invalid expression would
// silently never fire; this catches the obvious mistakes (wrong field count,
// out-of-range values) before save. It is intentionally minimal: numeric fields
// with `*`, `,`, `-`, `/`, matching the ranges croner enforces server-side and
// covering everything the picker generates. Named tokens (JAN/MON) are not
// accepted here; advanced users wanting those edit via the TUI or config.toml.

const FIELD_RANGES: [number, number][] = [
  [0, 59], // minute
  [0, 23], // hour
  [1, 31], // day of month
  [1, 12], // month
  [0, 7], // day of week (0 and 7 are both Sunday)
];
const FIELD_NAMES = ["minute", "hour", "day-of-month", "month", "day-of-week"];

function inRange(token: string, min: number, max: number): boolean {
  if (!/^\d+$/.test(token)) return false;
  const n = Number(token);
  return n >= min && n <= max;
}

function validItem(item: string, min: number, max: number): boolean {
  const slash = item.split("/");
  if (slash.length > 2) return false;
  const base = slash[0] ?? "";
  if (slash.length === 2) {
    const step = slash[1] ?? "";
    if (!/^\d+$/.test(step) || Number(step) < 1) return false;
  }
  if (base === "*") return true;
  const dash = base.split("-");
  if (dash.length === 1) return inRange(dash[0] ?? "", min, max);
  if (dash.length === 2) return inRange(dash[0] ?? "", min, max) && inRange(dash[1] ?? "", min, max);
  return false;
}

/** Return an error string for an invalid 5-field cron, or null when it is
 *  acceptable. Purely a UX gate; the picker only ever produces valid output. */
export function validateCron(expr: string): string | null {
  const parts = expr.trim().split(/\s+/).filter(Boolean);
  if (parts.length !== 5) {
    return "Cron must have exactly 5 fields: minute hour day-of-month month day-of-week.";
  }
  for (let i = 0; i < 5; i++) {
    const range = FIELD_RANGES[i];
    const field = parts[i];
    if (!range || field === undefined) continue;
    const [min, max] = range;
    const ok = field.split(",").every((item) => validItem(item, min, max));
    if (!ok) return `Invalid ${FIELD_NAMES[i]} field: "${field}".`;
  }
  return null;
}
