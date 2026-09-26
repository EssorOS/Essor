import { randomUUID } from 'crypto';

import type { Record, Workspace } from '../workspace';
import { between, defaultKey, newAfter } from './fractional';

/**
 * Read and write the Essor block model in a [`Workspace`].
 *
 * A page's blocks are records with `t: 'block'`, each carrying a `kind` and a
 * list of runs. This mirrors `src/page.rs`, so agent-written content is
 * indistinguishable from content the editor wrote.
 */

export const BLOCK_KINDS = ['paragraph', 'heading1', 'heading2', 'bullet'] as const;
export type BlockKind = (typeof BLOCK_KINDS)[number];

/** A run of text sharing the same inline marks. */
export interface TextRun {
  text: string;
  bold?: boolean;
  italic?: boolean;
}

/** A block as accepted from an agent. `runs` takes precedence over `text`. */
export interface BlockInput {
  kind?: BlockKind;
  text?: string;
  runs?: TextRun[];
}

/** A block as returned to an agent, with marks normalised to booleans. */
export interface BlockRecord {
  index: number;
  kind: BlockKind;
  text: string;
  runs: Required<TextRun>[];
}

export function isBlockKind(value: unknown): value is BlockKind {
  return typeof value === 'string' && (BLOCK_KINDS as readonly string[]).includes(value);
}

function newBlockId(): string {
  return randomUUID().replace(/-/g, '');
}

/** Read every live block in order. */
export function readBlocks(ws: Workspace, page: string): BlockRecord[] {
  return ws.blockRecords(page).map((record, index) => {
    const runs = normalizeRuns(record.runs ?? []);
    const kind = isBlockKind(record.kind) ? record.kind : 'paragraph';
    return { index, kind, text: runs.map((run) => run.text).join(''), runs };
  });
}

/** Replace the entire document body with `blocks`, returning the new records. */
export function replaceBlocks(ws: Workspace, page: string, blocks: BlockInput[]): Record[] {
  const changed: Record[] = [];
  for (const record of ws.blockRecords(page)) {
    const tombstone = ws.remove(record.id);
    if (tombstone !== undefined) {
      changed.push(tombstone);
    }
  }
  const positions = sequence(defaultKey(), blocks.length);
  for (const [index, block] of blocks.entries()) {
    changed.push(putBlock(ws, page, positions[index], block));
  }
  return changed;
}

/** Append `blocks` to the end of the page. */
export function appendBlocks(ws: Workspace, page: string, blocks: BlockInput[]): Record[] {
  const existing = ws.blockRecords(page);
  const last = existing[existing.length - 1];
  const start = last === undefined ? defaultKey() : newAfter(last.position);
  const positions = sequence(start, blocks.length);
  return blocks.map((block, index) => putBlock(ws, page, positions[index], block));
}

/** Insert one block at `index` (0..length). */
export function insertBlock(
  ws: Workspace,
  page: string,
  index: number,
  block: BlockInput,
): Record {
  const existing = ws.blockRecords(page);
  const bounded = Math.max(0, Math.min(index, existing.length));
  const lower = bounded > 0 ? existing[bounded - 1].position : null;
  const upper = bounded < existing.length ? existing[bounded].position : null;
  return putBlock(ws, page, positionBetween(lower, upper), block);
}

/** The result of an `updateBlock`: the block and whether the write changed it. */
export interface BlockUpdate {
  record: Record;
  changed: boolean;
}

/**
 * Update the block at `index`, or `undefined` when there is none.
 *
 * When the patch resolves to the block's current kind and runs, the store is
 * left untouched (and `changed` is `false`) so a no-op edit cannot bump the
 * version and clobber a concurrent peer's write under last-write-wins.
 */
