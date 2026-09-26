import * as http from 'http';
import * as path from 'path';
import * as awarenessProtocol from 'y-protocols/awareness';
import * as syncProtocol from 'y-protocols/sync';
import * as decoding from 'lib0/decoding';
import * as encoding from 'lib0/encoding';
import { WebSocket, WebSocketServer } from 'ws';

import { toBytes } from './bytes';
import {
  MESSAGE_AWARENESS,
  MESSAGE_SYNC,
  PING_TIMEOUT_MS,
  SHUTDOWN_WAIT_MS,
} from './protocol';
import {
  closeConn,
  createPersistence,
  docs,
  getYDoc,
  send,
  type Persistence,
  type WSSharedDoc,
} from './shared';

const HOST = process.env.HOST ?? '127.0.0.1';
const PORT = Number(process.env.PORT ?? 1234);
const DATA_DIR = process.env.DATA_DIR ?? path.join(__dirname, '..', 'data');

/** Longest room name accepted; keeps LevelDB keys bounded. */
const MAX_DOC_NAME = 256;

/**
 * The room name from a request path, or `null` when it is missing or malformed.
 * Room names become LevelDB keys, so reject path-like and control-character
 * values rather than persisting them.
 */
function documentName(url: string | undefined): string | null {
  const name = (url ?? '').slice(1).split('?')[0] || 'default';
  if (name.length > MAX_DOC_NAME) {
    return null;
  }
  if (name.includes('..') || /[\u0000-\u001f\u007f]/.test(name)) {
    return null;
  }
  return name;
}

/** Apply one framed protocol message from a client. */
function onMessage(conn: WebSocket, doc: WSSharedDoc, message: Uint8Array): void {
  try {
    const encoder = encoding.createEncoder();
    const decoder = decoding.createDecoder(message);
    const messageType = decoding.readVarUint(decoder);
    switch (messageType) {
      case MESSAGE_SYNC: {
        encoding.writeVarUint(encoder, MESSAGE_SYNC);
        syncProtocol.readSyncMessage(decoder, encoder, doc, conn);
        // A reply containing only the message type has nothing to say.
        if (encoding.length(encoder) > 1) {
          send(doc, conn, encoding.toUint8Array(encoder));
        }
        break;
      }
      case MESSAGE_AWARENESS:
        awarenessProtocol.applyAwarenessUpdate(
          doc.awareness,
          decoding.readVarUint8Array(decoder),
          conn,
        );
        break;
      default:
        break;
    }
  } catch (error) {
    console.error('[essor-sync] failed to handle message', error);
  }
}

/**
 * Attach a freshly upgraded websocket to the document named by the request
 * path, kick off the sync handshake and keep the connection alive.
 */
function setupWSConnection(
  conn: WebSocket,
  req: http.IncomingMessage,
  persistence: Persistence | null,
): void {
  const docName = documentName(req.url);
  if (docName === null) {
    conn.close(1008, 'invalid document name');
    return;
  }
  const doc = getYDoc(docName, persistence);
  doc.conns.set(conn, new Set());

  // Persistence loads asynchronously. Replying to a SyncStep1 before it finishes
  // would answer with empty state, and since the client only sends SyncStep1 on
  // connect it would never receive the persisted document until it reconnects.
  // Buffer inbound frames until initialization completes.
  let ready = false;
  const pending: Uint8Array[] = [];
  conn.on('message', (data) => {
    const message = toBytes(data);
    if (ready) {
      onMessage(conn, doc, message);
    } else {
      pending.push(message);
    }
  });

  let pongReceived = true;
  const pingInterval = setInterval(() => {
    if (!pongReceived) {
      if (doc.conns.has(conn)) {
        closeConn(doc, conn);
      }
      clearInterval(pingInterval);
    } else if (doc.conns.has(conn)) {
      pongReceived = false;
      try {
        conn.ping();
      } catch {
        closeConn(doc, conn);
        clearInterval(pingInterval);
      }
    }
  }, PING_TIMEOUT_MS);

  conn.on('close', () => {
    closeConn(doc, conn);
    clearInterval(pingInterval);
  });
  conn.on('pong', () => {
    pongReceived = true;
  });

  // Wait for persisted state so the first SyncStep1 reflects it.
  void doc.whenInitialized.then(() => {
    if (!doc.conns.has(conn)) {
      return;
    }
    const encoder = encoding.createEncoder();
    encoding.writeVarUint(encoder, MESSAGE_SYNC);
    syncProtocol.writeSyncStep1(encoder, doc);
    send(doc, conn, encoding.toUint8Array(encoder));

    const states = doc.awareness.getStates();
    if (states.size > 0) {
      const awareness = encoding.createEncoder();
      encoding.writeVarUint(awareness, MESSAGE_AWARENESS);
      encoding.writeVarUint8Array(
        awareness,
        awarenessProtocol.encodeAwarenessUpdate(doc.awareness, Array.from(states.keys())),
      );
      send(doc, conn, encoding.toUint8Array(awareness));
    }

    ready = true;
    for (const message of pending) {
      onMessage(conn, doc, message);
    }
    pending.length = 0;
  });
}

function main(): void {
  const persistence = createPersistence(DATA_DIR);
  const server = http.createServer((req, res) => {
    if (req.url === '/' || req.url === '/health') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ ok: true, documents: docs.size }));
      return;
    }
    res.writeHead(404);
    res.end();
  });

  const wss = new WebSocketServer({ noServer: true });
  server.on('upgrade', (req, socket, head) => {
    wss.handleUpgrade(req, socket, head, (conn) => setupWSConnection(conn, req, persistence));
  });

  const shutdown = (): void => {
    console.info('[essor-sync] shutting down');
    wss.clients.forEach((conn) => conn.close());
    wss.close(() => server.close(() => process.exit(0)));
    setTimeout(() => process.exit(0), SHUTDOWN_WAIT_MS).unref();
  };
  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);

  server.listen(PORT, HOST, () => {
    console.info(`[essor-sync] listening on ws://${HOST}:${PORT}`);
    console.info(`[essor-sync] persisting documents to ${DATA_DIR}`);
  });
}

main();
