import { Probe, waitFor, type WireRecord } from './probe';

const BASE_URL = process.env.ESSOR_SYNC_URL ?? 'ws://127.0.0.1:1234';

function block(id: string, page: string, position: string, text: string): WireRecord {
  return {
    id,
    page,
    position,
    version: { c: 1, client: 1 },
    deleted: false,
    t: 'block',
    kind: 'paragraph',
    runs: [{ text, bold: false, italic: false }],
  };
}

async function main(): Promise<void> {
  const page = `smoke${Date.now()}`;
  const a = new Probe(BASE_URL);
  const b = new Probe(BASE_URL);
  await Promise.all([a.connected, b.connected]);

  a.publish([
    { id: page, page, position: '80', version: { c: 0, client: 1 }, deleted: false, t: 'page', title: 'Smoke' },
    block('smoke-block-1', page, '80', 'hello from A'),
  ]);
  await waitFor(() => b.textOf(page) === 'hello from A', 'A -> B sync');
  console.log('ok: updates propagate between two clients');

  a.close();
  b.close();
  await new Promise((resolve) => setTimeout(resolve, 250));

  const c = new Probe(BASE_URL);
  await c.connected;
  await waitFor(() => c.textOf(page) === 'hello from A', 'persisted state');
  console.log('ok: state survives a disconnect and is served from persistence');
  c.close();
  process.exit(0);
}

main().catch((error) => {
  console.error('smoke test failed:', error);
  process.exit(1);
});
