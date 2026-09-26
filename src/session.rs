//! Real-time sync orchestration for the shared catalog and the active page.
//!
//! [`SyncSession`] owns the websocket connections: one to the shared `library`
//! room for the page list, and at most one to the active page's room. It is
//! deliberately free of UI types: callers supply a [`Notify`] callback and react
//! to the [`Signal`]s it delivers. The wire protocol lives in [`crate::net`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use yrs::updates::decoder::Decode;
use yrs::{Doc as YrsDoc, Transact, Update};

use crate::doc::REMOTE_ORIGIN;
use crate::library::Library;
use crate::net::SyncClient;

/// How long to give the initial catalog handshake at startup so a reachable
/// server's list can be adopted before the window is built. A timeout is safe:
/// the handshake's completion raises another [`Signal::LibraryChanged`].
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(250);

/// Fixed room name for the shared page list. Page ids are UUIDs, so this can
/// never collide with a page.
pub const LIBRARY_ROOM: &str = "library";

/// Something the sync layer needs the UI to react to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A remote edit arrived for the page in `room`. Carries the raw update so
    /// the UI, which owns the document, can apply it; carrying the room lets it
    /// ignore edits for a page it has since navigated away from.
    RemoteUpdate { room: String, update: Vec<u8> },
    /// The shared catalog changed.
    LibraryChanged,
}

/// Callback used to wake the UI. Shared by every connection the session owns.
pub type Notify = Arc<dyn Fn(Signal) + Send + Sync + 'static>;

/// A connection to the shared catalog, plus at most one to the active page.
pub struct SyncSession {
    /// Base websocket URL, or `None` when running offline.
    url: Option<String>,
    /// Connection for the active page, if one is open.
    document: Option<SyncClient>,
    /// Connection to the shared catalog, if configured.
    list: Option<SyncClient>,
    notify: Notify,
}

impl SyncSession {
    /// Connect the catalog to the shared list and guarantee at least one page
    /// exists. Local pages are merged automatically by the CRDT and uploaded on
    /// the handshake, so a client joining an existing library adopts it and a
    /// fresh server is seeded with one page.
    ///
    /// A `None` `url` yields an inert session; only the seed happens.
    ///
    /// `catalog_dirty` is set whenever a remote update changes the catalog, so
    /// the caller can persist it — remote changes must survive an offline
    /// restart just like local ones.
    pub fn start(
        url: Option<String>,
        notify: Notify,
        catalog: YrsDoc,
        catalog_dirty: Arc<AtomicBool>,
        library: &mut Library,
    ) -> Self {
        let url = match url {
            Some(url) if crate::net::is_supported_url(&url) => Some(url),
            Some(url) => {
                tracing::warn!(%url, "ESSOR_SYNC_URL is not a supported ws:// URL; running offline");
                None
            }
            None => None,
        };
        let mut session = Self {
            url,
            document: None,
            list: None,
            notify,
        };

        if let Some(url) = session.url.clone() {
            let notify = session.notify.clone();
            let remote = catalog.clone();
            let dirty = catalog_dirty;
            let on_remote = move |update: Vec<u8>| {
                let Ok(update) = Update::decode_v1(&update) else {
                    return;
                };
                // The catalog has no undo manager, so its worker thread may
                // apply remote updates directly, exactly like the UI thread.
                let mut txn = remote.transact_mut_with(REMOTE_ORIGIN);
                let _ = txn.apply_update(update);
                // The same changed test as `YrsDocument::apply_remote`: marks a
                // genuine remote edit (never an echo) for local persistence.
                let changed =
                    !txn.delete_set().is_empty() || txn.after_state() != txn.before_state();
                drop(txn);
                // An unchanged update is the server echoing our own edit back;
                // notifying would re-run `refresh_library` for nothing.
                if changed {
                    dirty.store(true, Ordering::Relaxed);
                    notify(Signal::LibraryChanged);
                }
            };
            let on_synced = {
                let notify = session.notify.clone();
                // An empty shared list raises no update, so the handshake
                // completing is itself a reason to reconcile.
                move || notify(Signal::LibraryChanged)
            };
            match SyncClient::connect(&url, LIBRARY_ROOM, catalog, on_remote, on_synced) {
                Ok(sync) => {
                    // A brief head start so a reachable server's list is adopted
                    // before the window is built. Bounded; a timeout is fine.
                    sync.wait_synced(HANDSHAKE_TIMEOUT);
                    session.list = Some(sync);
                }
                Err(error) => tracing::warn!(%error, %url, "failed to start catalog sync"),
            }
        }

        if library.entries().is_empty() {
            library.create();
            library.save();
        }

        session
    }

