// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import { loadRepoGroupOrder, persistRepoGroupOrder } from "./repoGroupOrder";

const ORDER_KEY = "aoe-repo-group-order-v1";

beforeEach(() => {
  window.localStorage.clear();
});

describe("repoGroupOrder", () => {
  it("returns an empty list when nothing is stored", () => {
    expect(loadRepoGroupOrder()).toEqual([]);
  });

  it("round-trips a persisted order through localStorage", () => {
    persistRepoGroupOrder(["/repo-b", "/repo-a"]);
    expect(window.localStorage.getItem(ORDER_KEY)).toBe(JSON.stringify(["/repo-b", "/repo-a"]));
    expect(loadRepoGroupOrder()).toEqual(["/repo-b", "/repo-a"]);
  });

  it("removes the key when persisting an empty order", () => {
    persistRepoGroupOrder(["/repo-a"]);
    persistRepoGroupOrder([]);
    expect(window.localStorage.getItem(ORDER_KEY)).toBeNull();
    expect(loadRepoGroupOrder()).toEqual([]);
  });

  it("ignores malformed JSON", () => {
    window.localStorage.setItem(ORDER_KEY, "{not json");
    expect(loadRepoGroupOrder()).toEqual([]);
  });

  it("ignores a stored value that is not an array", () => {
    window.localStorage.setItem(ORDER_KEY, JSON.stringify({ a: 1 }));
    expect(loadRepoGroupOrder()).toEqual([]);
  });

  it("drops non-string entries from the stored array", () => {
    window.localStorage.setItem(ORDER_KEY, JSON.stringify(["/repo-a", 42, null, "/repo-b"]));
    expect(loadRepoGroupOrder()).toEqual(["/repo-a", "/repo-b"]);
  });
});
