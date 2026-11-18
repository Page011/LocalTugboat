<script lang="ts">
  import { untrack } from "svelte";
  import { History, OctagonAlert } from "@lucide/svelte";
  import { api } from "./api";
  import type { ActivityEntry } from "./api";
  import { app } from "./app.svelte";
  import Chip from "./Chip.svelte";
  import { actionLabel, fmtTime, isProblem } from "./format";

  const PAGE = 100;
  let rows = $state<ActivityEntry[]>([]);
  let more = $state(false);
  let loaded = $state(false);
  let error = $state<string | null>(null);
  let token = 0;

  /** Reload keeps as many rows as the user had paged in. */
  async function load(append: boolean) {
    const mine = ++token;
    const before = append ? (rows.at(-1)?.id ?? null) : null;
    const limit = append ? PAGE : Math.min(Math.max(rows.length, PAGE), 500);
    try {
      const page = await api.activity(before, limit);
      if (mine !== token) return;
      rows = append ? rows.concat(page) : page;
      more = page.length === limit;
      error = null;
      loaded = true;
    } catch (e) {
      if (mine === token) error = e instanceof Error ? e.message : String(e);
    }
  }

  $effect(() => {
    void app.activityRev;
    untrack(() => load(false));
  });

  function who(e: ActivityEntry): string {
    if (e.device === null) return "System";
    return app.paired && e.device === app.paired.id ? app.paired.name : "This PC";
  }
</script>

<section class="screen" aria-labelledby="activity-h">
  <div class="screen-head">
    <h1 id="activity-h">Activity</h1>
    <p class="muted">Kept for 30 days.</p>
  </div>

  {#if error}
    <div class="empty error" role="alert">
      <span class="ico danger"><OctagonAlert size={32} aria-hidden="true" /></span>
      <h2>COULD NOT LOAD - {error}</h2>
      <button class="btn primary" onclick={() => load(false)}>Retry</button>
    </div>
  {:else if !loaded}
    <div class="empty"><p>Loading</p></div>
  {:else if rows.length === 0}
    <div class="empty">
      <span class="ico"><History size={32} aria-hidden="true" /></span>
      <h2>No activity yet.</h2>
      <p>Photos moved, kept, expired or deleted are listed here.</p>
    </div>
  {:else}
    <div class="scroll">
      <table>
        <thead>
          <tr><th scope="col">When</th><th scope="col">What</th><th scope="col">Details</th><th scope="col">Device</th></tr>
        </thead>
        <tbody>
          {#each rows as e (e.id)}
            <tr>
              <td class="mono nowrap">{fmtTime(e.at)}</td>
              <td class="nowrap">
                {#if isProblem(e.action)}<Chip label={{ text: actionLabel(e.action), tone: "danger", icon: "alert" }} />{:else}{actionLabel(e.action)}{/if}
              </td>
              <td class="detail" title={e.detail}>{e.detail}</td>
              <td class="muted nowrap">{who(e)}</td>
            </tr>
          {/each}
        </tbody>
      </table>
      {#if more}<button class="btn more" onclick={() => load(true)}>Load more</button>{/if}
    </div>
  {/if}
</section>

<style>
  .scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
  }
  table {
    width: 100%;
    border-collapse: collapse;
  }
  th,
  td {
    padding: 6px 10px;
    text-align: left;
    vertical-align: top;
    border-bottom: 1px solid var(--border);
  }
  th {
    position: sticky;
    top: 0;
    background: var(--bg);
    font-weight: 600;
    color: var(--fg-muted);
  }
  .nowrap {
    white-space: nowrap;
  }
  /* One line per row; the full text is the tooltip. max-width:0 lets the cell shrink to the table. */
  .detail {
    max-width: 0;
    width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .more {
    margin: 12px 0;
  }
</style>
