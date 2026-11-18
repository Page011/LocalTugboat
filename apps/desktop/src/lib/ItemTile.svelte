<script lang="ts">
  import { Film, Image as ImageIcon } from "@lucide/svelte";
  import { thumbSrc } from "./api";
  import type { ItemView } from "./api";
  import Chip from "./Chip.svelte";
  import { fmtSize, type Label } from "./format";

  let {
    item,
    label,
    index,
    selected,
    tabindex,
    onselect,
    onopen,
  }: {
    item: ItemView;
    label: Label;
    index: number;
    selected: boolean;
    tabindex: number;
    onselect: () => void;
    onopen: () => void;
  } = $props();

  // No thumbnail yet (none held, or a format the Shell cannot render) shows an icon. A load error is
  // remembered against the URL that failed, so the new URL made when the thumbnail appears is tried.
  const src = $derived(thumbSrc(item));
  let failedFor = $state<string | null>(null);
  const Kind = $derived(item.kind === "video" ? Film : ImageIcon);
</script>

<button
  class="tile"
  class:selected
  role="option"
  aria-selected={selected}
  aria-label="{item.name}, {item.kind}, {label.text}"
  title={item.name}
  data-i={index}
  {tabindex}
  onfocus={onselect}
  onclick={onselect}
  ondblclick={onopen}
>
  <span class="thumb">
    {#if src !== null && failedFor !== src}
      <img {src} alt="" loading="lazy" draggable="false" onerror={() => (failedFor = src)} />
    {:else}
      <Kind size={28} aria-hidden="true" />
    {/if}
  </span>
  <span class="name">{item.name}</span>
  <span class="meta"><Kind size={12} aria-hidden="true" /> {item.kind === "video" ? "Video" : "Photo"}, {fmtSize(item.size)}</span>
  <Chip {label} />
</button>

<style>
  .tile {
    width: 100%;
    height: 100%;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 3px;
    padding: 0 0 6px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface);
    text-align: left;
    cursor: pointer;
    overflow: hidden;
  }
  .tile:hover {
    background: var(--surface-2);
  }
  /* Selected = tint plus a full 1px border (never a one-edge stripe). */
  .tile.selected {
    background: var(--accent-tint);
    border-color: var(--accent);
  }
  .thumb {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 100%;
    height: 118px;
    flex: none;
    background: var(--surface-2);
    color: var(--fg-muted);
  }
  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: cover;
  }
  .name,
  .meta,
  .tile :global(.chip) {
    margin: 0 8px;
  }
  .name {
    max-width: calc(100% - 16px);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-weight: 600;
  }
  .meta {
    color: var(--fg-muted);
    font-size: 12px;
  }
  .tile.selected .meta {
    color: var(--fg-muted);
  }
</style>
