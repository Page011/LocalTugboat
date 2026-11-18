// Stale-answer guard shared by the loaders in app.svelte.ts (plain TS so bun test can run it).

/** Bump every counter and return a check: true while nobody has bumped that counter since. Two overlapping
 *  full reloads therefore each own their claim; the older one fails every check once the newer one started. */
export function claimAll<K extends string>(gen: Record<K, number>): (k: K) => boolean {
  const mine = {} as Record<K, number>;
  for (const k of Object.keys(gen) as K[]) mine[k] = ++gen[k];
  return (k) => mine[k] === gen[k];
}

/** Start a run of something that can overlap itself (a full reload) and return a check: true only for the newest
 *  run. Only the newest run may report the outcome (ready / load error): an older one finishing late must not
 *  declare success over a newer failure. */
export function startRun(counter: { n: number }): () => boolean {
  const mine = ++counter.n;
  return () => mine === counter.n;
}

export type RunOutcome = { ok: true } | { ok: false; error: string };

/** The ready / load-error decision of a reload, as a pure function: the next state for a finished run. Only the
 *  newest run (`latest()` true) may change it; an older run finishing late returns the state unchanged, so it can
 *  neither declare success over a newer failure nor show an error a newer success already cleared. */
export function reportRun(
  latest: () => boolean,
  outcome: RunOutcome,
  state: { ready: boolean; loadError: string | null },
): { ready: boolean; loadError: string | null } {
  if (!latest()) return { ready: state.ready, loadError: state.loadError };
  return outcome.ok ? { ready: true, loadError: null } : { ready: state.ready, loadError: outcome.error };
}

/** Save the LAST submitted value, one save at a time. `current()` is the value saved so far (null = unknown: nothing
 *  is saved); a value equal to it is skipped, but only when nothing is in flight. While a save runs, a newer
 *  value replaces the one waiting and goes out right after, so the submit order can never end on an older value.
 *  Every caller gets the promise of the whole run. `save` returns whether it worked and must update what `current()` reads. */
export function lastWins<T>(current: () => T | null, save: (v: T) => Promise<boolean>): (v: T) => Promise<void> {
  let waiting: { v: T } | null = null;
  let running: Promise<void> | null = null;
  const drain = async (): Promise<void> => {
    // Never finish in the submitting task: a second submit in that same task must still find this run in flight
    // (and be picked up by the loop below), not an already-settled promise.
    await null;
    try {
      while (waiting !== null) {
        const { v } = waiting;
        waiting = null;
        if (v !== current()) await save(v);
      }
    } finally {
      running = null; // same task as the loop's last check: no submit can slip in between
    }
  };
  return (v) => {
    waiting = { v };
    if (running === null) running = drain();
    return running;
  };
}
