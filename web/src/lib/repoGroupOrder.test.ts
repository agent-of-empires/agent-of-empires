// @vitest-environment jsdom

import { beforeEach, expect, it } from "vitest";

import { loadRepoGroupOrder, persistRepoGroupOrder } from "./repoGroupOrder";

const ORDER_KEY = "aoe-repo-group-order-v1";

beforeEach(() => {
  window.localStorage.clear();
});

it("round-trips an order and removes the key when empty", () => {
  expect(loadRepoGroupOrder()).toEqual([]);
  persistRepoGroupOrder(["/repo-b", "/repo-a"]);
  expect(window.localStorage.getItem(ORDER_KEY)).toBe(JSON.stringify(["/repo-b", "/repo-a"]));
  expect(loadRepoGroupOrder()).toEqual(["/repo-b", "/repo-a"]);
  persistRepoGroupOrder([]);
  expect(window.localStorage.getItem(ORDER_KEY)).toBeNull();
});

it("loads only the string entries of a stored array", () => {
  const cases: [string, string[]][] = [
    ["{not json", []],
    [JSON.stringify({ a: 1 }), []],
    [JSON.stringify(["/repo-a", 42, null, "/repo-b"]), ["/repo-a", "/repo-b"]],
  ];
  for (const [stored, expected] of cases) {
    window.localStorage.setItem(ORDER_KEY, stored);
    expect(loadRepoGroupOrder(), stored).toEqual(expected);
  }
});
