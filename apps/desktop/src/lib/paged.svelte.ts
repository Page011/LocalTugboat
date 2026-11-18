// Paged item list shared by Gallery and Bin: first page on load, next page on demand, and a
// reload that keeps as many rows as the user had scrolled through (so an event does not throw
// them back to the top).

import { api, ApiError } from "./api";
import type { FolderId, ItemView } from "./api";

const PAGE = 120;

export class PagedItems {
  rows = $state<ItemView[]>([]);
  hasMore = $state(false);
  loading = $state(false);
  /** The first load finished (so "empty" is a fact, not "not loaded yet"). */
  loaded = $state(false);
  error = $state<string | null>(null);

  #next: string | null = null;
  /** Bumped by every reload/reset; an older request that finishes late is discarded. */
  #token = 0;
  readonly #query: () => { folder: FolderId | null; bin: boolean };

  constructor(query: () => { folder: FolderId | null; bin: boolean }) {
    this.#query = query;
  }

  /** Forget the rows (another folder was picked) and load again. */
  reset(): Promise<void> {
    this.rows = [];
    this.#next = null;
    this.hasMore = false;
    this.loaded = false;
    return this.reload();
  }

  async reload(): Promise<void> {
    const token = ++this.#token;
    const want = Math.max(this.rows.length, PAGE);
    this.loading = true;
    try {
      const q = this.#query();
      let rows: ItemView[] = [];
      let after: string | null = null;
      do {
        const page = await api.items({ ...q, after, limit: Math.min(want - rows.length, 500) });
        rows = rows.concat(page.rows);
        after = page.next;
      } while (after !== null && rows.length < want);
      if (token !== this.#token) return;
      this.rows = rows;
      this.#next = after;
      this.hasMore = after !== null;
      this.error = null;
      this.loaded = true;
    } catch (e) {
      if (token === this.#token) this.error = e instanceof ApiError || e instanceof Error ? e.message : String(e);
    } finally {
      if (token === this.#token) this.loading = false;
    }
  }

  async loadMore(): Promise<void> {
    if (this.#next === null || this.loading) return;
    const token = this.#token;
    this.loading = true;
    try {
      const page = await api.items({ ...this.#query(), after: this.#next, limit: PAGE });
      if (token !== this.#token) return;
      this.rows = this.rows.concat(page.rows);
      this.#next = page.next;
      this.hasMore = page.next !== null;
      this.error = null;
    } catch (e) {
      if (token === this.#token) this.error = e instanceof Error ? e.message : String(e);
    } finally {
      if (token === this.#token) this.loading = false;
    }
  }
}
