// Row virtualisation for the gallery grid: fixed-size tiles, so which rows are on screen is
// arithmetic. Pure so the edge cases (zero width, scrolled past the end) are tested.

/** Gallery tile geometry in px: narrowest tile, tile height, gap between tiles, padding around the grid (room for the
 *  2px focus ring + 2px offset, or the first row and column are clipped). */
export const GRID = { tileMin: 168, tileH: 188, gap: 12, pad: 4 } as const;

/** Width the tiles may use, from the scroller's clientWidth (the visible width, scrollbar already excluded). */
export function innerWidth(clientWidth: number, pad: number): number {
  return Math.max(0, clientWidth - 2 * pad);
}

export interface GridLayout {
  cols: number;
  rows: number;
  rowHeight: number;
  totalHeight: number;
}

export function gridLayout(count: number, width: number, tileW: number, tileH: number, gap: number): GridLayout {
  const cols = Math.max(1, Math.floor((width + gap) / (tileW + gap)));
  const rows = Math.ceil(count / cols);
  const rowHeight = tileH + gap;
  return { cols, rows, rowHeight, totalHeight: rows === 0 ? 0 : rows * rowHeight - gap };
}

/** Rows to render for a viewport, with `overscan` extra rows each side: [first, last). */
export function visibleRows(layout: GridLayout, scrollTop: number, viewport: number, overscan = 2): [number, number] {
  if (layout.rows === 0) return [0, 0];
  const last = Math.min(layout.rows, Math.ceil((scrollTop + viewport) / layout.rowHeight) + overscan);
  const first = Math.min(Math.max(0, Math.floor(scrollTop / layout.rowHeight) - overscan), layout.rows - 1);
  return [first, Math.max(last, first + 1)];
}

/** Index of the focused item. The grid keeps a number, but items arrive at the top during sync and
 *  shift every index: follow the selected item by id, and use `fallback` only when it is not in the list. */
export function focusIndexFor(items: ReadonlyArray<{ id: string }>, selectedId: string | null, fallback: number): number {
  const i = selectedId === null ? -1 : items.findIndex((it) => it.id === selectedId);
  return i >= 0 ? i : fallback;
}

/** Item index after an arrow key. Does not wrap; null = the key does not move. */
export function arrowTarget(index: number, key: string, cols: number, count: number): number | null {
  const step = ({ ArrowLeft: -1, ArrowRight: 1, ArrowUp: -cols, ArrowDown: cols } as Record<string, number>)[key];
  if (step === undefined || count === 0) return null;
  const next = index + step;
  if (next >= 0 && next < count) return next;
  // Down from a short last row lands on the last item instead of doing nothing.
  return step === cols && cols > 1 && index < count - 1 ? count - 1 : null;
}

/** Width of one tile when the tiles of a row share the whole row (no dead strip on the right).
 *  Never narrower than `min`, which is also what an unmeasured (zero-width) grid gets. */
export function tileWidth(width: number, cols: number, gap: number, min: number): number {
  return Math.max(min, Math.floor((width - gap * (cols - 1)) / cols));
}