export function updateBlock(
  ws: Workspace,
  page: string,
  index: number,
  patch: { kind?: BlockKind; text?: string; runs?: TextRun[] },
): BlockUpdate | undefined {
  const existing = ws.blockRecords(page);
  const record = existing[index];
  if (record === undefined) {
    return undefined;
  }
  const kind = patch.kind ?? (isBlockKind(record.kind) ? record.kind : 'paragraph');
  const runs =
    patch.runs !== undefined || patch.text !== undefined
      ? buildRuns({ runs: patch.runs, text: patch.text })
      : normalizeRuns(record.runs ?? []);
  const currentKind = isBlockKind(record.kind) ? record.kind : 'paragraph';
  const currentRuns = normalizeRuns(record.runs ?? []);
  if (kind === currentKind && runsEqual(runs, currentRuns)) {
    return { record, changed: false };
  }
  const updated = ws.put({
    id: record.id,
    page,
    position: record.position,
    deleted: false,
    t: 'block',
    kind,
    runs,
  });
  return { record: updated, changed: true };
}

/** Delete the block at `index`, returning its tombstone or `undefined`. */
export function deleteBlock(
  ws: Workspace,
  page: string,
  index: number,
): Record | undefined {
  const existing = ws.blockRecords(page);
  const record = existing[index];
  return record === undefined ? undefined : ws.remove(record.id);
}

/** Case-insensitive substring matches with a surrounding excerpt. */
export function findInBlocks(
  blocks: BlockRecord[],
  query: string,
): Array<{ index: number; excerpt: string }> {
  const needle = query.toLowerCase();
  if (needle.length === 0) {
    return [];
  }
  const matches: Array<{ index: number; excerpt: string }> = [];
  for (const block of blocks) {
    const haystack = block.text.toLowerCase();
    const at = haystack.indexOf(needle);
    if (at === -1) {
      continue;
    }
    const start = Math.max(0, at - 40);
    const end = Math.min(block.text.length, at + query.length + 40);
    const prefix = start > 0 ? '…' : '';
    const suffix = end < block.text.length ? '…' : '';
    matches.push({ index: block.index, excerpt: `${prefix}${block.text.slice(start, end)}${suffix}` });
  }
  return matches;
}

/** A plain-text projection: blocks joined by newlines. */
export function documentText(blocks: BlockRecord[]): string {
  return blocks.map((block) => block.text).join('\n');
}

function normalizeRuns(runs: TextRun[]): Required<TextRun>[] {
  return runs.map((run) => ({
    text: run.text,
    bold: run.bold === true,
    italic: run.italic === true,
  }));
}

/** Whether two already-normalized run lists are identical. */
function runsEqual(a: Required<TextRun>[], b: Required<TextRun>[]): boolean {
  return (
    a.length === b.length &&
    a.every(
      (run, index) =>
        run.text === b[index].text && run.bold === b[index].bold && run.italic === b[index].italic,
    )
  );
}

function buildRuns(input: { runs?: TextRun[]; text?: string }): Required<TextRun>[] {
  if (input.runs !== undefined) {
    return normalizeRuns(input.runs);
  }
  const text = input.text ?? '';
  return text.length > 0 ? [{ text, bold: false, italic: false }] : [];
}

/** Create a new block record at `position`. */
function putBlock(ws: Workspace, page: string, position: string, block: BlockInput): Record {
  return ws.put({
    id: newBlockId(),
    page,
    position,
    deleted: false,
    t: 'block',
    kind: block.kind ?? 'paragraph',
    runs: buildRuns(block),
  });
}

/** `count` order keys starting at `start`, each strictly after the last. */
function sequence(start: string, count: number): string[] {
  const keys: string[] = [];
  let key = start;
  for (let i = 0; i < count; i++) {
    keys.push(key);
    key = newAfter(key);
  }
  return keys;
}

/**
 * A position between `lower` and `upper` (either may be absent), matching the
 * editor's `next_position`. When the bounds are not usable the call falls back
 * to appending after the lower bound, exactly as `Library::next_position` does.
 */
function positionBetween(lower: string | null, upper: string | null): string {
  return between(lower, upper) ?? (lower !== null ? newAfter(lower) : defaultKey());
}

