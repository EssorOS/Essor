//! The public sync handle and the worker loop that drives one document room.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, unbounded};
use yrs::Doc;
use yrs::sync::{Message, SyncMessage};

use super::Bridge;
use super::protocol::{SyncState, handle_message, send_message, send_sync_step1};
use super::transport::{
    INITIAL_RECONNECT, POLL_INTERVAL, Socket, connect_with_backoff, set_read_timeout,
};
use crate::doc::REMOTE_ORIGIN;

/// Source of unique observer keys. Two sync clients sharing one document must
/// not overwrite each other's observer, so each takes a distinct key.
static OBSERVER_SEQ: AtomicU64 = AtomicU64::new(0);

/// A live connection to one document's room. Dropping it disconnects and asks
/// the worker thread to stop; if it doesn't exit immediately (for example it is
/// blocked in a connect) it is detached rather than risking a stall on the
/// caller (the UI thread).
pub struct SyncClient {
    bridge: Sender<Bridge>,
    doc: Doc,
    thread: Option<JoinHandle<()>>,
    done: Receiver<()>,
    ready: Receiver<()>,
    observer_key: String,
}

impl SyncClient {
    /// Connect `doc` to `room` on the server at `base_url` (for example
    /// `ws://127.0.0.1:1234`).
    ///
    /// `on_remote` is invoked on the worker thread for every update the peer
    /// sent, with its raw bytes; the caller is responsible for applying it
    /// (typically on the document's owning thread). `on_synced` runs once per
    /// connection when the handshake completes, even when the peer's document is
    /// empty, which raises no update.
    pub fn connect<F, S>(
        base_url: &str,
        room: &str,
        doc: Doc,
        on_remote: F,
        on_synced: S,
    ) -> io::Result<Self>
    where
        F: Fn(Vec<u8>) + Send + Sync + 'static,
        S: Fn() + Send + Sync + 'static,
    {
        let (bridge, receiver) = unbounded();

        let observer_key = format!(
            "essor-sync-{}",
            OBSERVER_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        doc.observe_update_v1(observer_key.clone(), {
            let bridge = bridge.clone();
            move |txn, event| {
                // Remote updates are applied by the owner, which does not echo
                // them back; only local edits are forwarded to the server.
                let remote = txn
                    .origin()
                    .map(|origin| origin.as_ref() == REMOTE_ORIGIN.as_bytes())
                    .unwrap_or(false);
                if !remote {
                    let _ = bridge.send(Bridge::Local(event.update.clone()));
                }
            }
        })
        .map_err(|error| io::Error::other(error.to_string()))?;

        let url = format!("{}/{}", base_url.trim_end_matches('/'), room);
        let worker_doc = doc.clone();
        let (done_tx, done) = unbounded();
        let (ready_tx, ready) = bounded(1);
        let on_remote = Arc::new(on_remote);
        let on_synced = Arc::new(on_synced);
        let thread = match thread::Builder::new()
            .name(format!("essor-sync-{room}"))
            .spawn(move || {
                run(url, worker_doc, receiver, ready_tx, on_synced, on_remote);
                let _ = done_tx.send(());
            }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ = doc.unobserve_update_v1(observer_key.as_str());
                return Err(error);
            }
        };

        Ok(Self {
            bridge,
            doc,
            thread: Some(thread),
            done,
            ready,
            observer_key,
        })
    }

    /// Block until the server's initial state has been received, or `timeout`
    /// elapses. Unlike observing document updates this also completes when the
    /// shared document is empty, so callers can tell "server has nothing" from
    /// "not connected yet".
    pub fn wait_synced(&self, timeout: Duration) -> bool {
        self.ready.recv_timeout(timeout).is_ok()
    }
}

impl Drop for SyncClient {
    fn drop(&mut self) {
        let _ = self.bridge.send(Bridge::Shutdown);
        let _ = self.doc.unobserve_update_v1(self.observer_key.as_str());
        // Wait briefly for the worker to observe the shutdown. An idle worker is
        // in its read loop and exits within a poll interval; one blocked in a
        // connect can take up to `CONNECT_TIMEOUT`, so detach rather than stall
        // the caller (the UI thread). A detached worker may briefly hold a
        // duplicate room connection; the UI ignores its remote callbacks unless
        // the room is still the active page.
        if self.done.recv_timeout(POLL_INTERVAL * 2).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// Worker loop: keep the websocket in step with the shared document, retrying
/// with exponential backoff whenever the connection drops.
fn run(
    url: String,
    doc: Doc,
    receiver: Receiver<Bridge>,
    ready: Sender<()>,
    on_synced: Arc<dyn Fn() + Send + Sync>,
    on_remote: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let mut backoff = INITIAL_RECONNECT;

    loop {
        let Some(mut socket) = connect_with_backoff(&url, &receiver, &mut backoff) else {
            return; // asked to shut down
        };
        backoff = INITIAL_RECONNECT;
        set_read_timeout(&mut socket, Some(POLL_INTERVAL));

        // A fresh state per connection so `on_synced` fires again after a
        // reconnect (the server may have gained content while we were away).
        let state = SyncState::new(ready.clone(), on_synced.clone());

        // Announce our state. The server replies with whatever we're missing,
        // which also replays any edits made while disconnected.
        if send_sync_step1(&mut socket, &doc).is_err() {
            continue;
        }
        tracing::debug!(%url, "sync: connected");

        loop {
            match drain(&receiver, &mut socket) {
                Drained::Continue => {}
                Drained::Reconnect => break,
                Drained::Shutdown => {
                    let _ = socket.close(None);
                    return;
                }
            }

            match socket.read() {
                Ok(message) => {
                    handle_message(message, &mut socket, &doc, &state, on_remote.as_ref())
                }
                Err(tungstenite::Error::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    // Nothing to read within the poll window; loop to flush writes.
                }
                Err(error) => {
                    tracing::debug!(%error, %url, "sync: disconnected, reconnecting");
                    break;
                }
            }
        }
    }
}

/// How the worker should proceed after flushing queued work.
enum Drained {
    Continue,
    Reconnect,
    Shutdown,
}

/// Forward queued bridge messages, reporting whether the connection is still
/// usable.
fn drain(receiver: &Receiver<Bridge>, socket: &mut Socket) -> Drained {
    loop {
        match receiver.try_recv() {
            Ok(Bridge::Local(update)) => {
                let message = Message::Sync(SyncMessage::Update(update));
                if send_message(socket, message).is_err() {
                    return Drained::Reconnect;
                }
            }
            Ok(Bridge::Shutdown) => return Drained::Shutdown,
            Err(TryRecvError::Empty) => return Drained::Continue,
            Err(TryRecvError::Disconnected) => return Drained::Shutdown,
        }
    }
}
