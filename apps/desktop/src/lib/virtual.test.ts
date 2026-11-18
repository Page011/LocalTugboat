import { describe, expect, test } from "bun:test";
import { arrowTarget, focusIndexFor, GRID, gridLayout, innerWidth, tileWidth, visibleRows } from "./virtual";

describe("focusIndexFor", () => {
  const ids = (...s: string[]) => s.map((id) => ({ id }));
  test("follows the selected item when new items arrive at the top", () => {
    const before = ids("A", "B", "C");
    expect(focusIndexFor(before, "B", 0)).toBe(1);
    const after = [...ids("X"), ...before];
    expect(focusIndexFor(after, "B", 1)).toBe(2);
  });
  test("a selection that is not in the list falls back to the number", () => {
    expect(focusIndexFor(ids("A", "B"), "gone", 1)).toBe(1);
    expect(focusIndexFor(ids("A", "B"), null, 0)).toBe(0);
  });
});

describe("gridLayout", () => {
  test("fits as many columns as the width allows", () => {
    // 3 tiles of 100 + 2 gaps of 10 = 320
    expect(gridLayout(10, 320, 100, 100, 10)).toEqual({ cols: 3, rows: 4, rowHeight: 110, totalHeight: 430 });
    expect(gridLayout(10, 319, 100, 100, 10).cols).toBe(2);
  });
  test("a zero width still gives one column, and empty has no height", () => {
    expect(gridLayout(5, 0, 100, 100, 10).cols).toBe(1);
    expect(gridLayout(0, 500, 100, 100, 10)).toMatchObject({ rows: 0, totalHeight: 0 });
  });
});

describe("visibleRows", () => {
  const l = gridLayout(1000, 320, 100, 100, 10); // 334 rows of 110
  test("renders the viewport plus overscan", () => {
    expect(visibleRows(l, 0, 220, 2)).toEqual([0, 4]);
    expect(visibleRows(l, 1100, 220, 2)).toEqual([8, 14]);
  });
  test("scrolled past the end still renders the last row", () => {
    const [a, b] = visibleRows(l, 10_000_000, 220, 2);
    expect(b).toBe(l.rows);
    expect(a).toBeLessThan(b);
  });
  test("empty grid renders nothing", () => {
    expect(visibleRows(gridLayout(0, 320, 100, 100, 10), 0, 500)).toEqual([0, 0]);
  });
});

describe("arrowTarget", () => {
  test("moves by one or by a row", () => {
    expect(arrowTarget(4, "ArrowRight", 3, 10)).toBe(5);
    expect(arrowTarget(4, "ArrowLeft", 3, 10)).toBe(3);
    expect(arrowTarget(4, "ArrowDown", 3, 10)).toBe(7);
    expect(arrowTarget(4, "ArrowUp", 3, 10)).toBe(1);
  });
  test("does not wrap off either end", () => {
    expect(arrowTarget(0, "ArrowLeft", 3, 10)).toBeNull();
    expect(arrowTarget(1, "ArrowUp", 3, 10)).toBeNull();
    expect(arrowTarget(9, "ArrowRight", 3, 10)).toBeNull();
    expect(arrowTarget(9, "ArrowDown", 3, 10)).toBeNull();
  });
  test("down from a short last row lands on the last item", () => {
    expect(arrowTarget(8, "ArrowDown", 3, 10)).toBe(9);
  });
  test("other keys and an empty grid do nothing", () => {
    expect(arrowTarget(0, "a", 3, 10)).toBeNull();
    expect(arrowTarget(0, "ArrowDown", 3, 0)).toBeNull();
  });
});

describe("tileWidth", () => {
  test("tiles share the row, so the columns reach the right edge", () => {
    // 1440 wide window: 7 columns of at least 168 + 6 gaps of 12; the row is filled exactly.
    const width = 1200;
    const { cols } = gridLayout(50, width, 168, 188, 12);
    const w = tileWidth(width, cols, 12, 168);
    expect(w).toBeGreaterThanOrEqual(168);
    expect(cols * w + (cols - 1) * 12).toBeLessThanOrEqual(width);
    expect(width - (cols * w + (cols - 1) * 12)).toBeLessThan(cols); // under 1px of slack per tile
  });
  test("never below the minimum, and a zero width gives the minimum", () => {
    expect(tileWidth(0, 1, 12, 168)).toBe(168);
    expect(tileWidth(200, 1, 12, 168)).toBe(200);
  });
});

// Dark-gallery regression: the columns were sized for a width that included the vertical scrollbar, so the
// last column was clipped and a horizontal scrollbar appeared. The grid must fit the VISIBLE width for every width.
describe("the laid-out row always fits the visible width", () => {
  test("for every scroller width a window can have, no tile reaches past the visible edge", async () => {
    const { tileMin, tileH, gap, pad } = GRID;
    for (let visible = tileMin + 2 * pad; visible <= 3840; visible++) {
      const inner = innerWidth(visible, pad);
      const { cols } = gridLayout(500, inner, tileMin, tileH, gap);
      const w = tileWidth(inner, cols, gap, tileMin);
      const span = 2 * pad + cols * w + (cols - 1) * gap;
      expect(span).toBeLessThanOrEqual(visible);
    }
  });

  test("the scrollbar's width is not available to the tiles", () => {
    // 820 px scroller, 15 px classic scrollbar: the tiles get 805 - 8, not 820 - 8.
    const visible = 820 - 15;
    const inner = innerWidth(visible, GRID.pad);
    expect(inner).toBe(797);
    const { cols } = gridLayout(9, inner, GRID.tileMin, GRID.tileH, GRID.gap);
    expect(2 * GRID.pad + cols * tileWidth(inner, cols, GRID.gap, GRID.tileMin) + (cols - 1) * GRID.gap).toBeLessThanOrEqual(visible);
  });

  test("ItemGrid measures the element's clientWidth and never shows a horizontal scrollbar", async () => {
    const { readFileSync } = await import("node:fs");
    const src = readFileSync(new URL("./ItemGrid.svelte", import.meta.url), "utf8");
    expect(src).not.toMatch(/\sbind:clientWidth=/);
    expect(src).toMatch(/el\.clientWidth/);
    expect(src).toMatch(/overflow-x:\s*hidden/);
    expect(src).toMatch(/scrollbar-gutter:\s*stable/);
  });
});
