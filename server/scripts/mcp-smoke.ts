import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';

import { between, compareKeys, defaultKey, newAfter, newBefore, parseKey } from '../src/mcp/fractional';
import { startServer } from '../src/server';
import { Workspace, type Record } from '../src/workspace';
import { Probe, waitFor } from './probe';

/** Assert that a tool call fails, whether as an `isError` result or a throw. */
async function expectToolError(
  call: Promise<Awaited<ReturnType<Client['callTool']>>>,
): Promise<void> {
  let succeeded = false;
  try {
    const result = await call;
    succeeded = result.isError !== true;
  } catch {
    return;
  }
  if (succeeded) {
    throw new Error('expected the tool call to fail, but it succeeded');
  }
}

function firstJson<T>(result: Awaited<ReturnType<Client['callTool']>>): T {
  const content = result.content as Array<{ type: string; text?: string }>;
  const text = content.find((item) => item.type === 'text')?.text;
  if (text === undefined) {
    throw new Error('tool result carried no text content');
  }
  if (result.isError === true) {
    throw new Error(`tool failed: ${text}`);
  }
  return JSON.parse(text) as T;
}

function assertOrderKeys(): void {
  const checks: Array<[string, string]> = [
    [defaultKey(), '80'],
    [newAfter('80'), '8180'],
    [newAfter('8180'), '8280'],
    [newAfter('817f80'), '8280'],
    [newBefore('80'), '7f80'],
    [between('80', '8180') ?? '', '817f80'],
    [between(null, '8180') ?? '', '80'],
    [between('80', null) ?? '', '8180'],
  ];
  for (const [actual, expected] of checks) {
    if (actual !== expected) {
      throw new Error(`order key mismatch: expected ${expected}, got ${actual}`);
    }
  }
  if (parseKey('8180') === null || compareKeys('80', '8180') >= 0) {
    throw new Error('order key handling is wrong');
  }
  const mid = between('80', '8180');
  if (mid === null || compareKeys('80', mid) >= 0 || compareKeys(mid, '8180') >= 0) {
    throw new Error('between did not place a key strictly between its bounds');
  }
  console.log("ok: order keys match the editor's hex fractional-index format");
}

/**
 * The digest encoding must match Rust's `src/workspace.rs` byte for byte. These
 * records and hash mirror the `digest_matches_the_canonical_encoding` test there.
 */
function assertDigestVector(): void {
  const workspace = new Workspace(':memory:');
  const records: Record[] = [
    {
      id: '1111',
      page: 'p',
      position: '80',
      version: { c: 1, client: 2 },
      deleted: false,
      t: 'block',
      kind: 'paragraph',
      runs: [{ text: 'x', bold: false, italic: false }],
    },
    {
      id: '2222',
      page: 'p',
      position: '80',
      version: { c: 5, client: 2 },
      deleted: true,
      t: 'block',
      kind: 'paragraph',
      runs: [],
    },
  ];
  workspace.merge(records);
  const digest = workspace.digests()['p'];
  workspace.close();
  const expected = '93630e7bcd59bbdae071559336c082766caf3feb9dfad1eb384d637ca7f8ba4c';
  if (digest !== expected) {
    throw new Error(`digest mismatch: expected ${expected}, got ${digest}`);
  }
  console.log('ok: digest matches the Rust canonical encoding');
}

