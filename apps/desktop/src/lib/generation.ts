// Stale-answer guard shared by the loaders in app.svelte.ts (plain TS so bun test can run it).

/** Bump every counter and return a check: true while nobody has bumped that counter since. Two overlapping
 *  full reloads therefore each own their claim; the older one fails every check once the newer one started. */
export function claimAll<K extends string>(gen: Record<K, number>): (k: K) => boolean {
  const mine = {} as Record<K, number>;
  for (const k of Object.keys(gen) as K[]) mine[k] = ++gen[k];
  return (k) => mine[k] === gen[k];
}
