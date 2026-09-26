//! The y-sync wire protocol.
//!
//! This layer never mutates the shared document. Inbound updates are handed to
//! a caller-supplied `on_remote` callback so the document's owning thread can
//! apply them; only read-only work (answering a `SyncStep1` with a state diff)
//! touches the document here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::Sender;
use tungstenite::Message as WsMessage;
use yrs::encoding::read::Cursor;
use yrs::sync::{Message, MessageReader, SyncMessage};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Doc, ReadTxn, Transact};

use super::transport::Socket;

/// Per-connection handshake bookkeeping, shared with the worker loop.
pub(super) struct SyncState {
    /// Signalled once the peer's initial state has been received.
    ready: Sender<()>,
    /// Called once per connection when the handshake completes, even when the
    /// peer's document is empty and so raises no update.
    on_synced: Arc<dyn Fn() + Send + Sync>,
    /// Guards `on_synced` so it fires once for this connection.
    fired: AtomicBool,
}

impl SyncState {
    pub(super) fn new(ready: Sender<()>, on_synced: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            ready,
            on_synced,
            fired: AtomicBool::new(false),
        }
    }

    fn complete_handshake(&self) {
        let _ = self.ready.try_send(());
        if !self.fired.swap(true, Ordering::SeqCst) {
            (self.on_synced)();
        }
    }
}

/// Send only the sync handshake. The default y-sync `start` also advertises an
/// awareness state, but this client has no presence to share.
pub(super) fn send_sync_step1(socket: &mut Socket, doc: &Doc) -> Result<(), tungstenite::Error> {
    let state_vector = doc.transact().state_vector();
    send_message(socket, Message::Sync(SyncMessage::SyncStep1(state_vector)))
}

/// Feed one inbound websocket frame through the protocol, replying as needed.
/// Each update the peer sent is passed to `on_remote` as raw bytes rather than
/// applied here.
pub(super) fn handle_message(
    message: WsMessage,
    socket: &mut Socket,
    doc: &Doc,
    state: &SyncState,
    on_remote: &(dyn Fn(Vec<u8>) + Send + Sync),
) {
    match message {
        WsMessage::Binary(data) => handle_binary(data.as_ref(), socket, doc, state, on_remote),
        WsMessage::Ping(data) => {
            let _ = socket.send(WsMessage::Pong(data));
        }
        WsMessage::Close(_) => {
            let _ = socket.close(None);
        }
        _ => {}
    }
}

/// Decode and dispatch every y-sync message in one websocket frame. Updates
/// need no reply, so they are forwarded; a `SyncStep1` is answered with a
/// read-only diff from the shared document.
fn handle_binary(
    data: &[u8],
    socket: &mut Socket,
    doc: &Doc,
    state: &SyncState,
    on_remote: &(dyn Fn(Vec<u8>) + Send + Sync),
) {
    let mut decoder = DecoderV1::new(Cursor::new(data));
    for result in MessageReader::new(&mut decoder) {
        let message = match result {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(%error, "sync: invalid message");
                return;
            }
        };
        match message {
            Message::Sync(SyncMessage::SyncStep1(state_vector)) => {
                let update = doc.transact().encode_state_as_update_v1(&state_vector);
                let reply = Message::Sync(SyncMessage::SyncStep2(update));
                if let Err(error) = send_message(socket, reply) {
                    tracing::debug!(%error, "sync: failed to send reply");
                }
            }
            Message::Sync(SyncMessage::SyncStep2(update)) => {
                on_remote(update);
                state.complete_handshake();
            }
            Message::Sync(SyncMessage::Update(update)) => on_remote(update),
            _ => {}
        }
    }
}

/// Encode and send a y-sync protocol message.
pub(super) fn send_message(
    socket: &mut Socket,
    message: Message,
) -> Result<(), tungstenite::Error> {
    let mut encoder = EncoderV1::new();
    message.encode(&mut encoder);
    socket.send(WsMessage::Binary(encoder.to_vec().into()))
}
