<script lang="ts">
  import { untrack } from "svelte";
  import { OctagonAlert, Trash2, Undo2 } from "@lucide/svelte";
  import { api } from "./api";
  import { act, app } from "./app.svelte";
  import Chip from "./Chip.svelte";
  import { binLabel, fmtSize } from "./format";
  import ItemGrid from "./ItemGrid.svelte";
  import { PagedItems } from "./paged.svelte";

  let selectedId = $state<string | null>(null);
  const list = new PagedItems(() => ({ folder: null, bin: true }));

  $effect(() => {
    void app.itemsRev;
    untrack(() => list.reload());
  });

  let grid = $state<ItemGrid | undefined>();
  let head = $state<HTMLHeadingElement | undefined>();

  /** The restored item leaves the list and the bar unmounts with the focused button: focus the
   *  same place in the grid (or the heading when the Bin is now empty). */
  async function restore() {
    const at = list.rows.findIndex((r) => r.id === selectedId);
    if (!(await act(() => api.restoreItem(selectedId!)))) return;
    await list.reload();
    if (list.rows.length > 0) await grid?.focusIndex(at);
    else head?.focus();
  }

  const days = $derived(app.settings?.bin_days ?? 7);
  const sel = $derived(list.rows.find((r) => r.id === selectedId) ?? null);
</script>

<section class="screen" aria-labelledby="bin-h">
  <div class="screen-head">
    <h1 id="bin-h" tabindex="-1" bind:this={head}>Bin</h1>
    <p class="muted">Items stay here {days} {days === 1 ? "day" : "days"}, then this PC deletes its copy.</p>
  </div>

  {#if list.error}
    <div class="empty error" role="alert">
      <span class="ico danger"><OctagonAlert size={32} aria-hidden="true" /></span>
      <h2>COULD NOT LOAD - {list.error}</h2>
      <button class="btn primary" onclick={() => list.reset()}>Retry</button>
    </div>
  {:else if !list.loaded}
    <div class="empty"><p>Loading</p></div>
  {:else if list.rows.length === 0}
    <div class="empty">
      <span class="ico"><Trash2 size={32} aria-hidden="true" /></span>
      <h2>Bin is empty.</h2>
      <p>Deleted and expired items wait here before they are removed.</p>
    </div>
  {:else}
    <ItemGrid bind:this={grid} items={list.rows} labelFor={(it) => binLabel(it, days, app.now)} bind:selectedId ariaLabel="Items in the Bin" onopen={() => {}} onend={() => list.loadMore()} />
  {/if}

  {#if sel}
    <div class="bar" role="toolbar" aria-label="Actions for {sel.name}">
      <div class="info">
        <strong>{sel.name}</strong>
        <span class="muted">{fmtSize(sel.size)}</span>
        <Chip label={binLabel(sel, days, app.now)} />
      </div>
      <button class="btn primary" onclick={restore}><Undo2 size={16} aria-hidden="true" /> Restore</button>
    </div>
  {/if}
</section>

