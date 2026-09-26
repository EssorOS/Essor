//! Real-time sync over one multiplexed websocket.
//!
//! A single connection carries the whole workspace: the client sends its page
//! digests on connect, then full pages for the ones that differ, and per-page
//! deltas as it edits. Every frame is JSON; the worker never touches the store —
//! it only forwards bytes to the UI thread and sends bytes the UI hands it.

mod client;
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

/// Work handed from the UI thread to the network thread.
enum Bridge {
    /// A frame to forward to the server.
    Outbound(Vec<u8>),
    /// Stop the thread and close the socket.
    Shutdown,
}
