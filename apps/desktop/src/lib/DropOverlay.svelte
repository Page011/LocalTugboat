<script lang="ts">
  import { Upload } from "@lucide/svelte";
  import { app } from "./app.svelte";
  import { dropSub, dropTitle } from "./drop";

  const over = $derived(app.drop.over);
</script>

<!-- Files over the window: names the folder they will be copied to. It never takes the pointer, so the native drop reaches the window.
     A polite live region that is always in the page (so a screen reader hears the target and "FOLDERS NOT COPIED" the moment it fills). Progress and the result line live in the status bar. -->
<div class="sr" role="status" aria-live="polite">{over ? [dropTitle(over), dropSub(over)].filter(Boolean).join(". ") : ""}</div>

{#if over}
  <div class="veil" role="presentation">
    <div class="msg">
      <Upload size={32} aria-hidden="true" />
      <h2>{dropTitle(over)}</h2>
      {#if dropSub(over)}<p class="warn">{dropSub(over)}</p>{/if}
    </div>
  </div>
{/if}

<style>
  .sr {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
  }
  /* A tint and a full 1px-style border all round (never a one-edge stripe). */
  .veil {
    position: fixed;
    inset: 8px;
    z-index: 50;
    display: flex;
    align-items: center;
    justify-content: center;
    pointer-events: none;
    background: color-mix(in srgb, var(--accent-tint) 88%, transparent);
    border: 2px dashed var(--accent);
    border-radius: 12px;
  }
  .msg {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    padding: 18px 28px;
    background: var(--surface);
    color: var(--fg);
    text-align: center;
    border: 1px solid var(--border);
    border-radius: 10px;
  }
  .msg h2 {
    margin: 0;
  }
  .warn {
    margin: 0;
    padding: 2px 10px;
    border-radius: 999px;
    background: var(--warn-bg);
    color: var(--warn);
    font-weight: 600;
  }
</style>
