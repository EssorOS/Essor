import { createHash } from 'node:crypto';
import { DatabaseSync } from 'node:sqlite';

import { defaultKey, newAfter } from './mcp/fractional';

/**
 * The record store shared by websocket sync and the MCP endpoint.
 *
 * A workspace is a flat set of records keyed by id and merged by last-write-wins
 * on a Lamport clock. Pages and blocks are both records, distinguished by `t`.
 * The merge is one guarded upsert with `RETURNING`; the SQL mirrors
 * `src/workspace.rs` exactly, so the conflict rule is written once per runtime
 * and cannot drift.
 */

export interface Clock {
  c: number;
  client: number;
}

/** Whether version `a` wins over `b` under last-write-wins. */
export function versionGreater(a: Clock, b: Clock): boolean {
  return a.c !== b.c ? a.c > b.c : a.client > b.client;
}

export interface Run {
  text: string;
  bold: boolean;
  italic: boolean;
}

export interface Record {
  id: string;
  page: string;
  position: string;
  version: Clock;
  deleted: boolean;
  t: 'page' | 'block';
  title?: string;
  kind?: string;
  runs?: Run[];
}

/** A record before the store assigns it a fresh version. */
export type NewRecord = Omit<Record, 'version'>;

const SCHEMA = `
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS records (
  id             TEXT PRIMARY KEY,
  page           TEXT NOT NULL,
  rtype          TEXT NOT NULL,
  position       TEXT NOT NULL,
  version_c      INTEGER NOT NULL,
  version_client INTEGER NOT NULL,
  deleted        INTEGER NOT NULL,
  data           TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS records_page ON records(page);
CREATE TABLE IF NOT EXISTS pages (
  id     TEXT PRIMARY KEY,
  digest TEXT NOT NULL
) STRICT;
`;

const UPSERT = `
INSERT INTO records (id, page, rtype, position, version_c, version_client, deleted, data)
VALUES (?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
  page = excluded.page,
  rtype = excluded.rtype,
  position = excluded.position,
  version_c = excluded.version_c,
  version_client = excluded.version_client,
  deleted = excluded.deleted,
  data = excluded.data
WHERE (excluded.version_c, excluded.version_client)
    > (records.version_c, records.version_client)
RETURNING id
`;

const SELECT_COLUMNS = 'id, page, rtype, position, version_c, version_client, deleted, data';

export class Workspace {
  private readonly db: DatabaseSync;
  readonly clientId: number;
  private counter: number;
  /** Pages whose stored digest is stale and must be recomputed lazily. */
  private readonly pendingDigests = new Set<string>();

  constructor(path: string) {
    this.db = new DatabaseSync(path);
    this.db.exec(SCHEMA);
    this.clientId = loadClientId(this.db);
    const row = this.db
      .prepare('SELECT COALESCE(MAX(version_c), 0) AS c FROM records')
      .get() as { c: number } | undefined;
    this.counter = Number(row?.c ?? 0);
    // Rebuild digests from the records so a crash between a write and its digest
    // update cannot leave a stale digest that hides a change from the handshake.
    this.rebuildDigests();
  }

  close(): void {
    this.db.close();
  }

  /** A fresh version, one past every counter seen so far. */
  nextClock(): Clock {
    this.counter += 1;
    return { c: this.counter, client: this.clientId };
  }

  get(id: string): Record | undefined {
    const row = this.db
      .prepare(`SELECT ${SELECT_COLUMNS} FROM records WHERE id = ?`)
      .get(id);
    return row === undefined ? undefined : rowToRecord(row as unknown as Row);
  }

  pageRecords(): Record[] {
    return this.query(
      `SELECT ${SELECT_COLUMNS} FROM records WHERE rtype = 'page' AND deleted = 0 ORDER BY position, id`,
    );
  }

  blockRecords(page: string): Record[] {
    return this.query(
      `SELECT ${SELECT_COLUMNS} FROM records WHERE page = ? AND rtype = 'block' AND deleted = 0 ORDER BY position, id`,
      page,
    );
  }

  recordsForPage(page: string): Record[] {
    return this.query(
      `SELECT ${SELECT_COLUMNS} FROM records WHERE page = ? ORDER BY rtype, position, id`,
      page,
    );
  }

  allRecords(): Record[] {
    return this.query(
      `SELECT ${SELECT_COLUMNS} FROM records ORDER BY page, position, id`,
    );
  }

  pageExists(id: string): boolean {
    const row = this.db
      .prepare("SELECT 1 AS ok FROM records WHERE id = ? AND rtype = 'page' AND deleted = 0")
      .get(id);
    return row !== undefined;
  }

  /** Merge records, returning the ones that actually changed. */
  merge(records: Record[]): Record[] {
    const applied: Record[] = [];
    const touched = new Set<string>();
    let maxSeen = this.counter;
    for (const record of records) {
      maxSeen = Math.max(maxSeen, record.version.c);
      if (this.upsert(record)) {
        touched.add(record.page);
        applied.push(record);
      }
    }
    this.counter = Math.max(this.counter, maxSeen);
    for (const page of touched) {
      this.pendingDigests.add(page);
    }
    return applied;
  }

  /** Stamp a new record with a fresh version and merge it. */
  put(record: NewRecord): Record {
    const full: Record = { ...record, version: this.nextClock() };
    this.merge([full]);
    return full;
  }

