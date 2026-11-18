<script lang="ts">
  import { Film, Image as ImageIcon } from "@lucide/svelte";
  import { thumbSrc } from "./api";
  import type { ItemView } from "./api";
  import Chip from "./Chip.svelte";
  import { fileIcon, itemMeta, kindText, peerNote, type Label } from "./format";
  import { fileIcons } from "./icons";

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
  // Only photos and videos have thumbnails; any other file is a tile with an icon for its type (no thumbnail request at all).
  const isFile = $derived(item.kind === "file");
  const Kind = $derived(item.kind === "video" ? Film : isFile ? fileIcons[fileIcon(item.ext)] : ImageIcon);
  const note = $derived(peerNote(item));
</script>

<button
  class="tile"
  class:selected
  role="option"
  aria-selected={selected}
  aria-label="{item.name}, {kindText(item)}, {label.text}{note ? `, ${note}` : ''}"
  title={item.name}
  data-i={index}
  {tabindex}
  onfocus={onselect}
  onclick={onselect}
  ondblclick={onopen}
>
  <span class="thumb">
    {#if isFile}
      <Kind size={40} aria-hidden="true" />
    {:else if src !== null && failedFor !== src}
      <img {src} alt="" loading="lazy" draggable="false" onerror={() => (failedFor = src)} />
    {:else}
      <Kind size={28} aria-hidden="true" />
    {/if}
  </span>
  <span class="name">{item.name}</span>
  <span class="meta"><Kind size={12} aria-hidden="true" /> {itemMeta(item)}{#if note}<span class="peer"> {note}</span>{/if}</span>
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
    max-width: calc(100% - 16px);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--fg-muted);
    font-size: 12px;
  }
  .peer {
    color: var(--warn);
    font-weight: 600;
  }
  .tile.selected .meta {
    color: var(--fg-muted);
  }
</style>
