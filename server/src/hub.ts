import { WebSocket } from 'ws';

import { versionGreater, type Record, type Workspace } from './workspace';

/** How long a connection may go without a pong before it is dropped. */
const PING_TIMEOUT_MS = 30_000;

const wsOpen = 1;
const wsConnecting = 0;

/**
 * The websocket side of the sync server: it tracks connections, runs the
 * digest handshake, merges record frames, and relays the records that won
 * last-write-wins to the other clients.
 */
export class SyncHub {
  private readonly connections = new Set<WebSocket>();

  constructor(private readonly workspace: Workspace) {}

  /** Register a freshly upgraded connection and announce our page digests. */
  accept(conn: WebSocket): void {
    this.connections.add(conn);

    let pongReceived = true;
    const pingInterval = setInterval(() => {
      if (!pongReceived) {
        this.forget(conn, pingInterval);
      } else if (this.connections.has(conn)) {
        pongReceived = false;
        try {
          conn.ping();
        } catch {
          this.forget(conn, pingInterval);
        }
      }
    }, PING_TIMEOUT_MS);

    conn.on('message', (data) => this.onMessage(conn, data));
    conn.on('close', () => this.forget(conn, pingInterval));
    conn.on('pong', () => {
      pongReceived = true;
    });

    sendJson(conn, { type: 'hello', pages: this.workspace.digests() });
  }

  /** Broadcast changed records to every connection (used by the MCP endpoint). */
  publish(records: Record[]): void {
    this.broadcast(records);
  }

  /** Close every connection, for graceful shutdown. */
  shutdown(): void {
    for (const conn of this.connections) {
      conn.close();
    }
  }

  /** Apply one inbound JSON frame and relay what changed. */
  private onMessage(conn: WebSocket, data: unknown): void {
    let msg: { type?: string; pages?: { [id: string]: string }; records?: Record[] };
    try {
      msg = JSON.parse(String(data));
    } catch {
      return;
    }
    switch (msg.type) {
      case 'hello': {
        const server = this.workspace.digests();
        for (const [id, digest] of Object.entries(server)) {
          if (msg.pages?.[id] !== digest) {
            this.sendPage(conn, id);
          }
        }
        break;
      }
      case 'page':
      case 'update': {
        if (Array.isArray(msg.records)) {
          const applied = this.workspace.merge(msg.records);
          this.broadcast(applied, conn);
          // A record the server rejected lost last-write-wins. Send the winner
          // back so the origin converges immediately instead of on reconnect.
          const appliedIds = new Set(applied.map((record) => record.id));
          for (const record of msg.records) {
            if (appliedIds.has(record.id)) {
              continue;
            }
            const current = this.workspace.get(record.id);
            if (current !== undefined && versionGreater(current.version, record.version)) {
              sendJson(conn, { type: 'update', page: record.page, records: [current] });
            }
          }
        }
        break;
      }
      default:
        break;
    }
  }

  /** Send one page's full records to a single connection. */
  private sendPage(conn: WebSocket, id: string): void {
    sendJson(conn, { type: 'page', id, records: this.workspace.recordsForPage(id) });
  }

  /** Broadcast changed records to every connection except `origin`, by page. */
  private broadcast(records: Record[], origin?: WebSocket): void {
    if (records.length === 0) {
      return;
    }
    const byPage = new Map<string, Record[]>();
    for (const record of records) {
      const bucket = byPage.get(record.page);
      if (bucket === undefined) {
        byPage.set(record.page, [record]);
      } else {
        bucket.push(record);
      }
    }
    for (const [page, pageRecords] of byPage) {
      const frame = JSON.stringify({ type: 'update', page, records: pageRecords });
      for (const conn of this.connections) {
        if (conn !== origin) {
          sendRaw(conn, frame);
        }
      }
    }
  }

  /** Drop a connection and stop its heartbeat. */
  private forget(conn: WebSocket, pingInterval: NodeJS.Timeout): void {
    this.connections.delete(conn);
    clearInterval(pingInterval);
    conn.close();
  }
}

function sendJson(conn: WebSocket, value: unknown): void {
  sendRaw(conn, JSON.stringify(value));
}

function sendRaw(conn: WebSocket, frame: string): void {
  if (conn.readyState !== wsConnecting && conn.readyState !== wsOpen) {
    return;
  }
  try {
    conn.send(frame, (err) => {
      if (err != null) {
        conn.close();
      }
    });
  } catch {
    conn.close();
  }
}
