import { expect, it, vi } from "vitest";
import type { ServerAbout } from "../api";
import { createAboutRequests } from "../aboutRequests";

/** A fetch whose answers the test releases by hand, in any order. */
function heldFetches() {
  const pending: ((about: ServerAbout | null) => void)[] = [];
  const fetchOnce = vi.fn(
    () =>
      new Promise<ServerAbout | null>((resolve) => {
        pending.push(resolve);
      }),
  );
  return { fetchOnce, release: (i: number, about: ServerAbout | null) => pending[i]!(about) };
}

function setup() {
  const fetches = heldFetches();
  const answers: (string | undefined)[] = [];
  const on = { start: vi.fn(), answer: vi.fn((a: ServerAbout) => answers.push(a.profile)), settled: vi.fn() };
  return { ...fetches, on, answers, requests: createAboutRequests(fetches.fetchOnce, on) };
}

const about = (profile: string) => ({ profile, read_only: false }) as ServerAbout;

it("joins a second recovery to the one in flight, so its answer isn't discarded", async () => {
  const { fetchOnce, release, answers, on, requests } = setup();
  const first = requests.recover();
  const second = requests.recover();
  expect(fetchOnce).toHaveBeenCalledTimes(1);
  release(0, about("main"));
  await Promise.all([first, second]);
  expect(answers).toEqual(["main"]);
  expect(on.settled).toHaveBeenCalledTimes(1);
  // Once it has settled, the next recovery asks again.
  void requests.recover();
  expect(fetchOnce).toHaveBeenCalledTimes(2);
});

it("lets a rename's refresh supersede a recovery in flight, whichever answers first", async () => {
  // (order the two answers arrive, answers applied)
  const cases: [[number, number], string[]][] = [
    [[0, 1], ["renamed"]],
    [[1, 0], ["renamed"]],
  ];
  for (const [order, want] of cases) {
    const { fetchOnce, release, answers, requests } = setup();
    const recovery = requests.recover();
    // The rename asks again; a recovery started before it is stale.
    const rename = requests.refresh();
    expect(fetchOnce).toHaveBeenCalledTimes(2);
    const replies = [about("old"), about("renamed")];
    for (const i of order) release(i, replies[i]!);
    await Promise.all([recovery, rename]);
    expect(answers, `order ${order}`).toEqual(want);
  }
});

it("recovers by joining a rename's refresh already in flight", async () => {
  const { fetchOnce, release, answers, requests } = setup();
  const rename = requests.refresh();
  const recovery = requests.recover();
  expect(fetchOnce).toHaveBeenCalledTimes(1);
  release(0, about("renamed"));
  await Promise.all([rename, recovery]);
  expect(answers).toEqual(["renamed"]);
});

it("keeps the last answer when a request fails, and settles only the current request", async () => {
  const { release, answers, on, requests } = setup();
  const first = requests.refresh();
  release(0, about("main"));
  await first;
  const failed = requests.refresh();
  release(1, null);
  await failed;
  expect(answers).toEqual(["main"]);
  expect(on.start).toHaveBeenCalledTimes(2);
  expect(on.settled).toHaveBeenCalledTimes(2);
});
