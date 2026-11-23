<script lang="ts">
  import { onMount } from "svelte";
  import { FolderOpen, RefreshCw, Unlink } from "@lucide/svelte";
  import { api } from "./api";
  import { act, app, chooseLibrary, fail, rescanLibrary } from "./app.svelte";
  import Dialog from "./Dialog.svelte";
  import { ago, lastSeenText, parseBinDays } from "./format";
  import { lastWins } from "./generation";
  import { UPDATE_CHECK_HELP, UPDATE_CHECK_LABEL } from "./updates";

  let autostart = $state<boolean | null>(null);
  let updateCheck = $state<boolean | null>(null);
  let binText = $state("");
  let binError = $state<string | null>(null);
  /** The user typed since the last save started: the saved value must not overwrite the text. */
  let binDirty = $state(false);
  let confirmUnpair = $state(false);
  const uid = $props.id();
  // A missing or paused library has no scan to report and no folder to open.
  const ready = $derived(app.library?.health.t === "ready" && !!app.library.path);

  onMount(() => {
    api.autostart().then((v) => (autostart = v)).catch(fail);
    api.updateCheck().then((v) => (updateCheck = v)).catch(fail);
  });

  // Follow the saved value (another save, or the first load), but never overwrite what is being typed.
  $effect(() => {
    if (app.settings && !binDirty && binError === null) binText = String(app.settings.bin_days);
  });

  async function setAutostart(on: boolean) {
    await act(() => api.setAutostart(on));
    autostart = await api.autostart().catch(() => on);
  }

  async function setUpdateCheck(on: boolean) {
    await act(() => api.setUpdateCheck(on));
    updateCheck = await api.updateCheck().catch(() => on);
  }

  // One save at a time, the LAST submitted value wins: a second submit while a save runs is queued, never skipped
  // as "unchanged" against the old saved value.
  let binSaveFailed = false;
  const submitBin = lastWins<number>(
    () => app.settings?.bin_days ?? null,
    async (days) => {
      binSaveFailed = true;
      if (!app.settings) return false;
      const next = { ...app.settings, bin_days: days };
      if (!(await act(() => api.saveSettings(next)))) return false;
      app.settings = next;
      binSaveFailed = false;
      return true;
    },
  );

  async function saveBin() {
    const text = binText;
    const r = parseBinDays(text);
    if (!r.ok) {
      binDirty = false;
      binError = r.message;
      return;
    }
    binError = null;
    // Stay dirty while the save runs and if it fails: the effect above must not put the old saved value back
    // over the typed text. After the run, release only when it worked and nothing was typed meanwhile.
    await submitBin(r.days);
    // (Settings never loaded: nothing could be saved; release, so the field follows the value once it loads.)
    if (binText === text && (!binSaveFailed || !app.settings || r.days === app.settings.bin_days)) binDirty = false;
  }

  async function unpair() {
    confirmUnpair = false;
    if (await act(() => api.unpair())) {
      app.paired = await api.pairedDevice().catch(() => null);
      app.pairing = { t: "idle" };
    }
  }
</script>

<section class="screen" aria-labelledby="settings-h">
  <div class="screen-head"><h1 id="settings-h">Settings</h1></div>
  <div class="scroll">
    <div class="card block">
      <h2>Library folder</h2>
      <p class="mono">{app.library?.path ?? "Not chosen"}</p>
      {#if ready}<p class="muted">Last scan {ago(app.library?.last_scan_at ?? null, app.now)}.</p>{/if}
      <div class="row">
        <button class="btn" onclick={chooseLibrary}><FolderOpen size={16} aria-hidden="true" /> Change folder</button>
        <button class="btn" disabled={!ready} onclick={() => act(() => api.libraryOpenFolder())}><FolderOpen size={16} aria-hidden="true" /> Open in Explorer</button>
        <button class="btn" onclick={rescanLibrary}><RefreshCw size={16} aria-hidden="true" /> Scan now</button>
      </div>
    </div>

    <div class="card block">
      <h2>General</h2>
      <label class="field">
        <input type="checkbox" checked={autostart ?? false} disabled={autostart === null} onchange={(e) => setAutostart(e.currentTarget.checked)} />
        Start with Windows
      </label>
      <div class="stack">
        <label class="field">
          <input type="checkbox" checked={updateCheck ?? true} disabled={updateCheck === null} aria-describedby="{uid}-update-help" onchange={(e) => setUpdateCheck(e.currentTarget.checked)} />
          {UPDATE_CHECK_LABEL}
        </label>
        <span id="{uid}-update-help" class="muted">{UPDATE_CHECK_HELP}</span>
      </div>
      <div class="field">
        <label for="{uid}-bin">Bin days</label>
        <input
          id="{uid}-bin"
          class="num"
          inputmode="numeric"
          bind:value={binText}
          oninput={() => (binDirty = true)}
          aria-invalid={binError !== null}
          aria-describedby="{uid}-bin-help"
          onchange={saveBin}
          onkeydown={(e) => e.key === "Enter" && saveBin()}
        />
        <span id="{uid}-bin-help" class:error={binError !== null} class:muted={binError === null}>{binError ?? "Days a deleted or expired item waits before this PC removes its file."}</span>
      </div>
    </div>

    <div class="card block">
      <h2>Phone</h2>
      {#if app.paired}
        <p><strong>Linked to {app.paired.name}</strong></p>
        <p class="muted">Linked {ago(app.paired.paired_at, app.now)} - {lastSeenText(app.paired, app.sync, app.now).toLowerCase()}</p>
        <div><button class="btn danger" onclick={() => (confirmUnpair = true)}><Unlink size={16} aria-hidden="true" /> Log out</button></div>
      {:else}
        <p class="muted">Not linked to a phone.</p>
      {/if}
    </div>

    <div class="card block">
      <h2>About</h2>
      <p class="muted">Local Tugboat {app.info?.version ?? ""} on {app.info?.device_name ?? ""}</p>
      <p class="muted">Listening on <span class="mono">{app.info?.bind ?? ""}</span></p>
      <p class="muted">Data in <span class="mono">{app.info?.data_dir ?? ""}</span></p>
    </div>
  </div>
</section>

<Dialog bind:open={confirmUnpair} title="Log out of {app.paired?.name ?? 'phone'}?">
  <p>Files stay on this PC.</p>
  {#snippet footer()}
    <button class="btn" onclick={() => (confirmUnpair = false)}>Cancel</button>
    <button class="btn danger" onclick={unpair}>Log out</button>
  {/snippet}
</Dialog>

<style>
  .scroll {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .block {
    display: flex;
    flex-direction: column;
    gap: 8px;
    align-items: flex-start;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    gap: 8px;
  }
  .stack {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .field {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .num {
    width: 72px;
    padding: 4px 8px;
    border: 1px solid var(--border-strong);
    border-radius: 6px;
    background: var(--surface);
  }
  .num[aria-invalid="true"] {
    border-color: var(--danger);
  }
  .error {
    color: var(--danger);
  }
</style>
