# Essor sync server

A small SQLite-backed websocket server that keeps [Essor](../README.md)
documents in sync across clients. One connection carries the whole workspace:
clients and server exchange per-page digests, send the pages that differ, and
then stream per-page record deltas.

Each page and block is a record merged by last-write-wins on a Lamport clock.
Merging is at block granularity: two clients editing the same block concurrently
resolve last-write-wins and the losing version is replaced wholesale, while edits
to different blocks merge cleanly. Deletes are tombstones that are retained so
they keep propagating across reconnects, and page order uses fractional indices.
The server therefore does not need to order writes — it merges, broadcasts the
changed records, and persists to SQLite.

## Run it

```sh
cd server
npm install
npm run dev      # watch mode, TypeScript directly
# or
npm run build && npm start
```

The server listens on `ws://127.0.0.1:1234` by default and persists to
`server/data/essor.db` with Node's built-in `node:sqlite`. Set `HOST=0.0.0.0` to
accept connections from the network.

| Variable | Default | Description |
| --- | --- | --- |
| `HOST` | `127.0.0.1` | Interface to bind. |
| `PORT` | `1234` | Port to listen on. |
| `DATA_DIR` | `server/data` | Directory for the SQLite database. |
| `ESSOR_MCP` | `1` | Serve the MCP endpoint. Set to `0` to disable it. |

`GET /health` returns `{ "ok": true, "documents": <count>, "mcp": <bool> }`.

## Protocol

One websocket at `ws://<host>:<port>/sync`. Frames are JSON:

| Message | Direction | Purpose |
| --- | --- | --- |
| `{ "type": "hello", "pages": { "<id>": "<digest>" } }` | both | Advertise page digests. |
| `{ "type": "page", "id", "records": [...] }` | both | Send a whole page. |
| `{ "type": "update", "page", "records": [...] }` | both | Send changed records. |

On `hello`, each side sends a `page` for every page whose digest differs
(including one the other has never seen). Merges are idempotent, so an echo of
your own record is a no-op.

## MCP (letting agents edit documents)

The server exposes a [Model Context Protocol](https://modelcontextprotocol.io)
endpoint at `POST /mcp`, so an AI agent can read and edit documents directly.
It uses the stateless Streamable HTTP transport: each request is independent, and
only `POST` is supported (`GET`/`DELETE` return `405`).

The endpoint is mounted on the same HTTP server as the websocket sync, on the
same port. Point an MCP client at `http://127.0.0.1:1234/mcp`. For example, in
an opencode/Claude-style config:

```jsonc
{
  "mcp": {
    "essor": {
      "type": "remote",
      "url": "http://127.0.0.1:1234/mcp",
      "enabled": true
    }
  }
}
```

Tools use structured blocks — `{ kind, text, runs }` where `kind` is
`paragraph`, `heading1`, `heading2`, or `bullet` and runs carry `bold`/`italic`:

| Tool | Purpose |
| --- | --- |
| `list_documents` | List pages in sidebar order. |
| `get_document` | Read a page's blocks, text, and marks. |
| `search_documents` | Case-insensitive search across pages. |
| `create_document` | Create a page (optionally with initial blocks). |
| `delete_document` | Unlist a page. Refuses to delete the last one. |
| `append_blocks` | Append blocks to a page. |
| `replace_document` | Replace a page's entire body. |
| `insert_block` | Insert one block at an index. |
| `update_block` | Change a block's kind and/or content. |
| `delete_block` | Delete a block (never the last one). |

Writes go through the same store the editor syncs, and the changed records are
broadcast to connected editors, so agent edits show up live.

The MCP endpoint has no authentication and can write, so it inherits the same
warning as the sync protocol: keep the loopback bind, or put it behind a trusted
network/proxy.

## Security

This is a development server: it has no authentication or access control, and
anyone who can reach the port can read and edit every document. Keep the default
loopback bind, or put it behind a trusted network/proxy.

## Point Essor at it

Start the server, then run the editor with the base URL of the server:

```sh
ESSOR_SYNC_URL=ws://127.0.0.1:1234 cargo run --release
```

When `ESSOR_SYNC_URL` is set, the client syncs in both directions; local
autosave to SQLite keeps working. Leave the variable unset to run fully offline.

## Smoke test

With a server running, `npm run smoke` opens two clients, verifies that an edit
on one reaches the other, then reconnects and checks the state was persisted.

`npm run mcp-smoke` needs no running server: it boots its own on a random port,
drives the MCP tools through a Streamable HTTP client, and confirms the blocks an
agent writes are visible to a plain websocket client.
