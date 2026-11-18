import { describe, expect, test } from "bun:test";
import { loadActivityPages, MAX_PAGE } from "./activityPages";
import type { ActivityEntry } from "./bindings/ActivityEntry";

/** `total` entries, newest (highest id) first, served by cursor like the Rust side. */
function fake(total: number) {
  const all = Array.from({ length: total }, (_, i) => ({ id: total - i, at: 0, device: null, item: null, action: "added", detail: "" }) as ActivityEntry);
  const asked: number[] = [];
  const fetchPage = async (before: number | null, limit: number) => {
    asked.push(limit);
    return all.filter((e) => before === null || e.id < before).slice(0, limit);
  };
  return { fetchPage, asked };
}

describe("loadActivityPages", () => {
  test("a reload past the 500 cap keeps every row the user had paged in", async () => {
    const { fetchPage, asked } = fake(1200);
    const { rows, more } = await loadActivityPages(fetchPage, 600);
    expect(rows.length).toBe(600);
    expect(rows.map((r) => r.id)).toEqual(Array.from({ length: 600 }, (_, i) => 1200 - i));
    expect(more).toBe(true);
    expect(asked).toEqual([MAX_PAGE, 100]);
  });
  test("asking for more than exists returns all of it and says there is no more", async () => {
    const { fetchPage } = fake(1200);
    const { rows, more } = await loadActivityPages(fetchPage, 1500);
    expect(rows.length).toBe(1200);
    expect(more).toBe(false);
  });
  test("a small list stops after one short page", async () => {
    const { fetchPage, asked } = fake(30);
    const { rows, more } = await loadActivityPages(fetchPage, 100);
    expect(rows.length).toBe(30);
    expect(more).toBe(false);
    expect(asked).toEqual([100]);
  });
});