    /// (Re)connect the active page to its room, replacing any existing page
    /// connection. A no-op when no server is configured.
    ///
    /// Remote updates are forwarded to the UI as [`Signal::RemoteUpdate`] rather
    /// than applied here: this document has an undo manager, and `yrs` requires
    /// its owner to be the only writer.
    pub fn connect_document(&mut self, doc: YrsDoc, room: String) {
        self.document = None;
        let Some(url) = self.url.clone() else {
            return;
        };
        let notify = self.notify.clone();
        let on_remote = {
            let room = room.clone();
            move |update: Vec<u8>| {
                notify(Signal::RemoteUpdate {
                    room: room.clone(),
                    update,
                });
            }
        };
        match SyncClient::connect(&url, &room, doc, on_remote, || {}) {
            Ok(client) => self.document = Some(client),
            Err(error) => tracing::warn!(%error, %url, %room, "failed to start page sync"),
        }
    }

    /// Close every connection.
    pub fn disconnect(&mut self) {
        self.document = None;
        self.list = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use yrs::ReadTxn;

    use crate::test_support::{TestServer, temp_dir, wait_until};

    use super::*;

    /// Apply inbound catalog updates to `doc` the way the session does.
    fn applier(doc: &YrsDoc) -> impl Fn(Vec<u8>) + Send + Sync + 'static {
        let doc = doc.clone();
        move |bytes: Vec<u8>| {
            let Ok(update) = Update::decode_v1(&bytes) else {
                return;
            };
            let mut txn = doc.transact_mut_with(REMOTE_ORIGIN);
            let _ = txn.apply_update(update);
        }
    }

    /// A raw connection to the shared library room backed by `library`.
    fn connect_catalog(url: &str, library: &Library) -> SyncClient {
        let doc = library.catalog_handle();
        SyncClient::connect(
            url,
            LIBRARY_ROOM,
            doc,
            applier(&library.catalog_handle()),
            || {},
        )
        .unwrap()
    }

    fn sees(library: &Library, id: &str) -> bool {
        library.entries().iter().any(|meta| meta.id == id)
    }

    #[test]
    fn start_seeds_a_page_when_the_shared_list_is_empty() {
        let Some(server) = TestServer::start("session-empty") else {
            return;
        };

        let mut library = Library::open(Some(temp_dir("session-empty-lib")));
        let notify: Notify = Arc::new(|_| {});
        let _session = SyncSession::start(
            Some(server.url()),
            notify,
            library.catalog_handle(),
            library.catalog_dirty(),
            &mut library,
        );

        assert_eq!(library.entries().len(), 1);
    }

    #[test]
    fn start_seeds_offline() {
        let mut library = Library::open(Some(temp_dir("session-offline-lib")));
        let notify: Notify = Arc::new(|_| {});
        let _session = SyncSession::start(
            None,
            notify,
            library.catalog_handle(),
            library.catalog_dirty(),
            &mut library,
        );
        assert_eq!(library.entries().len(), 1);
    }

    #[test]
    fn start_adopts_pages_the_server_already_has() {
        let Some(server) = TestServer::start("session-existing") else {
            return;
        };
        let url = server.url();

        // Pre-populate the shared list and confirm it reached the server, then
        // drop the seed so only the server holds it.
        {
            let mut host = Library::open(Some(temp_dir("session-host")));
            let id = host.create();
            host.save();

            let _host_client = connect_catalog(&url, &host);
            let probe = Library::open(Some(temp_dir("session-probe")));
            let _probe_client = connect_catalog(&url, &probe);
            assert!(
                wait_until(|| sees(&probe, &id), Duration::from_secs(5)),
                "seed page did not reach the server"
            );
        }

        let mut library = Library::open(Some(temp_dir("session-lib")));
        let notify: Notify = Arc::new(|_| {});
        let _session = SyncSession::start(
            Some(url),
            notify,
            library.catalog_handle(),
            library.catalog_dirty(),
            &mut library,
        );

        // `start` must adopt the remote page rather than inventing a blank one.
        assert_eq!(library.entries().len(), 1);
    }

