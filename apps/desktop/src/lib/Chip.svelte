<script lang="ts">
  import type { Label } from "./format";
  import { icons } from "./icons";

  let { label }: { label: Label } = $props();
  const Icon = $derived(icons[label.icon]);
</script>

<span class="chip {label.tone}"><Icon size={12} aria-hidden="true" /><span>{label.text}</span></span>
