/**
 * Wire constants shared by the sync server and the y-sync protocol.
 *
 * These values are fixed by the Yjs websocket protocol; the Rust client ships
 * the same tags via `yrs::sync`.
 */
export const MESSAGE_SYNC = 0;
export const MESSAGE_AWARENESS = 1;
export const MESSAGE_AUTH = 2;
export const MESSAGE_QUERY_AWARENESS = 3;

/** How long a connection may go without a pong before it is dropped. */
export const PING_TIMEOUT_MS = 30_000;

/** How long to wait for the quit signal during graceful shutdown. */
export const SHUTDOWN_WAIT_MS = 2_000;
