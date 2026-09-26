//! Sync orchestration over one multiplexed websocket.
//!
//! [`SyncSession`] owns the connection and speaks the tiny JSON protocol:
//! `hello` (page digests), `page` (a whole page's records), and `update` (a
//! page's changed records). It is free of UI types: callers supply a [`Notify`]
//! callback and react to the [`Signal`]s it delivers. The wire logic lives in
//! [`crate::net`].

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::Record;
use crate::net::SyncClient;

/// Something the sync layer needs the UI to react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// The socket connected (or reconnected): run the digest handshake.
    Connected,
    /// A raw inbound frame; parse and apply it with [`SyncSession::handle`].
    Message(Vec<u8>),
}

/// What an inbound frame turned out to be, so the caller can react.
pub enum Inbound {
    /// The server's page digests arrived and any differing pages were sent.
    Handshake,
    /// Records were merged; these pages changed.
    Records(Vec<String>),
}

/// The catalog operations the sync protocol drives.
///
/// Implemented by [`crate::library::Library`], so the protocol wire logic never
/// depends on the catalog types (or the UI) directly.
pub trait SyncSource {
    fn has_provisional(&self) -> bool;
    fn confirm_provisional(&mut self);
    fn drop_provisional(&mut self);
    fn digests(&self) -> BTreeMap<String, String>;
    fn records_for_page(&self, page: &str) -> Vec<Record>;
    fn take_outbound(&self) -> BTreeMap<String, Vec<Record>>;
    fn merge_records(&self, records: Vec<Record>) -> Vec<String>;
}

/// Callback used to wake the UI.
pub type Notify = Arc<dyn Fn(Signal) + Send + Sync + 'static>;

/// One message of the sync protocol.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub(crate) enum Msg {
    Hello { pages: BTreeMap<String, String> },
    Page { id: String, records: Vec<Record> },
    Update { page: String, records: Vec<Record> },
}

/// Handle one inbound frame against `source`, sending any handshake replies
/// through `send`. Split out from [`SyncSession`] so tests can drive it without
/// a socket.
pub(crate) fn handle_frame<F, S>(mut send: F, bytes: &[u8], source: &mut S) -> Inbound
where
    F: FnMut(Vec<u8>),
    S: SyncSource,
{
    match serde_json::from_slice::<Msg>(bytes) {
        Ok(Msg::Hello { pages }) => {
            if source.has_provisional() {
                if pages.is_empty() {
                    source.confirm_provisional();
                } else {
                    source.drop_provisional();
                }
            }
            // Send every page whose digest differs (including one the server has
            // never seen). Records surviving a merge are idempotent.
            for (id, digest) in source.digests() {
                if pages.get(&id) != Some(&digest) {
                    let records = source.records_for_page(&id);
                    if let Ok(frame) = serde_json::to_vec(&Msg::Page {
                        id: id.clone(),
                        records,
                    }) {
                        send(frame);
                    }
                }
            }
            Inbound::Handshake
        }
        Ok(Msg::Page { records, .. }) | Ok(Msg::Update { records, .. }) => {
            Inbound::Records(source.merge_records(records))
        }
        Err(_) => Inbound::Records(Vec::new()),
    }
}

/// The client's single sync connection, or `None` when running offline.
pub struct SyncSession {
    client: Option<SyncClient>,
}

impl SyncSession {
    /// Start the session. A `None` (or unsupported) URL yields an inert session.
    pub fn start(url: Option<String>, notify: Notify) -> Self {
        let client = match url {
            Some(url) if crate::net::is_supported_url(&url) => {
                let on_message = {
                    let notify = notify.clone();
                    move |bytes: Vec<u8>| notify(Signal::Message(bytes))
                };
                let on_connected = {
                    let notify = notify.clone();
                    move || notify(Signal::Connected)
                };
                match SyncClient::connect(&url, on_message, on_connected) {
                    Ok(client) => Some(client),
                    Err(error) => {
                        tracing::warn!(%error, %url, "failed to start sync");
                        None
                    }
                }
            }
            Some(url) => {
                tracing::warn!(%url, "ESSOR_SYNC_URL is not a supported ws:// URL; running offline");
                None
            }
            None => None,
        };
        Self { client }
    }

    /// Send our page digests to start (or restart) the handshake.
    pub fn hello<S: SyncSource>(&self, source: &S) {
        self.send_msg(&Msg::Hello {
            pages: source.digests(),
        });
    }

    /// Send a page's changed records.
    pub fn send_update(&self, page: &str, records: Vec<Record>) {
        self.send_msg(&Msg::Update {
            page: page.to_string(),
            records,
        });
    }

    /// Send every page with unsent local changes.
    pub fn flush<S: SyncSource>(&self, source: &S) {
        for (page, records) in source.take_outbound() {
            self.send_update(&page, records);
        }
    }

    /// Handle one inbound frame, driving the handshake and merging records.
    pub fn handle<S: SyncSource>(&self, bytes: &[u8], source: &mut S) -> Inbound {
        handle_frame(|frame| self.send_raw(frame), bytes, source)
    }

    fn send_msg(&self, msg: &Msg) {
        if let Ok(bytes) = serde_json::to_vec(msg) {
            self.send_raw(bytes);
        }
    }

    fn send_raw(&self, bytes: Vec<u8>) {
        if let Some(client) = &self.client {
            client.send(bytes);
        }
    }

    /// Close the connection.
    pub fn disconnect(&mut self) {
        self.client = None;
    }
}
