# Essor sync server

A small Yjs websocket server that keeps [Essor](../README.md) documents in
sync across clients. It speaks the standard Yjs websocket protocol, so any
`y-websocket`-compatible client (including `yrs`, the Rust client Essor uses)
can connect.

Each Essor page is a document id, and each document id is a websocket *room*:
connect to `ws://<host>:<port>/<document-id>`.

## Run it

```sh
cd server
npm install
npm run dev      # watch mode, TypeScript directly
# or
npm run build && npm start
```

The server listens on `ws://127.0.0.1:1234` by default and persists documents to
`server/data` with [LevelDB](https://www.npmjs.com/package/y-leveldb). Set
`HOST=0.0.0.0` to accept connections from the network.

| Variable | Default | Description |
| --- | --- | --- |
| `HOST` | `127.0.0.1` | Interface to bind. |
| `PORT` | `1234` | Port to listen on. |
| `DATA_DIR` | `server/data` | LevelDB directory for persisted documents. |

`GET /health` returns `{ "ok": true, "documents": <count> }`.

## Security

This is a development server: it has no authentication or access control, and
anyone who can reach the port can read and edit every document. Keep the default
loopback bind, or put it behind a trusted network/proxy. Room names are validated
(no `..`, control characters, or names longer than 256 bytes).

## Point Essor at it

Start the server, then run the editor with the base URL of the server:

```sh
ESSOR_SYNC_URL=ws://127.0.0.1:1234 cargo run --release
```

When `ESSOR_SYNC_URL` is set, the active page connects to the matching room
and syncs in both directions; local autosave to `.ydoc` files keeps working.
Leave the variable unset to run fully offline.

## Smoke test

With a server running, `npm run smoke` opens two clients against a throwaway
room, verifies that an edit on one reaches the other, then reconnects and
checks the state was persisted.
