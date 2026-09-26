import * as fs from 'fs';
import * as http from 'http';
import * as path from 'path';
import { WebSocketServer } from 'ws';

import { SyncHub } from './hub';
import { handleMcpRequest } from './mcp/http';
import { Workspace } from './workspace';

/** How long to wait for the quit signal during graceful shutdown. */
const SHUTDOWN_WAIT_MS = 2_000;

/** Options for an embeddable sync server; tests and the CLI both use these. */
export interface ServerOptions {
  host?: string;
  port?: number;
  dataDir?: string;
  /** Serve the MCP endpoint. Defaults to `ESSOR_MCP !== '0'`. */
  mcp?: boolean;
}

/** A running sync server and the handles needed to drive and stop it. */
export interface RunningServer {
  server: http.Server;
  wss: WebSocketServer;
  workspace: Workspace;
  url: string;
  dataDir: string;
  close: () => Promise<void>;
}

/** Start the sync server, resolving once it is accepting connections. */
export async function startServer(options: ServerOptions = {}): Promise<RunningServer> {
  const host = options.host ?? process.env.HOST ?? '127.0.0.1';
  const port = options.port ?? Number(process.env.PORT ?? 1234);
  const dataDir = options.dataDir ?? process.env.DATA_DIR ?? path.join(__dirname, '..', 'data');
  const mcpEnabled = options.mcp ?? process.env.ESSOR_MCP !== '0';

  fs.mkdirSync(dataDir, { recursive: true });
  const workspace = new Workspace(path.join(dataDir, 'essor.db'));
  const hub = new SyncHub(workspace);

  const server = http.createServer((req, res) => {
    const route = (req.url ?? '').split('?')[0];
    if (route === '/' || route === '/health') {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(
        JSON.stringify({
          ok: true,
          documents: workspace.pageRecords().length,
          mcp: mcpEnabled,
        }),
      );
      return;
    }
    if (mcpEnabled && route === '/mcp') {
      void handleMcpRequest(req, res, workspace, (records) => hub.publish(records)).catch(
        (error) => {
          console.error('[essor-sync] failed to handle MCP request', error);
          if (!res.headersSent) {
            res.writeHead(500, { 'Content-Type': 'application/json' });
          }
          res.end();
        },
      );
      return;
    }
    res.writeHead(404);
    res.end();
  });

  const wss = new WebSocketServer({ noServer: true });
  server.on('upgrade', (req, socket, head) => {
    const route = (req.url ?? '').split('?')[0];
    if (route !== '/sync') {
      socket.destroy();
      return;
    }
    wss.handleUpgrade(req, socket, head, (conn) => hub.accept(conn));
  });

  await new Promise<void>((resolve, reject) => {
    const onError = (error: Error): void => reject(error);
    server.once('error', onError);
    server.listen(port, host, () => {
      server.off('error', onError);
      resolve();
    });
  });

  const address = server.address();
  const boundPort = typeof address === 'object' && address !== null ? address.port : port;
  const displayHost = host === '0.0.0.0' ? '127.0.0.1' : host;
  const close = (): Promise<void> =>
    new Promise((resolve) => {
      hub.shutdown();
      wss.close(() => server.close(() => {
        workspace.close();
        resolve();
      }));
    });

  return { server, wss, workspace, url: `ws://${displayHost}:${boundPort}`, dataDir, close };
}

function main(): void {
  void startServer()
    .then((running) => {
      console.info(`[essor-sync] listening on ${running.url}`);
      console.info(`[essor-sync] persisting documents to ${running.dataDir}`);
      const shutdown = (): void => {
        console.info('[essor-sync] shutting down');
        void running.close().then(() => process.exit(0));
        setTimeout(() => process.exit(0), SHUTDOWN_WAIT_MS).unref();
      };
      process.on('SIGINT', shutdown);
      process.on('SIGTERM', shutdown);
    })
    .catch((error) => {
      console.error('[essor-sync] failed to start', error);
      process.exit(1);
    });
}

if (require.main === module) {
  main();
}
