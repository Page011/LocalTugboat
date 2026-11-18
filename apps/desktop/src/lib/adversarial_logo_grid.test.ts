// Adversarial checks of the gallery grid layout: the columns must fit the visible width (the scroller width minus its
// vertical scrollbar) at every width.
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { GRID, gridLayout, innerWidth, tileWidth } from "./virtual";

const { tileMin, tileH, gap, pad } = GRID;
function span(visible: number) {
  const inner = innerWidth(visible, pad);
  const { cols } = gridLayout(500, inner, tileMin, tileH, gap);
  const w = tileWidth(inner, cols, gap, tileMin);
  return { cols, w, span: 2 * pad + cols * w + (cols - 1) * gap };
}

describe("window widths 900 / 1200 / 1440 with any scrollbar 0..20 px", () => {
  // Window 1042 px measured a 809 px scroller (qa run): chrome + sidebar + padding = 233 px.
  for (const win of [900, 1200, 1440]) {
    test(`${win} px window`, () => {
      for (let sb = 0; sb <= 20; sb++) {
        // clientWidth is rounded: an actual 0.5 px narrower box can report the larger integer.
        const visible = win - 233 - sb;
        const r = span(visible);
        expect(r.span).toBeLessThanOrEqual(visible);
        expect(r.w).toBeGreaterThanOrEqual(tileMin);
      }
    });
  }
});

describe("every column count 1..5 at its narrowest and widest scroller", () => {
  for (let c = 1; c <= 5; c++) {
    test(`${c} column(s)`, () => {
      const widths: number[] = [];
      for (let v = tileMin + 2 * pad; v <= 2000; v++) if (span(v).cols === c) widths.push(v);
      expect(widths.length).toBeGreaterThan(0);
      for (const v of [widths[0]!, widths[widths.length - 1]!]) {
        const r = span(v);
        expect(r.span).toBeLessThanOrEqual(v);
        // No dead strip wider than one column's rounding.
        expect(v - r.span).toBeLessThan(c);
      }
    });
  }
});

test("the source guard against bind:clientWidth actually detects it", () => {
  const src = readFileSync(new URL("./ItemGrid.svelte", import.meta.url), "utf8");
  const regressed = src.replace("bind:this={box}", "bind:this={box} bind:clientWidth={width}");
  expect(regressed).toMatch(/\sbind:clientWidth=/);
  // virtual.test.ts must use the working pattern (\s = whitespace), not the typo /sbind:clientWidth=/ that can never match.
  const guard = readFileSync(new URL("./virtual.test.ts", import.meta.url), "utf8");
  expect(guard).toContain(String.raw`/\sbind:clientWidth=/`);
  expect(guard).not.toContain("/sbind:clientWidth=/");
});
