<script lang="ts">
  import { onMount } from "svelte";
  import { History, Images, OctagonAlert, Settings as SettingsIcon, Smartphone, Trash2 } from "@lucide/svelte";
  import type { Component } from "svelte";
  import "./app.css";
  import { api } from "./lib/api";
  import { act, app, chooseLibrary, reload, start, useCloudFolderAnyway, type Screen } from "./lib/app.svelte";
  import Activity from "./lib/Activity.svelte";
  import Banners from "./lib/Banners.svelte";
  import Bin from "./lib/Bin.svelte";
  import Dialog from "./lib/Dialog.svelte";
  import DropOverlay from "./lib/DropOverlay.svelte";
  import Gallery from "./lib/Gallery.svelte";
  import Mark from "./lib/Mark.svelte";
  import Pair from "./lib/Pair.svelte";
  import Settings from "./lib/Settings.svelte";
  import StatusBar from "./lib/StatusBar.svelte";

  const nav: Array<{ id: Screen; label: string; icon: Component<{ size?: number; "aria-hidden"?: boolean | "true" | "false" }> }> = [
    { id: "gallery", label: "Gallery", icon: Images },
    { id: "bin", label: "Bin", icon: Trash2 },
    { id: "activity", label: "Activity", icon: History },
    { id: "pair", label: "Pair", icon: Smartphone },
    { id: "settings", label: "Settings", icon: SettingsIcon },
  ];

  onMount(() => {
    let stop: (() => void) | undefined;
    let gone = false;
    start()
      .then((s) => (gone ? s() : (stop = s)))
      .catch((e) => (app.loadError = e instanceof Error ? e.message : String(e)));
    return () => {
      gone = true;
      stop?.();
    };
  });

  // The cloud-folder question is open while the store holds one.
  const cloudOpen = {
    get: () => app.cloudPrompt !== null,
    set: (v: boolean) => {
      if (!v) app.cloudPrompt = null;
    },
  };
</script>

{#if app.loadError && !app.ready}
  <div class="boot error" role="alert">
    <span class="ico danger"><OctagonAlert size={32} aria-hidden="true" /></span>
    <h1>COULD NOT START - {app.loadError}</h1>
    <button class="btn primary" onclick={reload}>Retry</button>
  </div>
{:else if !app.ready}
  <div class="boot"><p class="muted">Starting</p></div>
{:else}
  <div class="shell">
    <nav class="rail" aria-label="Screens">
      <div class="brand"><Mark width={56} /><span>Tugboat</span></div>
      {#each nav as n (n.id)}
        <button class="nav" aria-current={app.screen === n.id ? "page" : undefined} onclick={() => (app.screen = n.id)}>
          <n.icon size={18} aria-hidden="true" />
          <span>{n.label}</span>
          {#if n.id === "pair" && app.pairing.t === "code"}<span class="chip warn">CONFIRM</span>{/if}
        </button>
      {/each}
    </nav>
    <div class="main">
      <Banners />
      <main>
        {#if app.screen === "gallery"}<Gallery />
        {:else if app.screen === "bin"}<Bin />
        {:else if app.screen === "activity"}<Activity />
        {:else if app.screen === "pair"}<Pair />
        {:else}<Settings />{/if}
      </main>
      <StatusBar />
    </div>
  </div>

  <DropOverlay />

  <Dialog bind:open={cloudOpen.get, cloudOpen.set} title="Cloud-synced folder">
    <p>{app.cloudPrompt?.message ?? ""}</p>
    <p class="muted">Photos in a cloud folder are uploaded to the internet. Tugboat is meant to stay on your network.</p>
    {#snippet footer()}
      <button class="btn" onclick={() => (app.cloudPrompt = null)}>Cancel</button>
      <button
        class="btn"
        onclick={async () => {
          app.cloudPrompt = null;
          await chooseLibrary();
        }}>Choose another</button
      >
      <button class="btn danger" onclick={useCloudFolderAnyway}>Use it anyway</button>
    {/snippet}
  </Dialog>

  <Dialog bind:open={app.notSyncedOpen} title="Not synced">
    <p class="muted">These files are not synced. The reason is on each line.</p>
    <ul class="files">
      {#each app.library?.not_synced ?? [] as f, i (i)}
        <li><strong>{f.name}</strong> <span class="chip warn">{f.reason}</span>{#if f.path}<br /><span class="mono muted">{f.path}</span>{/if}</li>
      {:else}
        <li class="muted">Nothing left.</li>
      {/each}
    </ul>
    {#snippet footer()}
      <button class="btn" onclick={() => act(() => api.libraryOpenFolder())}>Open library folder</button>
      <button class="btn primary" onclick={() => (app.notSyncedOpen = false)}>Close</button>
    {/snippet}
  </Dialog>
{/if}

<style>
  .boot {
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 12px;
    padding: 24px;
    text-align: center;
  }
  .boot.error h1 {
    color: var(--danger);
  }
  .shell {
    display: grid;
    grid-template-columns: 176px 1fr;
    height: 100%;
  }
  .rail {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 12px 8px;
    background: var(--rail);
    color: var(--rail-fg);
    border-right: 1px solid var(--border);
    --mark-fg: #ffffff;
  }
  /* Small logo, then the word, on one line (owner 2026-11-18: "[Logo] Tugboat ... so the logo is nice and small"). */
  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 4px 10px 12px;
    font-size: 17px;
    font-weight: 600;
  }
  .nav {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 7px 10px;
    border: 1px solid transparent;
    border-radius: 6px;
    background: none;
    text-align: left;
    cursor: pointer;
  }
  .nav:hover {
    background: var(--rail-hover);
  }
  /* Current screen = tint plus a full 1px border (never a one-edge stripe). */
  .nav[aria-current="page"] {
    background: var(--rail-current);
    border-color: var(--rail-edge);
    font-weight: 600;
  }
  .main {
    display: flex;
    flex-direction: column;
    min-width: 0;
    min-height: 0;
    height: 100%;
  }
  main {
    flex: 1;
    min-height: 0;
  }
  .files {
    list-style: none;
    margin: 12px 0 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }
</style>
