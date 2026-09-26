//! The public sync handle and the worker loop that drives the connection.

use std::io;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TryRecvError, unbounded};
use tungstenite::Message as WsMessage;

use super::Bridge;
use super::transport::{
    INITIAL_RECONNECT, POLL_INTERVAL, Socket, connect_with_backoff, set_read_timeout,
};

/// Path appended to the base URL for the single multiplexed endpoint.
const SYNC_PATH: &str = "sync";

/// A live connection to the sync server. Dropping it disconnects and asks the
/// worker thread to stop; if it doesn't exit promptly it is detached rather than
/// risking a stall on the UI thread.
pub struct SyncClient {
    outbound: Sender<Bridge>,
    thread: Option<JoinHandle<()>>,
    done: Receiver<()>,
}

impl SyncClient {
    /// Connect to `base_url` (for example `ws://127.0.0.1:1234`).
    ///
    /// `on_message` is invoked on the worker thread for every inbound frame with
    /// its raw bytes; the caller applies it on the document's owning thread.
    /// `on_connected` runs once per successful connection so the UI can kick off
    /// the digest handshake; it fires again after a reconnect.
    pub fn connect<F, C>(base_url: &str, on_message: F, on_connected: C) -> io::Result<Self>
    where
        F: Fn(Vec<u8>) + Send + Sync + 'static,
        C: Fn() + Send + Sync + 'static,
    {
        let (outbound, receiver) = unbounded();
        let url = format!("{}/{}", base_url.trim_end_matches('/'), SYNC_PATH);
        let (done_tx, done) = unbounded();
        let on_message = Arc::new(on_message);
        let on_connected = Arc::new(on_connected);
        let thread = thread::Builder::new()
            .name("essor-sync".to_string())
            .spawn(move || {
                run(url, receiver, on_connected, on_message);
                let _ = done_tx.send(());
            })?;

        Ok(Self {
            outbound,
            thread: Some(thread),
            done,
        })
    }

    /// Queue a frame to send on the next flush.
    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.outbound.send(Bridge::Outbound(bytes));
    }
}

impl Drop for SyncClient {
    fn drop(&mut self) {
        let _ = self.outbound.send(Bridge::Shutdown);
        if self.done.recv_timeout(POLL_INTERVAL * 2).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

/// Worker loop: keep the websocket in step with the outbound queue, retrying
/// with exponential backoff whenever the connection drops.
fn run(
    url: String,
    receiver: Receiver<Bridge>,
    on_connected: Arc<dyn Fn() + Send + Sync>,
    on_message: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let mut backoff = INITIAL_RECONNECT;
    let mut deferred: Vec<Vec<u8>> = Vec::new();

    loop {
        let Some(mut socket) = connect_with_backoff(&url, &receiver, &mut backoff, &mut deferred)
        else {
            return; // asked to shut down
        };
        backoff = INITIAL_RECONNECT;
        set_read_timeout(&mut socket, Some(POLL_INTERVAL));

        (on_connected)();

        // Replay frames queued while disconnected. If one fails, the reconnect
        // handshake will still recover the records.
        let mut send_failed = false;
        for bytes in deferred.drain(..) {
            if socket.send(WsMessage::Binary(bytes.into())).is_err() {
                send_failed = true;
                break;
            }
        }
        if send_failed {
            continue;
        }

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
                Ok(WsMessage::Binary(data)) => (on_message)(data.to_vec()),
                Ok(WsMessage::Text(text)) => (on_message)(text.to_string().into_bytes()),
                Ok(WsMessage::Ping(data)) => {
                    let _ = socket.send(WsMessage::Pong(data));
                }
                Ok(WsMessage::Close(_)) => break,
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::TimedOut =>
                {
                    // Nothing to read within the poll window; loop to flush writes.
                }
                Err(_) => break,
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

/// Forward queued frames, reporting whether the connection is still usable.
fn drain(receiver: &Receiver<Bridge>, socket: &mut Socket) -> Drained {
    loop {
        match receiver.try_recv() {
            Ok(Bridge::Outbound(bytes)) => {
                if socket.send(WsMessage::Binary(bytes.into())).is_err() {
                    return Drained::Reconnect;
                }
            }
            Ok(Bridge::Shutdown) => return Drained::Shutdown,
            Err(TryRecvError::Empty) => return Drained::Continue,
            Err(TryRecvError::Disconnected) => return Drained::Shutdown,
        }
    }
}
