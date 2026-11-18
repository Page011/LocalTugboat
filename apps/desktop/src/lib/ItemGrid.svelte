<script lang="ts">
  // Virtualised photo grid: only the rows in view (plus a little overscan) are in the DOM, so a
  // library of 10k items costs the same as 100. Arrow keys move the selection (roving tabindex:
  // one tile is a Tab stop, not hundreds).
  import { tick } from "svelte";
  import type { ItemView } from "./api";
  import type { Label } from "./format";
  import ItemTile from "./ItemTile.svelte";
  import { arrowTarget, GRID, gridLayout, innerWidth, tileWidth, visibleRows } from "./virtual";

  let {
    items,
    labelFor,
    selectedId = $bindable(null),
    ariaLabel,
    onopen,
    onend,
  }: {
    items: ItemView[];
    labelFor: (it: ItemView) => Label;
    selectedId: string | null;
    ariaLabel: string;
    onopen: (it: ItemView) => void;
    /** The last rows came into view: load the next page. */
    onend: () => void;
  } = $props();

  // TILE_W is the narrowest a tile gets; the tiles of a row then share the leftover width, so a
  // wide window has no dead strip on the right.
  const { tileMin: TILE_W, tileH: TILE_H, gap: GAP, pad: PAD } = GRID;

  let box = $state<HTMLDivElement | undefined>();
  let width = $state(0);
  let height = $state(0);
  let scrollTop = $state(0);
  let focusIdx = $state(0);

  const inner = $derived(innerWidth(width, PAD));
  const layout = $derived(gridLayout(items.length, inner, TILE_W, TILE_H, GAP));
  const tileW = $derived(tileWidth(inner, layout.cols, GAP, TILE_W));
  const range = $derived(visibleRows(layout, scrollTop, height));
  const start = $derived(range[0] * layout.cols);
  const end = $derived(Math.min(items.length, range[1] * layout.cols));
  const shown = $derived(items.slice(start, end));
  // The Tab stop must be a tile that exists: fall back to the first one on screen.
  const stop = $derived(focusIdx >= start && focusIdx < end ? focusIdx : start);

  // Measure the scroller's own clientWidth, which is the visible width WITHOUT the vertical scrollbar. (Svelte's
  // bind:clientWidth reports the width the scrollbar sits inside, so the columns were sized for room the scrollbar
  // took: a clipped last column and a horizontal scrollbar, worst with the always-visible classic scrollbar of a
  // dark colour-scheme window.)
  $effect(() => {
    const el = box;
    if (!el) return;
    const measure = () => {
      width = el.clientWidth;
      height = el.clientHeight;
    };
    measure();
    const watcher = new ResizeObserver(measure);
    watcher.observe(el);
    return () => watcher.disconnect();
  });

  $effect(() => {
    if (items.length > 0 && end >= items.length - layout.cols * 2) onend();
  });

  /** Select and focus tile `index` (clamped), scrolling it into view. Used by the keys and by
   *  screens that act on the selected item: the item leaves the list, focus must not drop to <body>. */
  export async function focusIndex(index: number): Promise<void> {
    if (!box || items.length === 0) return;
    const target = Math.min(Math.max(0, index), items.length - 1);
    focusIdx = target;
    selectedId = items[target]?.id ?? null;
    const top = Math.floor(target / layout.cols) * layout.rowHeight;
    if (top < box.scrollTop) box.scrollTop = top;
    else if (top + TILE_H > box.scrollTop + height) box.scrollTop = top + TILE_H - height;
    await tick();
    box.querySelector<HTMLElement>(`[data-i="${target}"]`)?.focus();
  }

  function onkeydown(e: KeyboardEvent) {
    if (e.key === "Enter") {
      const it = items[stop];
      if (it) onopen(it);
      return;
    }
    const target = arrowTarget(stop, e.key, layout.cols, items.length);
    if (target === null) return;
    e.preventDefault();
    void focusIndex(target);
  }
</script>

<!-- svelte-ignore a11y_interactive_supports_focus -->
<div
  class="grid"
  role="listbox"
  aria-label={ariaLabel}
  tabindex="-1"
  bind:this={box}
  onscroll={() => (scrollTop = box?.scrollTop ?? 0)}
  {onkeydown}
>
  <div class="sizer" style:height="{layout.totalHeight}px">
    {#each shown as it, k (it.id)}
      {@const i = start + k}
      <div class="cell" style:top="{Math.floor(i / layout.cols) * layout.rowHeight}px" style:left="{(i % layout.cols) * (tileW + GAP)}px" style:width="{tileW}px" style:height="{TILE_H}px">
        <ItemTile
          item={it}
          label={labelFor(it)}
          index={i}
          selected={it.id === selectedId}
          tabindex={i === stop ? 0 : -1}
          onselect={() => {
            focusIdx = i;
            selectedId = it.id;
          }}
          onopen={() => onopen(it)}
        />
      </div>
    {/each}
  </div>
</div>

<style>
  .grid {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    /* The tiles are laid out to fit the visible width exactly: never a horizontal scrollbar. The gutter is always
       reserved so the width does not change when the vertical scrollbar appears (no relayout loop). */
    overflow-x: hidden;
    scrollbar-gutter: stable;
    outline: none;
  }
  .sizer {
    position: relative;
    margin: 4px; /* PAD */
  }
  .cell {
    position: absolute;
  }
</style>
