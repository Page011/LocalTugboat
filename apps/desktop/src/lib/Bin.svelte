<script lang="ts">
  import { untrack } from "svelte";
  import { OctagonAlert, Trash2, Undo2 } from "@lucide/svelte";
  import { api } from "./api";
  import { act, app, fail } from "./app.svelte";
  import Chip from "./Chip.svelte";
  import Dialog from "./Dialog.svelte";
  import { DELETE_FOREVER_TEXT, DELETE_FOREVER_TITLE, EMPTY_BIN_TITLE, binLabel, emptyBinText, fmtSize, peerNote } from "./format";
  import ItemGrid from "./ItemGrid.svelte";
  import { PagedItems } from "./paged.svelte";

  let selectedId = $state<string | null>(null);
  let confirmOne = $state(false);
  let confirmAll = $state(false);
  let emptyCount = $state(0);
  let emptyToken = $state("");
  const list = new PagedItems(() => ({ folder: null, bin: true }));

  $effect(() => {
    void app.itemsRev;
    untrack(() => list.reload());
  });

  let grid = $state<ItemGrid | undefined>();
  let head = $state<HTMLHeadingElement | undefined>();

  /** The restored or deleted item leaves the list and the bar unmounts with the focused button: focus the
   *  same place in the grid (or the heading when the Bin is now empty). */
  async function leave(run: () => Promise<unknown>) {
    const at = list.rows.findIndex((r) => r.id === selectedId);
    if (!(await act(run))) return;
    await list.reload();
    if (list.rows.length > 0) await grid?.focusIndex(at);
    else head?.focus();
  }

  const restore = () => leave(() => api.restoreItem(selectedId!));

  function deleteForever() {
    confirmOne = false;
    const id = selectedId!;
    return leave(() => api.purgeItem(id));
  }

  /** The count in the question is read now, from the Core, not from the pages loaded so far. The token names
   *  those exact items: Empty Bin deletes only while the Bin still lists them. */
  async function askEmpty() {
    try {
      const asked = await api.binCount();
      emptyCount = asked.count;
      emptyToken = asked.token;
    } catch (e) {
      fail(e);
      return;
    }
    confirmAll = emptyCount > 0;
    if (emptyCount === 0) await list.reload();
  }

  async function emptyBin() {
    confirmAll = false;
    if (!(await act(() => api.emptyBin(emptyCount, emptyToken)))) {
      // The Bin changed under the question: show what it holds now, the next Empty Bin asks again.
      await list.reload();
      return;
    }
    selectedId = null;
    await list.reload();
    head?.focus();
  }

  const days = $derived(app.settings?.bin_days ?? 7);
  const sel = $derived(list.rows.find((r) => r.id === selectedId) ?? null);
</script>

<section class="screen" aria-labelledby="bin-h">
  <div class="screen-head">
    <h1 id="bin-h" tabindex="-1" bind:this={head}>Bin</h1>
    <p class="muted">Items stay here {days} {days === 1 ? "day" : "days"}, then this PC deletes its copy.</p>
    <button class="btn danger" disabled={list.rows.length === 0} onclick={askEmpty}><Trash2 size={16} aria-hidden="true" /> Empty Bin</button>
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
        {#if peerNote(sel)}<Chip label={{ text: peerNote(sel) ?? "", tone: "warn", icon: "alert" }} />{/if}
      </div>
      <button class="btn primary" onclick={restore}><Undo2 size={16} aria-hidden="true" /> Restore</button>
      <button class="btn danger" onclick={() => (confirmOne = true)}><Trash2 size={16} aria-hidden="true" /> Delete forever</button>
    </div>
  {/if}
</section>

<Dialog bind:open={confirmOne} title={DELETE_FOREVER_TITLE}>
  <p>{DELETE_FOREVER_TEXT}</p>
  {#snippet footer()}
    <button class="btn" onclick={() => (confirmOne = false)}>Cancel</button>
    <button class="btn danger" onclick={deleteForever}>Delete forever</button>
  {/snippet}
</Dialog>

<Dialog bind:open={confirmAll} title={EMPTY_BIN_TITLE}>
  <p>{emptyBinText(emptyCount)}</p>
  {#snippet footer()}
    <button class="btn" onclick={() => (confirmAll = false)}>Cancel</button>
    <button class="btn danger" onclick={emptyBin}>Empty Bin</button>
  {/snippet}
</Dialog>

