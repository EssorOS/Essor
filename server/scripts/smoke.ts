import { WebSocket } from 'ws';
import * as Y from 'yjs';
import * as syncProtocol from 'y-protocols/sync';
import * as decoding from 'lib0/decoding';
import * as encoding from 'lib0/encoding';

import { toBytes } from '../src/bytes';
import { MESSAGE_SYNC } from '../src/protocol';

const BASE_URL = process.env.ESSOR_SYNC_URL ?? 'ws://127.0.0.1:1234';
const TIMEOUT_MS = Number(process.env.SMOKE_TIMEOUT_MS ?? 5000);

/** A bare-bones y-websocket client used only to exercise the server. */
class Probe {
  readonly doc = new Y.Doc();
  private readonly ws: WebSocket;
  private resolveConnected!: () => void;
  readonly connected: Promise<void>;

  constructor(room: string) {
    this.ws = new WebSocket(`${BASE_URL}/${room}`);
    this.connected = new Promise((resolve) => {
      this.resolveConnected = resolve;
    });
    this.ws.binaryType = 'arraybuffer';
    this.ws.on('open', () => {
      const encoder = encoding.createEncoder();
      encoding.writeVarUint(encoder, MESSAGE_SYNC);
      syncProtocol.writeSyncStep1(encoder, this.doc);
      this.ws.send(encoding.toUint8Array(encoder));
      this.resolveConnected();
    });
    this.ws.on('message', (data) => this.onMessage(toBytes(data)));
    this.doc.on('update', (update, origin) => {
      if (origin === 'remote') {
        return;
      }
      const encoder = encoding.createEncoder();
      encoding.writeVarUint(encoder, MESSAGE_SYNC);
      syncProtocol.writeUpdate(encoder, update);
      this.ws.send(encoding.toUint8Array(encoder));
    });
  }

  private onMessage(message: Uint8Array): void {
    const decoder = decoding.createDecoder(message);
    const encoder = encoding.createEncoder();
    const messageType = decoding.readVarUint(decoder);
    if (messageType === MESSAGE_SYNC) {
      encoding.writeVarUint(encoder, MESSAGE_SYNC);
      syncProtocol.readSyncMessage(decoder, encoder, this.doc, 'remote');
      if (encoding.length(encoder) > 1) {
        this.ws.send(encoding.toUint8Array(encoder));
      }
    }
  }

  close(): void {
    this.ws.close();
  }
}

function waitFor(check: () => boolean, label: string): Promise<void> {
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

async function main(): Promise<void> {
  const room = `smoke-${Date.now()}`;
  const a = new Probe(room);
  const b = new Probe(room);
  await Promise.all([a.connected, b.connected]);

  a.doc.getText('probe').insert(0, 'hello from A');
  await waitFor(() => b.doc.getText('probe').toString() === 'hello from A', 'A -> B sync');
  console.log('ok: updates propagate between two clients');

  a.close();
  b.close();
  await new Promise((resolve) => setTimeout(resolve, 250));

  const c = new Probe(room);
  await c.connected;
  await waitFor(() => c.doc.getText('probe').toString() === 'hello from A', 'persisted state');
  console.log('ok: state survives a disconnect and is served from persistence');
  c.close();
  process.exit(0);
}

main().catch((error) => {
  console.error('smoke test failed:', error);
  process.exit(1);
});
