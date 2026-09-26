import * as Y from 'yjs';
import * as awarenessProtocol from 'y-protocols/awareness';
import * as syncProtocol from 'y-protocols/sync';
import * as encoding from 'lib0/encoding';
import { LeveldbPersistence } from 'y-leveldb';
import type { WebSocket } from 'ws';

import { MESSAGE_AWARENESS, MESSAGE_SYNC } from './protocol';

/**
 * Storage backend attached to a shared document. It is responsible for loading
 * persisted state into the document and for writing every subsequent update.
 */
export interface Persistence {
  bindState(docName: string, doc: WSSharedDoc): Promise<void>;
}

/**
 * A single collaborative document and the set of connections editing it.
 *
 * Mirrors `y-websocket`'s `WSSharedDoc`: document updates are rebroadcast to
 * every connection, and awareness changes are fanned out separately.
 */
export class WSSharedDoc extends Y.Doc {
  readonly name: string;
  readonly conns = new Map<WebSocket, Set<number>>();
  readonly awareness: awarenessProtocol.Awareness;
  readonly whenInitialized: Promise<void>;

  constructor(name: string, persistence: Persistence | null) {
    super({ gc: true });
    this.name = name;
    this.awareness = new awarenessProtocol.Awareness(this);
    // The server has no presence of its own.
    this.awareness.setLocalState(null);

    this.on('update', this.onUpdate);
    this.awareness.on('update', this.onAwarenessUpdate);
    this.whenInitialized = persistence
      ? persistence.bindState(name, this)
      : Promise.resolve();
  }

  /** Broadcast a document update to every connection. */
  private onUpdate = (update: Uint8Array): void => {
    const encoder = encoding.createEncoder();
    encoding.writeVarUint(encoder, MESSAGE_SYNC);
    syncProtocol.writeUpdate(encoder, update);
    const message = encoding.toUint8Array(encoder);
    this.conns.forEach((_, conn) => send(this, conn, message));
  };

  /** Broadcast awareness changes to every connection. */
  private onAwarenessUpdate = (
    changes: { added: number[]; updated: number[]; removed: number[] },
    origin: unknown,
  ): void => {
    const changedClients = changes.added.concat(changes.updated, changes.removed);
    if (origin != null && typeof origin === 'object') {
      const controlled = this.conns.get(origin as WebSocket);
      if (controlled !== undefined) {
        changes.added.forEach((id) => controlled.add(id));
        changes.removed.forEach((id) => controlled.delete(id));
      }
    }
    const encoder = encoding.createEncoder();
    encoding.writeVarUint(encoder, MESSAGE_AWARENESS);
    encoding.writeVarUint8Array(
      encoder,
      awarenessProtocol.encodeAwarenessUpdate(this.awareness, changedClients),
    );
    const buff = encoding.toUint8Array(encoder);
    this.conns.forEach((_, conn) => send(this, conn, buff));
  };
}

/** Documents currently held in memory, keyed by room name. */
export const docs = new Map<string, WSSharedDoc>();

/** Look up a document, creating and binding it on first access. */
export function getYDoc(name: string, persistence: Persistence | null): WSSharedDoc {
  let doc = docs.get(name);
  if (doc === undefined) {
    doc = new WSSharedDoc(name, persistence);
    docs.set(name, doc);
  }
  return doc;
}

/** Build a LevelDB-backed persistence layer rooted at `dir`. */
export function createPersistence(dir: string): Persistence {
  const ldb = new LeveldbPersistence(dir);
  return {
    async bindState(docName: string, doc: WSSharedDoc): Promise<void> {
      const persisted = await ldb.getYDoc(docName);
      const newUpdates = Y.encodeStateAsUpdate(doc);
      await ldb.storeUpdate(docName, newUpdates);
      Y.applyUpdate(doc, Y.encodeStateAsUpdate(persisted));
      doc.on('update', (update: Uint8Array) => {
        void ldb.storeUpdate(docName, update);
      });
    },
  };
}

const wsOpen = 1;
const wsConnecting = 0;

/**
 * Send `message` to `conn`, dropping the connection if it can no longer be
 * reached. Safe to call for connections in any state.
 */
export function send(doc: WSSharedDoc, conn: WebSocket, message: Uint8Array): void {
  if (conn.readyState !== wsConnecting && conn.readyState !== wsOpen) {
    closeConn(doc, conn);
    return;
  }
  try {
    conn.send(message, (err) => {
      if (err != null) {
        closeConn(doc, conn);
      }
    });
  } catch {
    closeConn(doc, conn);
  }
}

/**
 * Remove `conn` from its document, clean up the awareness state it owned and,
 * when the last editor leaves, forget the document so it can be collected.
 */
export function closeConn(doc: WSSharedDoc, conn: WebSocket): void {
  const controlled = doc.conns.get(conn);
  if (controlled === undefined) {
    return;
  }
  doc.conns.delete(conn);
  awarenessProtocol.removeAwarenessStates(doc.awareness, Array.from(controlled), null);
  if (doc.conns.size === 0) {
    docs.delete(doc.name);
    doc.destroy();
  }
  conn.close();
}
