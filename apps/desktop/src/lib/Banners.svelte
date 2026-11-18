<script lang="ts">
  import { OctagonAlert, TriangleAlert } from "@lucide/svelte";
  import { api } from "./api";
  import { act, app, chooseLibrary, locateLibrary, rescanLibrary, resolveMissing } from "./app.svelte";
  import { computeBanners, type BannerAction } from "./banners";
  import { shortPath } from "./format";

  const banners = $derived(computeBanners({ info: app.info, sync: app.sync, library: app.library, firewall: app.firewall, error: app.error }));

  const run: Record<BannerAction, () => unknown> = {
    "fw-fix": () =>
      act(async () => {
        await api.firewallFix();
        app.firewall = await api.firewallRecheck();
      }),
    "fw-recheck": () => act(async () => (app.firewall = await api.firewallRecheck())),
    "lib-locate": locateLibrary,
    "lib-rescan": rescanLibrary,
    "lib-bin": () => resolveMissing(true),
    "lib-wait": () => resolveMissing(false),
    "lib-choose": chooseLibrary,
    "not-synced": () => (app.notSyncedOpen = true),
    "dismiss-error": () => (app.error = null),
  };
</script>

{#if banners.length > 0}
  <div class="banners">
    {#each banners as b (b.id)}
      <div class="banner {b.tone}" role={b.tone === "danger" ? "alert" : "status"}>
        {#if b.tone === "danger"}<OctagonAlert size={16} aria-hidden="true" />{:else}<TriangleAlert size={16} aria-hidden="true" />{/if}
        <span class="text">{b.text}</span>
        {#if b.detail}<span class="detail mono" title={b.detail}>{shortPath(b.detail)}</span>{/if}
        {#each b.actions as a (a.id)}
          <button class="btn" onclick={() => run[a.id]()}>{a.label}</button>
        {/each}
      </div>
    {/each}
  </div>
{/if}

<style>
  .banners {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px 20px 0;
  }
  .banner {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 10px;
    border: 1px solid;
    border-radius: 8px;
  }
  .banner.danger {
    background: var(--danger-bg);
    color: var(--danger);
    border-color: var(--danger);
  }
  .banner.warn {
    background: var(--warn-bg);
    color: var(--warn);
    border-color: var(--warn);
  }
  .text {
    flex: 1 1 auto;
    min-width: 0;
    overflow-wrap: anywhere;
  }
  .detail {
    flex: 0 1 auto;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: inherit;
  }
  .btn {
    color: var(--fg);
    flex: none;
  }
</style>