  /** Tombstone a record by id, if it exists. */
  remove(id: string): Record | undefined {
    const current = this.get(id);
    if (current === undefined) {
      return undefined;
    }
    return this.put({
      id: current.id,
      page: current.page,
      position: current.position,
      deleted: true,
      t: current.t,
      title: current.t === 'page' ? '' : undefined,
      kind: current.t === 'block' ? current.kind : undefined,
      runs: current.t === 'block' ? [] : undefined,
    });
  }

  digests(): { [id: string]: string } {
    this.flushDigests();
    const rows = this.db.prepare('SELECT id, digest FROM pages').all() as Array<{
      id: string;
      digest: string;
    }>;
    const out: { [id: string]: string } = {};
    for (const row of rows) {
      out[row.id] = row.digest;
    }
    return out;
  }

  /** The next page position, after every existing page. */
  nextPagePosition(): string {
    const positions = this.pageRecords()
      .map((record) => record.position)
      .filter((value) => value.length > 0)
      .sort();
    const last = positions[positions.length - 1];
    return last === undefined ? defaultKey() : newAfter(last);
  }

  private upsert(record: Record): boolean {
    const result = this.db
      .prepare(UPSERT)
      .all(
        record.id,
        record.page,
        record.t,
        record.position,
        record.version.c,
        record.version.client,
        record.deleted ? 1 : 0,
        payloadData(record),
      );
    return result.length > 0;
  }

  private query(sql: string, ...params: any[]): Record[] {
    const rows = this.db.prepare(sql).all(...params) as unknown as Row[];
    return rows.map(rowToRecord);
  }

  /** Recompute and store digests for every page whose records changed. */
  private flushDigests(): void {
    if (this.pendingDigests.size === 0) {
      return;
    }
    const pages = [...this.pendingDigests];
    this.pendingDigests.clear();
    const statement = this.db.prepare(
      `INSERT INTO pages (id, digest) VALUES (?, ?)
       ON CONFLICT(id) DO UPDATE SET digest = excluded.digest`,
    );
    for (const page of pages) {
      statement.run(page, this.computeDigest(page));
    }
  }

  /** Drop and recompute every stored digest from the records. Runs on open. */
  private rebuildDigests(): void {
    this.db.exec('DELETE FROM pages');
    const rows = this.db.prepare('SELECT DISTINCT page FROM records ORDER BY page').all() as Array<{
      page: string;
    }>;
    const statement = this.db.prepare(
      `INSERT INTO pages (id, digest) VALUES (?, ?)
       ON CONFLICT(id) DO UPDATE SET digest = excluded.digest`,
    );
    for (const row of rows) {
      statement.run(row.page, this.computeDigest(row.page));
    }
  }

  private computeDigest(page: string): string {
    const rows = this.db
      .prepare(
        'SELECT id, version_c, version_client, deleted FROM records WHERE page = ? ORDER BY id',
      )
      .all(page) as unknown as DigestRow[];
    return digestOf(rows);
  }
}

interface DigestRow {
  id: string;
  version_c: number;
  version_client: number;
  deleted: number;
}

/**
 * SHA-256 over a page's `(id, version, deleted)` tuples, byte-for-byte the same
 * encoding as `src/workspace.rs`. The rows must already be ordered by id.
 */
export function digestOf(rows: DigestRow[]): string {
  const hash = createHash('sha256');
  for (const row of rows) {
    const id = Buffer.from(row.id, 'utf8');
    const length = Buffer.alloc(4);
    length.writeUInt32BE(id.length);
    hash.update(length);
    hash.update(id);
    hash.update(u64(row.version_c));
    hash.update(u64(row.version_client));
    hash.update(Buffer.from([row.deleted ? 1 : 0]));
  }
  return hash.digest('hex');
}

interface Row {
  id: string;
  page: string;
  rtype: string;
  position: string;
  version_c: number;
  version_client: number;
  deleted: number;
  data: string;
}

function rowToRecord(row: Row): Record {
  const payload = JSON.parse(row.data) as {
    t?: string;
    title?: string;
    kind?: string;
    runs?: Run[];
  };
  const record: Record = {
    id: row.id,
    page: row.page,
    position: row.position,
    version: { c: Number(row.version_c), client: Number(row.version_client) },
    deleted: row.deleted !== 0,
    t: payload.t === 'page' ? 'page' : 'block',
  };
  if (record.t === 'page') {
    record.title = payload.title ?? '';
  } else {
    record.kind = payload.kind ?? 'paragraph';
    record.runs = payload.runs ?? [];
  }
  return record;
}

function payloadData(record: Record): string {
  if (record.t === 'page') {
    return JSON.stringify({ t: 'page', title: record.title ?? '' });
  }
  return JSON.stringify({
    t: 'block',
    kind: record.kind ?? 'paragraph',
    runs: record.runs ?? [],
  });
}

/** Big-endian 8-byte encoding of a non-negative integer. */
function u64(value: number): Buffer {
  const buffer = Buffer.alloc(8);
  buffer.writeBigUInt64BE(BigInt(value));
  return buffer;
}

function loadClientId(db: DatabaseSync): number {
  const row = db
    .prepare("SELECT value FROM meta WHERE key = 'client_id'")
    .get() as { value: string } | undefined;
  if (row !== undefined) {
    const parsed = Number(row.value);
    if (Number.isFinite(parsed)) {
      return parsed;
    }
  }
  const id = Date.now() * 1024 + Math.floor(Math.random() * 1024);
  db.prepare(
    `INSERT INTO meta (key, value) VALUES ('client_id', ?), ('schema_version', '1')
     ON CONFLICT(key) DO UPDATE SET value = excluded.value`,
  ).run(String(id));
  return id;
}

export const FORMAT = 1;
