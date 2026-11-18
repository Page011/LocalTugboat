// Activity paging: pure so the cursor maths is tested without a window.

import type { ActivityEntry } from "./bindings/ActivityEntry";

/** Most rows one request may ask for (the Rust side caps it). */
export const MAX_PAGE = 500;

/** Fetch newest-first pages by cursor (`before` = the last id so far) until `want` rows are in or
 *  a short page says the list ended. `more` = there may be another page. A reload that kept only
 *  one request's worth would cut the rows the user had paged in past the cap. */
export async function loadActivityPages(
  fetchPage: (before: number | null, limit: number) => Promise<ActivityEntry[]>,
  want: number,
): Promise<{ rows: ActivityEntry[]; more: boolean }> {
  const rows: ActivityEntry[] = [];
  while (rows.length < want) {
    const limit = Math.min(want - rows.length, MAX_PAGE);
    const page = await fetchPage(rows.at(-1)?.id ?? null, limit);
    rows.push(...page);
    if (page.length < limit) return { rows, more: false };
  }
  return { rows, more: true };
}
