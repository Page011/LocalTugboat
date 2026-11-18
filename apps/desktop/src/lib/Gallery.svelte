<script lang="ts">
  import { untrack } from "svelte";
  import { ExternalLink, FolderOpen, OctagonAlert, Pin, QrCode, Trash2, WifiOff } from "@lucide/svelte";
  import { api } from "./api";
  import { act, app, beginPairing, fail } from "./app.svelte";
  import { fmtSize, itemLabel, retentionLabel } from "./format";
  import Chip from "./Chip.svelte";
  import ItemGrid from "./ItemGrid.svelte";
  import Mark from "./Mark.svelte";
  import { PagedItems } from "./paged.svelte";

  let folder = $state<string | null>(null);
  let selectedId = $state<string | null>(null);
  const list = new PagedItems(() => ({ folder, bin: false }));
  let lastFolder: string | null | undefined;

  // Reload on any change event; a new folder filter starts from the first page. untrack: the
  // reload reads its own rows, which must not become this effect's dependencies.
  $effect(() => {
    void app.itemsRev;
    const f = folder;
    untrack(() => (f !== lastFolder ? ((lastFolder = f), list.reset()) : list.reload()));
  });
  // A filter on a folder that was deleted falls back to All.
  $effect(() => {
    if (folder !== null && !app.folders.some((f) => f.id === folder)) folder = null;
  });

  // A file dropped on the window lands in the folder shown here. Only the folder id goes to Rust (never a path); when
  // this screen goes away the target is None, so a drop elsewhere says "Copy to Inbox".
  $effect(() => {
    void api.dropTargetSet(folder).catch(fail);
  });
  $effect(() => () => void api.dropTargetSet(null).catch(fail));

  const current = $derived(app.folders.find((f) => f.id === folder) ?? null);
  const total = $derived(app.folders.reduce((n, f) => n + f.count, 0));
  const sel = $derived(list.rows.find((r) => r.id === selectedId) ?? null);
  const moveTargets = $derived(app.folders.filter((f) => f.id !== sel?.folder));
  const offline = $derived(app.sync?.state === "offline");

  let grid = $state<ItemGrid | undefined>();
  let allBtn = $state<HTMLButtonElement | undefined>();

  /** Run an action on the selected item. It may leave the list (delete, move out of a filtered
   *  folder) and unmount the bar with the focused button: focus the same place in the grid, or
   *  the folder filter when nothing is left, so a keyboard user does not restart from the top. */
  async function actOnSelected(run: () => Promise<unknown>) {
    const at = list.rows.findIndex((r) => r.id === selectedId);
    if (!(await act(run))) return;
    await list.reload();
    if (list.rows.length > 0) await grid?.focusIndex(at);
    else allBtn?.focus();
  }
</script>

<section class="screen" aria-labelledby="gallery-h">
  <div class="screen-head">
    <h1 id="gallery-h">Gallery</h1>
  </div>

  <div class="folders" role="group" aria-label="Folders">
    <button class="btn" bind:this={allBtn} aria-pressed={folder === null} onclick={() => (folder = null)}>All <span class="muted">{total}</span></button>
    {#each app.folders as f (f.id)}
      <button class="btn" aria-pressed={folder === f.id} title="Retention: {retentionLabel(f.retention)}" onclick={() => (folder = f.id)}>
        {f.name} <span class="muted">{f.count}</span>
      </button>
    {/each}
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
      {#if folder !== null}
        <span class="ico"><FolderOpen size={32} aria-hidden="true" /></span>
        <h2>Nothing in {current?.name ?? "this folder"}.</h2>
        <button class="btn" onclick={() => (folder = null)}>Show all folders</button>
      {:else if app.paired === null}
        <span class="ico"><QrCode size={32} aria-hidden="true" /></span>
        <h2>Pair your phone</h2>
        <p>Scan a code with the Tugboat app. Photos and files then show up here.</p>
        <button class="btn primary" onclick={beginPairing}><QrCode size={16} aria-hidden="true" /> Pair a phone</button>
      {:else if offline}
        <span class="ico"><WifiOff size={32} aria-hidden="true" /></span>
        <h2>Phone offline</h2>
        <p>Photos and files show up here when {app.paired.name} connects.</p>
      {:else}
        <span class="art"><Mark width={72} /></span>
        <h2>Nothing yet.</h2>
        <p>Photos and files from your phone show up here. Drop files here to copy them in.</p>
      {/if}
    </div>
  {:else}
    <ItemGrid bind:this={grid} items={list.rows} labelFor={(it) => itemLabel(it, app.now)} bind:selectedId ariaLabel="Files" onopen={(it) => act(() => api.openItem(it.id))} onend={() => list.loadMore()} />
  {/if}

  {#if sel}
    <div class="bar" role="toolbar" aria-label="Actions for {sel.name}">
      <div class="info">
        <strong>{sel.name}</strong>
        <span class="muted">{fmtSize(sel.size)}</span>
        <Chip label={itemLabel(sel, app.now)} />
      </div>
      <button class="btn" aria-pressed={sel.pinned} onclick={() => act(() => api.setPinned(sel.id, !sel.pinned))}>
        <Pin size={16} aria-hidden="true" /> {sel.pinned ? "Kept" : "Keep"}
      </button>
      <select
        class="btn"
        aria-label="Move to folder"
        value=""
        onchange={(e) => {
          const to = e.currentTarget.value;
          e.currentTarget.value = "";
          if (to) void actOnSelected(() => api.moveItem(sel.id, to));
        }}
      >
        <option value="">Move to...</option>
        {#each moveTargets as f (f.id)}<option value={f.id}>{f.name}</option>{/each}
      </select>
      <button class="btn" disabled={!sel.path} title="Show in Explorer" aria-label="Show in Explorer" onclick={() => act(() => api.revealItem(sel.id))}><FolderOpen size={16} aria-hidden="true" /> <span class="lbl">Show in Explorer</span></button>
      <button class="btn" disabled={!sel.path} title="Open" aria-label="Open" onclick={() => act(() => api.openItem(sel.id))}><ExternalLink size={16} aria-hidden="true" /> <span class="lbl">Open</span></button>
      <button class="btn" onclick={() => actOnSelected(() => api.deleteItem(sel.id))}><Trash2 size={16} aria-hidden="true" /> Delete</button>
    </div>
  {/if}
</section>

<style>
  .folders {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }
</style>
