import type { IncomingMessage, ServerResponse } from 'http';
import { StreamableHTTPServerTransport } from '@modelcontextprotocol/sdk/server/streamableHttp.js';

import type { Workspace } from '../workspace';
import { createMcpServer, type Broadcast } from './server';

/** Largest MCP request body accepted, to bound memory from a single call. */
const MAX_BODY_BYTES = 1_000_000;

/**
 * Serve one stateless MCP request over the Streamable HTTP transport.
 *
 * A fresh server and transport are created per request (the stateless pattern):
 * there is no session to keep, which keeps the sync server's HTTP plumbing
 * simple and lets any number of agents connect and disconnect freely.
 *
 * Only POST is supported. GET (the optional server-push stream) and DELETE
 * (session teardown) both return 405, which is valid for a stateless server.
 */
export async function handleMcpRequest(
  req: IncomingMessage,
  res: ServerResponse,
  workspace: Workspace,
  broadcast: Broadcast,
): Promise<void> {
  if (req.method !== 'POST') {
    res.writeHead(405, { 'Content-Type': 'application/json', Allow: 'POST' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        error: { code: -32000, message: 'Method not allowed' },
        id: null,
      }),
    );
    return;
  }

  let body: unknown;
  try {
    body = await readJsonBody(req);
  } catch (error) {
    res.writeHead(400, { 'Content-Type': 'application/json' });
    res.end(
      JSON.stringify({
        jsonrpc: '2.0',
        error: { code: -32700, message: `invalid request body: ${messageOf(error)}` },
        id: null,
      }),
    );
    return;
  }

  const server = createMcpServer(workspace, broadcast);
  const transport = new StreamableHTTPServerTransport({
    sessionIdGenerator: undefined,
    enableJsonResponse: true,
  });

  res.on('close', () => {
    void transport.close();
    void server.close();
  });

  await server.connect(transport);
  await transport.handleRequest(req, res, body);
}

/** Read and parse a JSON request body, enforcing the size cap. */
function readJsonBody(req: IncomingMessage): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    let size = 0;
    req.on('data', (chunk: Buffer) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        req.destroy();
        reject(new Error('body exceeds 1 MB'));
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => {
      if (chunks.length === 0) {
        resolve(undefined);
        return;
      }
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
      } catch (error) {
        reject(error);
      }
    });
    req.on('error', reject);
  });
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
