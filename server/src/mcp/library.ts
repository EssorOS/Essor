import { randomUUID } from 'crypto';

import type { Record, Workspace } from '../workspace';

/**
 * Read and write the shared page catalog in the [`Workspace`].
 *
 * A page is a record with `t: 'page'`; its title and position live on the
 * record. Keeping this shape identical to `src/library.rs` means an
 * agent-created page shows up in every editor's sidebar exactly like a locally
 * created one.
 */

export interface PageMeta {
  id: string;
  title: string;
}

/** A fresh page id: UUIDv4 in simple form, matching the editor's ids. */
export function newPageId(): string {
  return randomUUID().replace(/-/g, '');
}

/** Every page, ordered by position then id. */
export function readCatalog(ws: Workspace): PageMeta[] {
  return ws.pageRecords().map((record) => ({
    id: record.id,
    title: (record.title ?? '').trim().length > 0 ? (record.title ?? '') : 'Untitled',
  }));
}

/** Whether the catalog lists a page with `id`. */
export function pageExists(ws: Workspace, id: string): boolean {
  return ws.pageExists(id);
}

/** Add a page to the catalog, appended after every existing page. */
export function addPage(ws: Workspace, id: string, title: string): Record {
  return ws.put({
    id,
    page: id,
    position: ws.nextPagePosition(),
    deleted: false,
    t: 'page',
    title,
  });
}

/** Remove a page from the catalog. */
export function removePage(ws: Workspace, id: string): Record | undefined {
  return ws.remove(id);
}
