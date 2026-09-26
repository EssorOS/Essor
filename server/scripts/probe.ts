import { WebSocket } from 'ws';

import { digestOf, versionGreater } from '../src/workspace';

const TIMEOUT_MS = Number(process.env.SMOKE_TIMEOUT_MS ?? 5000);

export interface Clock {
  c: number;
  client: number;
}

export interface WireRecord {
  id: string;
  page: string;
  position: string;
  version: Clock;
  deleted: boolean;
  t: 'page' | 'block';
  title?: string;
  kind?: string;
  runs?: Array<{ text: string; bold: boolean; italic: boolean }>;
}

/** A bare-bones JSON sync client used only to exercise the server. */
export class Probe {
  private readonly records = new Map<string, WireRecord>();
  private readonly ws: WebSocket;
  private resolveConnected!: () => void;
  readonly connected: Promise<void>;

  constructor(url: string) {
    this.ws = new WebSocket(`${url.replace(/\/$/, '')}/sync`);
    this.connected = new Promise((resolve) => {
      this.resolveConnected = resolve;
    });
    this.ws.on('open', () => {
      this.send({ type: 'hello', pages: this.digests() });
      this.resolveConnected();
    });
    this.ws.on('message', (data) => this.onMessage(String(data)));
  }

  private onMessage(raw: string): void {
    const msg = JSON.parse(raw) as {
      type: string;
      pages?: { [id: string]: string };
      records?: WireRecord[];
    };
    if (msg.type === 'hello') {
      // Our digests are the source of truth for what to push; for a probe with
      // no local content this sends nothing.
      for (const page of this.localPages()) {
        if (msg.pages?.[page] !== this.digestOf(page)) {
          this.send({ type: 'page', id: page, records: this.recordsForPage(page) });
        }
      }
    } else if ((msg.type === 'page' || msg.type === 'update') && Array.isArray(msg.records)) {
      this.merge(msg.records);
    }
  }

  merge(records: WireRecord[]): void {
    for (const record of records) {
      const current = this.records.get(record.id);
      if (current === undefined || versionGreater(record.version, current.version)) {
        this.records.set(record.id, record);
      }
    }
  }

  publish(records: WireRecord[]): void {
    this.merge(records);
    this.send({ type: 'update', page: records[0]?.page, records });
  }

  liveBlocks(page: string): WireRecord[] {
    return [...this.records.values()]
      .filter((record) => record.t === 'block' && record.page === page && !record.deleted)
      .sort((a, b) =>
        a.position !== b.position
          ? a.position < b.position
            ? -1
            : 1
          : a.id < b.id
            ? -1
            : 1,
      );
  }

  textOf(page: string): string {
    return this.liveBlocks(page)
      .map((block) => (block.runs ?? []).map((run) => run.text).join(''))
      .join('\n');
  }

  private localPages(): string[] {
    return [...new Set([...this.records.values()].filter((r) => !r.deleted).map((r) => r.page))];
  }

  private recordsForPage(page: string): WireRecord[] {
    return [...this.records.values()].filter((r) => r.page === page);
  }

  private digests(): { [id: string]: string } {
    const out: { [id: string]: string } = {};
    for (const page of this.localPages()) {
      out[page] = this.digestOf(page);
    }
    return out;
  }

  private digestOf(page: string): string {
    const rows = this.recordsForPage(page)
      .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
      .map((record) => ({
        id: record.id,
        version_c: record.version.c,
        version_client: record.version.client,
        deleted: record.deleted ? 1 : 0,
      }));
    return digestOf(rows);
  }

  private send(value: unknown): void {
    this.ws.send(JSON.stringify(value));
  }

  close(): void {
    this.ws.close();
  }
}

export function waitFor(check: () => boolean, label: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const started = Date.now();
    const timer = setInterval(() => {
      if (check()) {
        clearInterval(timer);
        resolve();
      } else if (Date.now() - started > TIMEOUT_MS) {
        clearInterval(timer);
        reject(new Error(`timed out waiting for ${label}`));
      }
    }, 25);
  });
}
