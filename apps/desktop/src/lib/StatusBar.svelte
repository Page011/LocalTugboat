<script lang="ts">
  import { app } from "./app.svelte";
  import { syncLabel } from "./format";
  import { dropPercent, dropProgressText } from "./drop";
  import { icons } from "./icons";

  const label = $derived(app.sync ? syncLabel(app.sync, app.now) : null);
  const Icon = $derived(label ? icons[label.icon] : null);
  const progress = $derived(app.drop.showProgress ? app.drop.progress : null);
  const note = $derived(app.drop.note?.text ?? null);
</script>

<footer class="status">
  {#if label && Icon}
    <!-- Icon + text + tone: sync state never depends on colour. No live region: the text carries a clock that changes every minute. -->
    <span class="state {label.tone}"><Icon size={14} aria-hidden="true" /><span>{label.text}</span></span>
  {:else}
    <span class="muted">Starting</span>
  {/if}
  <!-- A drop in progress or just done: docked here, so it never covers the selection bar. -->
  {#if progress}
    <span class="drop" role="status">
      <span class="dtext">{dropProgressText(progress)}</span>
      <progress max="100" value={dropPercent(progress) ?? undefined} aria-label={dropProgressText(progress)}></progress>
    </span>
  {:else if note}
    <span class="drop dtext" role="status">{note}</span>
  {/if}
  <span class="grow"></span>
  {#if app.info}<span class="muted">v{app.info.version}</span>{/if}
</footer>

<style>
  .status {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 6px 20px;
    border-top: 1px solid var(--border);
    background: var(--surface);
    font-size: 13px;
  }
  .state {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .state span {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .state.ok {
    color: var(--ok);
  }
  .state.warn {
    color: var(--warn);
  }
  .state.danger {
    color: var(--danger);
  }
  .drop {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .dtext {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  progress {
    width: 120px;
    accent-color: var(--accent);
  }
  .grow {
    flex: 1;
  }
</style>
