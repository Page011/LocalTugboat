<script lang="ts">
  // Native modal <dialog>: the browser gives focus trapping, Escape and the backdrop.
  import type { Snippet } from "svelte";
  import { X } from "@lucide/svelte";

  let { open = $bindable(false), title, children, footer }: { open: boolean; title: string; children: Snippet; footer?: Snippet } = $props();
  let el = $state<HTMLDialogElement | undefined>();
  const uid = $props.id();

  $effect(() => {
    if (!el) return;
    if (open && !el.open) el.showModal();
    if (!open && el.open) el.close();
  });
</script>

<dialog bind:this={el} aria-labelledby="{uid}-t" onclose={() => (open = false)}>
  <header>
    <h2 id="{uid}-t">{title}</h2>
    <button class="btn" aria-label="Close" onclick={() => (open = false)}><X size={16} aria-hidden="true" /></button>
  </header>
  <div class="body">{@render children()}</div>
  {#if footer}<footer>{@render footer()}</footer>{/if}
</dialog>

<style>
  dialog {
    width: min(560px, calc(100vw - 32px));
    max-height: calc(100vh - 48px);
    padding: 0;
    border: 1px solid var(--border-strong);
    border-radius: 10px;
    background: var(--surface);
    color: var(--fg);
  }
  dialog[open] {
    display: flex;
    flex-direction: column;
  }
  dialog::backdrop {
    background: var(--scrim);
  }
  header,
  footer {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 12px 16px;
  }
  header {
    justify-content: space-between;
    border-bottom: 1px solid var(--border);
  }
  footer {
    justify-content: flex-end;
    border-top: 1px solid var(--border);
  }
  .body {
    padding: 16px;
    overflow-y: auto;
    min-height: 0;
  }
</style>
