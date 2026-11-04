import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ServerAbout } from "../api";
import { fetchAboutRetrying } from "../serverAbout";

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
