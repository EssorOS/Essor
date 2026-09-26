# Essor

A small, fast, block-based text editor written in Rust. Essor owns its own
document model and editing semantics, and renders text directly on the GPU.

Each paragraph is a *block* that can be a paragraph, a heading, or a bullet
list item. Blocks are records in a last-write-wins store, merged per block, so
the document format supports real-time collaboration and undo/redo.

## Features

- **Block model** — Paragraph, Heading 1, Heading 2, and Bullet list blocks.
- **Markdown-style shortcuts** — typing `# `, `## `, `- `, or `* ` at the
  start of a paragraph converts it to the matching block kind.
- **Slash menu** — type `/` to open a filterable command menu.
- **Gutter menu** — hover a block to insert a new block below (`+`) or change
  its kind / delete it (`⋮`).
- **Inline marks** — bold and italic over arbitrary ranges, preserved across
  edits.
- **Rich text editing** — word/line-aware selection, grapheme-correct caret
  motion (combining marks, emoji ZWJ sequences), IME composition, and
  clipboard cut/copy/paste.
- **Document sidebar** — create new pages, switch between them, and delete
  them (with confirmation) from a list down the left edge.
- **Undo / redo** — scoped to your own edits, so it never clobbers a peer.
- **Autosave** — every edit is written to a local SQLite database.
- **GPU rendering** — text is shaped with Parley and painted with Vello.

## Requirements

- Rust 1.89+ (the 2024 edition, plus `File::try_lock` for the data-dir lock).
- Developed and tested on macOS. Non-macOS builds are expected to work; the
  macOS-specific window-resize fix is compiled out elsewhere.

## Build and run

```sh
cargo run
```

The release profile is recommended for interactive use:

```sh
cargo run --release
```

Run the tests:

```sh
cargo test
```

By default the app installs a quiet log subscriber. Set `RUST_LOG` to control
logging (for example `RUST_LOG=warn` or `RUST_LOG=debug`).

## Where documents are stored

All pages live in one SQLite database:

```
<data dir>/essor/essor.db
```

On macOS that is typically under `~/Library/Application Support/essor/`. Delete
the directory to start over.

Only one instance can use a data directory at a time. To run a second instance
(for example to test sync locally), point it at its own directory:

```sh
ESSOR_DATA_DIR=$(mktemp -d) cargo run --release
```

A second instance started without `ESSOR_DATA_DIR` exits with an error, because
two processes sharing one database would also share a client id and break
last-write-wins.

## Real-time sync

Essor can sync live through a small Node/TypeScript websocket server. One
connection carries the whole workspace: on connect the client and server
exchange per-page digests, each side sends the pages whose digest differs, and
edits afterwards are sent as per-page record deltas. Records are merged by
last-write-wins on a Lamport clock, so offline edits reconcile automatically.

Merging is at *block* granularity: a block's text and marks are one record, so
two people editing the same block concurrently resolve last-write-wins and the
losing version is replaced wholesale. Edits to different blocks merge cleanly;
character-level merging within one block (as a text CRDT would provide) is not
implemented. Deletes are tombstones that stay in the store so they keep
propagating across reconnects, so the digest map grows with the number of pages
ever created.

Start the server:

```sh
cd server
npm install
npm run dev
```

Then point the editor at it:

```sh
ESSOR_SYNC_URL=ws://127.0.0.1:1234 cargo run --release
```

With `ESSOR_SYNC_URL` unset the editor runs fully offline. Connections retry
automatically with backoff if the server restarts. The server persists to SQLite
under `server/data`; see [server/README.md](server/README.md) for configuration
and a smoke test.

## Agents (MCP)

The sync server also exposes a Model Context Protocol endpoint at `POST /mcp`,
so agents can list, read, create, and edit documents with structured blocks.
With the server running, point an MCP client at `http://127.0.0.1:1234/mcp`; see
[server/README.md](server/README.md#mcp-letting-agents-edit-documents) for the
tool list and an example config.

## Keyboard and mouse

| Action | Binding |
| --- | --- |
| New block | `Enter` |
| Delete backward / forward | `Backspace` / `Delete` |
| Move caret | Arrow keys |
| Move by word | `Alt`/`Ctrl` + Left/Right |
| Move to line start/end | `Home` / `End` |
| Select | Hold `Shift` while moving |
| Select word / line | Double-click / triple-click, then drag to extend |
| Bold / italic | `Cmd`/`Ctrl` + `B` / `I` |
| Heading 1 / Heading 2 / Paragraph | `Cmd`/`Ctrl` + `Alt` + `1` / `2` / `0` |
| Undo / redo | `Cmd`/`Ctrl` + `Z` / `Shift+Z` |
| Copy / cut / paste | `Cmd`/`Ctrl` + `C` / `X` / `V` |
| Select all | `Cmd`/`Ctrl` + `A` |
| Slash menu | Type `/`; navigate with arrows, confirm with `Enter`/`Tab`, dismiss with `Esc` |

On macOS the command modifier is `Cmd`; on other platforms it is `Ctrl`.

## Architecture

The editing interface is the `Doc` trait in `src/doc.rs`; the UI never depends on
the store. The concrete implementation is `PageHandle` (`src/page.rs`), a
per-page view over one `Workspace` (`src/workspace.rs`), which owns every page
and block as a record in SQLite and merges them by last-write-wins on a Lamport
clock. `src/runs.rs` holds the pure run-splicing helpers that preserve marks
across an edit. Deletes are tombstones; block order uses fractional indices. Each
page has a SHA-256 digest over its record versions for the reconnect handshake.

`src/library.rs` is the UI-facing catalog (the page list, titles and order, the
active page) built on the workspace. `src/sidebar.rs` is the custom-painted
document list. The root layout is a `Flex` row of the sidebar and a scrolling
editor; the sidebar emits `SidebarAction`s that `main.rs` handles by swapping the
editor's `Doc`.

The editor widget is split into focused modules under `src/editor/`:

| Module | Responsibility |
| --- | --- |
| `editor.rs` | `Editor` state, construction, the Masonry `Widget` impl |
| `layout.rs` | Per-block Parley layout cache and incremental relayout |
| `selection.rs` | Cursor/selection state, hit testing, drag selection, pointer events |
| `cursor.rs` | Caret geometry and keyboard cursor motion |
| `edit.rs` | Text mutations, marks, clipboard, history, key handling |
| `menu.rs` | Slash menu and block/gutter menu |
| `paint.rs` | Vello painting of text, carets, selections, and menus |
| `text.rs` | Grapheme and byte-offset utilities |
| `theme.rs` | Colors, metrics, and layout constants |

`src/blink.rs` owns the caret-blink timer thread, and `src/main.rs` wires the
widget into a `masonry_winit` window. `src/net/` owns the background websocket
client: a single multiplexed connection that forwards frames between the server
and the UI thread. `src/session.rs` owns the handshake and the JSON message
types.

## License

MIT — see [LICENSE](LICENSE).