async function main(): Promise<void> {
  assertOrderKeys();
  assertDigestVector();

  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'essor-mcp-'));
  const running = await startServer({ host: '127.0.0.1', port: 0, dataDir, mcp: true });
  const baseUrl = running.url.replace('ws://', 'http://');
  console.log(`ok: sync server listening on ${running.url}`);

  const client = new Client({ name: 'essor-mcp-smoke', version: '0.1.0' });
  const transport = new StreamableHTTPClientTransport(new URL(`${baseUrl}/mcp`));
  await client.connect(transport);
  console.log('ok: MCP client connected over Streamable HTTP');

  const tools = await client.listTools();
  const names = tools.tools.map((tool) => tool.name).sort();
  if (!names.includes('create_document') || !names.includes('get_document')) {
    throw new Error(`unexpected tool list: ${names.join(', ')}`);
  }
  console.log(`ok: ${names.length} tools advertised`);

  const created = firstJson<{ id: string }>(
    await client.callTool({
      name: 'create_document',
      arguments: {
        title: 'MCP smoke',
        blocks: [
          { kind: 'heading1', text: 'MCP smoke' },
          { kind: 'paragraph', runs: [{ text: 'hello ', bold: true }, { text: 'world' }] },
        ],
      },
    }),
  );
  console.log(`ok: created document ${created.id}`);

  const read = firstJson<{ blocks: Array<{ kind: string; text: string; runs: Array<{ bold?: boolean }> }> }>(
    await client.callTool({ name: 'get_document', arguments: { id: created.id } }),
  );
  if (read.blocks.length !== 2 || read.blocks[1]?.text !== 'hello world') {
    throw new Error(`unexpected document body: ${JSON.stringify(read)}`);
  }
  if (read.blocks[1]?.runs[0]?.bold !== true) {
    throw new Error(`bold mark was not preserved: ${JSON.stringify(read.blocks[1])}`);
  }
  console.log('ok: block structure and inline marks round-trip');

  const appended = firstJson<{ total: number }>(
    await client.callTool({
      name: 'append_blocks',
      arguments: { id: created.id, blocks: [{ kind: 'bullet', text: 'item' }] },
    }),
  );
  if (appended.total !== 3) {
    throw new Error(`append did not reach three blocks: ${JSON.stringify(appended)}`);
  }
  console.log('ok: append_blocks');

  await client.callTool({
    name: 'update_block',
    arguments: { id: created.id, index: 0, kind: 'heading2', text: 'Renamed' },
  });
  const updated = firstJson<{ blocks: Array<{ kind: string; text: string }> }>(
    await client.callTool({ name: 'get_document', arguments: { id: created.id } }),
  );
  if (updated.blocks[0]?.kind !== 'heading2' || updated.blocks[0]?.text !== 'Renamed') {
    throw new Error(`update_block failed: ${JSON.stringify(updated.blocks[0])}`);
  }
  console.log('ok: update_block');

  const noop = firstJson<{ updated: boolean }>(
    await client.callTool({
      name: 'update_block',
      arguments: { id: created.id, index: 0, kind: 'heading2', text: 'Renamed' },
    }),
  );
  if (noop.updated !== false) {
    throw new Error(`a no-op update_block should report updated:false: ${JSON.stringify(noop)}`);
  }
  console.log('ok: update_block is a no-op when nothing changes');

  await expectToolError(
    client.callTool({ name: 'replace_document', arguments: { id: created.id, blocks: [] } }),
  );
  const afterEmpty = firstJson<{ blocks: unknown[] }>(
    await client.callTool({ name: 'get_document', arguments: { id: created.id } }),
  );
  if (afterEmpty.blocks.length === 0) {
    throw new Error('replace_document with an empty body wiped the document');
  }
  console.log('ok: replace_document refuses an empty body');

  await client.callTool({
    name: 'insert_block',
    arguments: { id: created.id, index: 0, block: { kind: 'paragraph', text: 'top' } },
  });
  const inserted = firstJson<{ blocks: Array<{ text: string }> }>(
    await client.callTool({ name: 'get_document', arguments: { id: created.id } }),
  );
  if (inserted.blocks[0]?.text !== 'top') {
    throw new Error(`insert_block did not land at index 0: ${JSON.stringify(inserted.blocks)}`);
  }
  console.log('ok: insert_block lands at the requested index');

  const found = firstJson<{ results: Array<{ id: string }> }>(
    await client.callTool({ name: 'search_documents', arguments: { query: 'hello' } }),
  );
  if (!found.results.some((entry) => entry.id === created.id)) {
    throw new Error(`search missed the document: ${JSON.stringify(found)}`);
  }
  console.log('ok: search_documents');

  // An editor connecting over the sync websocket must see the agent's content.
  const probe = new Probe(running.url);
  await probe.connected;
  await waitFor(
    () => probe.textOf(created.id).includes('Renamed') && probe.textOf(created.id).includes('hello world'),
    'MCP content to reach a websocket client',
  );
  probe.close();
  console.log('ok: MCP edits are visible to an editor over the sync protocol');

  const second = firstJson<{ id: string }>(
    await client.callTool({ name: 'create_document', arguments: { title: 'Keeper' } }),
  );
  const deleted = firstJson<{ deleted: boolean; remaining: number }>(
    await client.callTool({ name: 'delete_document', arguments: { id: created.id } }),
  );
  if (!deleted.deleted || deleted.remaining !== 1) {
    throw new Error(`delete_document reported no deletion: ${JSON.stringify(deleted)}`);
  }
  const after = firstJson<{ documents: Array<{ id: string }> }>(
    await client.callTool({ name: 'list_documents', arguments: {} }),
  );
  if (after.documents.length !== 1 || after.documents[0]?.id !== second.id) {
    throw new Error(`the deleted page is still listed: ${JSON.stringify(after)}`);
  }
  console.log('ok: delete_document removes the page from the library');

  await client.close();
  await running.close();
  fs.rmSync(dataDir, { recursive: true, force: true });
  console.log('\nmcp smoke test passed');
}

main().catch((error) => {
  console.error('mcp smoke test failed:', error);
  process.exit(1);
});
