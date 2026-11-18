import { afterEach, describe, expect, it, vi } from "vitest";

import { getLiveQueueCount, setLiveQueueCount, subscribeLiveQueueCounts } from "./acpStateStorage";

const unsubs: (() => void)[] = [];
function listen(filter: string[] | null) {
  const cb = vi.fn();
  unsubs.push(subscribeLiveQueueCounts(cb, filter && new Set(filter)));
  return cb;
}

afterEach(() => {
  for (const unsub of unsubs.splice(0)) unsub();
  for (const id of ["a", "b"]) setLiveQueueCount(id, null);
});

describe("live queue counts", () => {
  it("notifies matching subscribers only when a session's count changes", () => {
    const a = listen(["a"]);
    const b = listen(["b"]);
    const all = listen(null);
    const calls = () => [a, b, all].map((cb) => cb.mock.calls.length);
    // (step, the count it leaves for "a", calls per subscriber after it)
    const steps: [() => void, number | undefined, number[]][] = [
      [() => setLiveQueueCount("a", 3), 3, [1, 0, 1]],
      [() => setLiveQueueCount("a", 3), 3, [1, 0, 1]],
      [() => setLiveQueueCount("a", 0), 0, [2, 0, 2]],
      [() => setLiveQueueCount("b", 1), 0, [2, 1, 3]],
      [() => setLiveQueueCount("a", null), undefined, [3, 1, 4]],
      [() => setLiveQueueCount("a", null), undefined, [3, 1, 4]],
    ];
    for (const [step, count, expected] of steps) {
      step();
      expect(getLiveQueueCount("a")).toBe(count);
      expect(calls()).toEqual(expected);
    }
  });

  it("stops firing after unsubscribe", () => {
    const cb = listen(["a"]);
    for (const unsub of unsubs.splice(0)) unsub();
    setLiveQueueCount("a", 1);
    expect(cb).not.toHaveBeenCalled();
  });
});
