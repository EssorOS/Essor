//! Real-time document sync over the y-sync websocket protocol.
//!
//! A [`yrs::Doc`] is a cheap handle over an `Arc`-backed store, so a handle can
//! be shared with a background thread while the UI keeps reading it. Crucially,
//! the worker never *writes* the document: local edits are forwarded to it
//! through an observer, and inbound updates are handed back to the owner to
//! apply. That keeps a document's `UndoManager` single-threaded, which `yrs`
//! requires. The thread speaks the same wire protocol as `y-websocket` servers,
//! which keeps it compatible with the bundled Node/TypeScript server and any
//! other Yjs peer.
//!
//! The plumbing is split up as follows:
//!
//! - [`client`] owns the public [`SyncClient`] handle and the worker loop that
//!   keeps the socket in step with the shared document.
//! - [`transport`] opens the socket with bounded connect/reconnect timing.
//! - [`protocol`] decodes and dispatches the y-sync messages.

mod client;
mod protocol;
mod transport;

#[cfg(test)]
mod tests;

pub use client::SyncClient;

/// Whether `url` points at a plaintext websocket endpoint this client can open.
///
/// The sync client links no TLS backend, so a `wss://` URL can never connect;
/// catching it before a worker starts turns a silently-offline run into one
/// clear warning at startup.
pub(crate) fn is_supported_url(url: &str) -> bool {
    url.strip_prefix("ws://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
}

/// Work handed from the shared document observer to the network thread.
enum Bridge {
    /// A local edit that should be forwarded to the server.
    Local(Vec<u8>),
    /// Stop the thread and close the socket.
    Shutdown,
}
