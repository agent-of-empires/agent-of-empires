import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ServerAbout } from "../api";
import { ABOUT_TRY_TIMEOUT_MS, fetchAboutRetrying } from "../serverAbout";

beforeEach(() => vi.useFakeTimers());
afterEach(() => vi.useRealTimers());

const about = { read_only: false } as ServerAbout;

it("retries /api/about with backoff and gives up after the last wait", async () => {
  // (answers per try, result, tries made)
  const cases: [(ServerAbout | null)[], ServerAbout | null, number][] = [
    [[about], about, 1],
    [[null, about], about, 2],
    [[null, null, about], about, 3],
    [[null, null, null, about], null, 3],
  ];
  for (const [answers, want, tries] of cases) {
    const fetchOnce = vi.fn(async () => answers.shift() ?? null);
    const result = fetchAboutRetrying(fetchOnce, [500, 1500]);
    await vi.advanceTimersByTimeAsync(499);
    expect(fetchOnce.mock.calls.length, `${tries}`).toBe(1);
    await vi.advanceTimersByTimeAsync(2000);
    await expect(result).resolves.toBe(want);
    expect(fetchOnce).toHaveBeenCalledTimes(tries);
  }
});

it("stops retrying once the caller has gone", async () => {
  let gone = false;
  const fetchOnce = vi.fn(async () => null);
  const result = fetchAboutRetrying(fetchOnce, [500, 1500], () => gone);
  gone = true;
  await vi.advanceTimersByTimeAsync(5000);
  await expect(result).resolves.toBeNull();
  expect(fetchOnce).toHaveBeenCalledTimes(1);
});

it("gives each try a deadline, so a hung request can't hold the shell", async () => {
  // A request that ends when aborted, and one that never settles at all.
  const deaf = () => new Promise<ServerAbout | null>(() => {});
  const hang = (signal: AbortSignal) =>
    new Promise<ServerAbout | null>((_, reject) =>
      signal.addEventListener("abort", () => reject(new Error("aborted"))),
    );
  const deadline = ABOUT_TRY_TIMEOUT_MS;
  // (behaviour per try, result, settles after ms)
  const cases: [string, ((s: AbortSignal) => Promise<ServerAbout | null>)[], ServerAbout | null, number][] = [
    ["fails fast, then hangs, then answers", [async () => null, hang, async () => about], about, 500 + deadline + 1500],
    ["every try hangs", [hang, hang, hang], null, 3 * deadline + 500 + 1500],
    ["every try ignores the abort", [deaf, deaf, deaf], null, 3 * deadline + 500 + 1500],
  ];
  for (const [name, tries, want, settlesAfter] of cases) {
    const signals: AbortSignal[] = [];
    let i = 0;
    const fetchOnce = vi.fn((signal: AbortSignal) => {
      signals.push(signal);
      return tries[i++]!(signal).catch(() => null);
    });
    let settled = false;
    const result = fetchAboutRetrying(fetchOnce, [500, 1500]).finally(() => (settled = true));
    await vi.advanceTimersByTimeAsync(settlesAfter - 1);
    expect(settled, name).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    expect(settled, name).toBe(true);
    await expect(result, name).resolves.toBe(want);
    // A try that outlasted its deadline was aborted, not left running.
    expect(signals.filter((s) => s.aborted).length, name).toBe(tries.filter((t) => t === hang || t === deaf).length);
  }
});
