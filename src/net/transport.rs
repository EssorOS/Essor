//! Opening the websocket and reconnecting with bounded timing.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use tungstenite::WebSocket;
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;

use super::Bridge;

/// How often the network thread wakes to flush queued local edits and to check
/// whether it has been asked to shut down.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Delay before the first reconnection attempt, and the cap it backs off to.
pub(super) const INITIAL_RECONNECT: Duration = Duration::from_millis(250);
const MAX_RECONNECT: Duration = Duration::from_secs(10);

/// Upper bound on a single TCP connect attempt. Without this a dead server
/// blocks the worker for the OS default timeout, which also delays shutdown.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);

pub(super) type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

/// Open a connection, retrying with exponential backoff until one succeeds or
/// the worker is told to shut down. Frames queued while waiting are collected in
/// `deferred` and sent once a connection is up, rather than discarded.
pub(super) fn connect_with_backoff(
    url: &str,
    receiver: &Receiver<Bridge>,
    backoff: &mut Duration,
    deferred: &mut Vec<Vec<u8>>,
) -> Option<Socket> {
    loop {
        if let Ok(socket) = connect_bounded(url) {
            return Some(socket);
        }

        let deadline = Instant::now() + *backoff;
        *backoff = (*backoff * 2).min(MAX_RECONNECT);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receiver.recv_timeout(remaining) {
                Ok(Bridge::Shutdown) | Err(RecvTimeoutError::Disconnected) => return None,
                Ok(Bridge::Outbound(bytes)) => deferred.push(bytes),
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
    }
}

/// Open a websocket with a bounded TCP connect time. `tungstenite::connect`
/// uses the OS default, which can stall shutdown for tens of seconds when the
/// server is unreachable.
fn connect_bounded(url: &str) -> Result<Socket, tungstenite::Error> {
    let request = url.into_client_request()?;
    let uri = request.uri();
    if uri.scheme_str() == Some("wss") {
        return Err(tungstenite::Error::Url(
            tungstenite::error::UrlError::TlsFeatureNotEnabled,
        ));
    }
    let host = uri.host().ok_or_else(|| {
        tungstenite::Error::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sync url has no host",
        ))
    })?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri.port_u16().unwrap_or(80);

    let mut connected = None;
    for addr in (host, port).to_socket_addrs()? {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            connected = Some(stream);
            break;
        }
    }
    let stream = connected.ok_or_else(|| {
        tungstenite::Error::Io(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "could not connect to sync server",
        ))
    })?;
    let _ = stream.set_nodelay(true);

    let (socket, _response) = tungstenite::client(request, MaybeTlsStream::Plain(stream)).map_err(
        |error| match error {
            tungstenite::HandshakeError::Failure(error) => error,
            tungstenite::HandshakeError::Interrupted(_) => tungstenite::Error::Io(io::Error::new(
                io::ErrorKind::WouldBlock,
                "sync handshake interrupted",
            )),
        },
    )?;
    Ok(socket)
}

/// Put a read timeout on the socket so the loop can interleave reads and writes.
pub(super) fn set_read_timeout(socket: &mut Socket, timeout: Option<Duration>) {
    if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
        let _ = stream.set_read_timeout(timeout);
    }
}