    #[test]
    fn start_does_not_duplicate_a_page_already_local() {
        let Some(server) = TestServer::start("session-local") else {
            return;
        };
        let url = server.url();

        let mut host = Library::open(Some(temp_dir("session-local-host")));
        let id = host.create();
        host.save();

        let mut library = Library::open(Some(temp_dir("session-local-lib")));
        // Mirror the host's catalog into the local library, as a client that
        // synced on an earlier run would have.
        {
            let host_doc = host.catalog_handle();
            let sv = host_doc.transact().state_vector();
            let update = host_doc.transact().encode_state_as_update_v1(&sv);
            let lib_doc = library.catalog_handle();
            let mut txn = lib_doc.transact_mut();
            let _ = txn.apply_update(Update::decode_v1(&update).unwrap());
        }
        library.save();

        // Confirm the page reaches the server, then drop the seed connection.
        {
            let _host_client = connect_catalog(&url, &host);
            let probe = Library::open(Some(temp_dir("session-local-probe")));
            let _probe_client = connect_catalog(&url, &probe);
            assert!(
                wait_until(|| sees(&probe, &id), Duration::from_secs(5)),
                "page did not reach the server"
            );
        }

        let notify: Notify = Arc::new(|_| {});
        let _session = SyncSession::start(
            Some(url),
            notify,
            library.catalog_handle(),
            library.catalog_dirty(),
            &mut library,
        );

        // Adopted once, not duplicated.
        let ids: Vec<String> = library.entries().into_iter().map(|meta| meta.id).collect();
        assert_eq!(ids, vec![id]);
    }

    #[test]
    fn a_server_wipe_does_not_delete_local_pages() {
        let Some(mut server) = TestServer::start("session-wipe") else {
            return;
        };
        let url = server.url();

        let mut library = Library::open(Some(temp_dir("session-wipe-lib")));
        let notify: Notify = Arc::new(|_| {});
        let _session = SyncSession::start(
            Some(url.clone()),
            notify,
            library.catalog_handle(),
            library.catalog_dirty(),
            &mut library,
        );
        let id = library.create();
        library.save();

        {
            let probe = Library::open(Some(temp_dir("session-wipe-probe")));
            let _probe_client = connect_catalog(&url, &probe);
            assert!(
                wait_until(|| sees(&probe, &id), Duration::from_secs(5)),
                "page did not reach the server before the wipe"
            );
        }

        // Wipe the server's store and restart it. The connected client still
        // holds the catalog and must re-upload it, not treat the empty server
        // list as a deletion.
        server.wipe();

        let probe = Library::open(Some(temp_dir("session-wipe-probe2")));
        let _probe_client = connect_catalog(&url, &probe);
        assert!(
            wait_until(|| sees(&probe, &id), Duration::from_secs(10)),
            "client did not repopulate the wiped server"
        );
        assert!(sees(&library, &id), "client deleted its own page");
    }

    #[test]
    fn a_remote_page_survives_an_offline_restart() {
        let Some(server) = TestServer::start("session-persist") else {
            return;
        };
        let url = server.url();

        // A host publishes a page to the shared list and stays connected so the
        // server holds it in memory for the client below.
        let mut host = Library::open(Some(temp_dir("session-persist-host")));
        let id = host.create();
        host.save();
        let _host_client = connect_catalog(&url, &host);
        {
            let probe = Library::open(Some(temp_dir("session-persist-probe")));
            let _probe_client = connect_catalog(&url, &probe);
            assert!(
                wait_until(|| sees(&probe, &id), Duration::from_secs(5)),
                "page did not reach the server"
            );
        }

        // A client adopts the remote page, persists the catalog, then closes.
        let dir = temp_dir("session-persist-lib");
        {
            let mut library = Library::open(Some(dir.clone()));
            let notify: Notify = Arc::new(|_| {});
            let session = SyncSession::start(
                Some(url.clone()),
                notify,
                library.catalog_handle(),
                library.catalog_dirty(),
                &mut library,
            );
            assert!(sees(&library, &id), "client did not adopt the remote page");
            library.save();
            drop(session);
        }

        // Reopening with no server configured must still list the page.
        let reopened = Library::open(Some(dir.clone()));
        assert!(
            sees(&reopened, &id),
            "a remote page was not persisted for offline use"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
