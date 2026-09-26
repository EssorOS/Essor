# Essor

A small, fast, block-based text editor written in Rust. Essor owns its own
document model and editing semantics, and renders text directly on the GPU.

Each paragraph is a *block* that can be a paragraph, a heading, or a bullet
list item. Content is stored in a CRDT, so the document format is ready for
real-time collaboration and undo/redo without a custom history layer.

## Features

- **Block model** — Paragraph, Heading 1, Heading 2, and Bullet list blocks.
- **Markdown-style shortcuts** — typing `# `, `## `, `- `, or `* ` at the
  start of a paragraph converts it to the matching block kind.
- **Slash menu** — type `/` to open a filterable command menu.
- **Gutter menu** — hover a block to insert a new block below (`+`) or change
  its kind / delete it (`⋮`).
- **Inline marks** — bold and italic over arbitrary ranges.
- **Rich text editing** — word/line-aware selection, grapheme-correct caret
  motion (combining marks, emoji ZWJ sequences), IME composition, and
  clipboard cut/copy/paste.
- **Document sidebar** — create new pages, switch between them, and delete
  them (with confirmation) from a list down the left edge.
- **Undo / redo** driven by the CRDT's undo manager.
- **Autosave** — each document is persisted to its own `.ydoc` file after every
  edit, with the page list tracked in a small JSON index.
- **GPU rendering** — text is shaped with Parley and painted with Vello.

## Requirements

- Rust with support for the 2024 edition (Rust 1.85+).
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

Each page is saved as its own Yjs update file. The page list — which pages
exist, their titles, and their order — is itself a Yjs document, persisted
alongside them:

```
<data dir>/essor/library.ydoc
<data dir>/essor/documents/<id>.ydoc
```

On macOS that is typically under
`~/Library/Application Support/essor/`. Delete the directory to start over.

## Real-time sync

Essor can sync documents live through a small Node/TypeScript websocket server
that speaks the standard Yjs protocol. The sidebar list — which pages exist,
their titles, and their order — is synced through a shared `library` room, and
each page's content is synced through a room named after its id. Only the active
page holds a content connection.

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
automatically with backoff if the server restarts, and edits made while
disconnected are reconciled on reconnect. The server persists documents to
LevelDB under `server/data`; see [server/README.md](server/README.md) for
configuration and a smoke test.

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

The document model lives behind the `Doc` trait in `src/doc.rs`, so the UI
never depends on the CRDT crate directly. The current backend is
`YrsDocument`, a `yrs` (Yjs) document holding a root array of block maps, each
with a `kind` and a rich `Y.Text`.

`src/library.rs` owns the set of on-disk documents (the files and the JSON
index) and `src/sidebar.rs` is the custom-painted document list. The root layout
is a `Flex` row of the sidebar and a scrolling editor; the sidebar emits
`SidebarAction`s that `main.rs` handles by swapping the editor's `Doc`.

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
sync client: it shares a `yrs` document with a worker thread, forwards local
edits to the server and relays remote edits back to the UI. `src/session.rs`
orchestrates the connections — the shared page list and the active page — and
`src/library.rs` owns the catalog and the local `.ydoc` files.

## License

MIT — see [LICENSE](LICENSE).
