use std::time::Duration;

use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, Text, Transact, Update};

use crate::test_support::{TestServer, wait_until};

use super::{SyncClient, is_supported_url};

#[test]
fn supported_urls_are_plain_ws_with_a_host() {
    assert!(is_supported_url("ws://127.0.0.1:1234"));
    assert!(is_supported_url("ws://example.com/room"));
    assert!(!is_supported_url("wss://example.com"));
    assert!(!is_supported_url("ws://"));
    assert!(!is_supported_url("ws:///room"));
    assert!(!is_supported_url("http://example.com"));
    assert!(!is_supported_url(""));
}

/// An `on_remote` callback that applies an update to `doc` as if the caller were
/// the owning UI thread. Tests have no undo manager, so applying on the worker
/// callback thread is harmless.
fn applier(doc: &Doc) -> impl Fn(Vec<u8>) + Send + Sync + 'static {
    let doc = doc.clone();
    move |bytes| {
        let Ok(update) = Update::decode_v1(&bytes) else {
            return;
        };
        let mut txn = doc.transact_mut_with("test-remote");
        let _ = txn.apply_update(update);
    }
}

#[test]
fn syncs_two_documents_through_the_server() {
    let Some(server) = TestServer::start("sync") else {
        return;
    };
    let url = server.url();

    let doc_a = Doc::new();
    let doc_b = Doc::new();
    let _a = SyncClient::connect(&url, "test-room", doc_a.clone(), applier(&doc_a), || {}).unwrap();
    let _b = SyncClient::connect(&url, "test-room", doc_b.clone(), applier(&doc_b), || {}).unwrap();

    {
        let text = doc_a.get_or_insert_text("probe");
        text.push(&mut doc_a.transact_mut(), "hello sync");
    }
    assert!(
        wait_until(
            || doc_b
                .get_or_insert_text("probe")
                .get_string(&doc_b.transact())
                == "hello sync",
            Duration::from_secs(5),
        ),
        "a -> b did not sync"
    );

    {
        let text = doc_b.get_or_insert_text("probe");
        text.push(&mut doc_b.transact_mut(), " and back");
    }
    assert!(
        wait_until(
            || doc_a
                .get_or_insert_text("probe")
                .get_string(&doc_a.transact())
                == "hello sync and back",
            Duration::from_secs(5),
        ),
        "b -> a did not sync"
    );
}

#[test]
fn reconnects_after_the_server_restarts() {
    let Some(mut server) = TestServer::start("reconnect") else {
        return;
    };
    let url = server.url();

    let doc_a = Doc::new();
    let doc_b = Doc::new();
    let _a = SyncClient::connect(
        &url,
        "reconnect-room",
        doc_a.clone(),
        applier(&doc_a),
        || {},
    )
    .unwrap();
    let _b = SyncClient::connect(
        &url,
        "reconnect-room",
        doc_b.clone(),
        applier(&doc_b),
        || {},
    )
    .unwrap();

    doc_a
        .get_or_insert_text("probe")
        .push(&mut doc_a.transact_mut(), "one");
    assert!(
        wait_until(
            || doc_b
                .get_or_insert_text("probe")
                .get_string(&doc_b.transact())
                == "one",
            Duration::from_secs(5),
        ),
        "initial sync failed"
    );

    // Take the server down and edit while disconnected.
    server.stop();
    std::thread::sleep(Duration::from_millis(300));
    doc_a
        .get_or_insert_text("probe")
        .push(&mut doc_a.transact_mut(), "two");

    // Bring a fresh server up on the same port and data directory.
    server.restart();

    assert!(
        wait_until(
            || doc_b
                .get_or_insert_text("probe")
                .get_string(&doc_b.transact())
                == "onetwo",
            Duration::from_secs(10),
        ),
        "offline edit did not resync after reconnect"
    );
}
