use std::thread::sleep;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use serde_json::json;

use crate::doc::Doc as _;
use crate::library::Library;
use crate::session::{SyncSource, handle_frame};
use crate::test_support::TestServer;

use super::{SyncClient, is_supported_url};

#[test]
fn supported_urls_are_plain_ws_with_a_host() {
    assert!(is_supported_url("ws://127.0.0.1:1234"));
    assert!(is_supported_url("ws://example.com"));
    assert!(!is_supported_url("wss://example.com"));
    assert!(!is_supported_url("ws://"));
    assert!(!is_supported_url("ws:///sync"));
    assert!(!is_supported_url("http://example.com"));
    assert!(!is_supported_url(""));
}

fn connect(url: &str) -> (SyncClient, Receiver<Vec<u8>>, Receiver<()>) {
    let (tx, rx) = unbounded();
    let (ctx, crx) = unbounded();
    let client = SyncClient::connect(
        url,
        move |frame| {
            let _ = tx.send(frame);
        },
        move || {
            let _ = ctx.send(());
        },
    )
    .unwrap();
    (client, rx, crx)
}

fn send_hello(client: &SyncClient, library: &Library) {
    let frame =
        serde_json::to_vec(&json!({ "type": "hello", "pages": library.digests() })).unwrap();
    client.send(frame);
}

fn pump(client: &SyncClient, rx: &Receiver<Vec<u8>>, library: &mut Library) {
    while let Ok(frame) = rx.try_recv() {
        handle_frame(|reply| client.send(reply), &frame, library);
    }
}

fn text_of(library: &Library, page: &str) -> String {
    let Some(doc) = library.open_document(page) else {
        return String::new();
    };
    let Some(block) = doc.snapshot().first().map(|snapshot| snapshot.id) else {
        return String::new();
    };
    doc.text(block)
}

#[test]
fn two_clients_sync_a_page_through_the_server() {
    let Some(server) = TestServer::start("sync") else {
        return;
    };
    let url = server.url();

    let mut library_a = Library::open(None).unwrap();
    let mut library_b = Library::open(None).unwrap();
    let page = library_a.create();
    {
        let doc = library_a.open_document(&page).unwrap();
        let block = doc.snapshot().remove(0).id;
        doc.set_text(block, "hello sync");
    }

    let (client_a, rx_a, conn_a) = connect(&url);
    let (client_b, rx_b, conn_b) = connect(&url);
    conn_a.recv_timeout(Duration::from_secs(5)).unwrap();
    conn_b.recv_timeout(Duration::from_secs(5)).unwrap();

    send_hello(&client_a, &library_a);
    send_hello(&client_b, &library_b);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&client_a, &rx_a, &mut library_a);
        pump(&client_b, &rx_b, &mut library_b);
        if library_b.page_exists(&page) && text_of(&library_b, &page) == "hello sync" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "b did not receive the page from a"
        );
        sleep(Duration::from_millis(10));
    }

    // Edit on B and confirm it flows back to A.
    {
        let doc = library_b.open_document(&page).unwrap();
        let block = doc.snapshot().remove(0).id;
        doc.set_text(block, "hello sync and back");
    }
    for (page_id, records) in library_b.take_outbound() {
        let frame =
            serde_json::to_vec(&json!({ "type": "update", "page": page_id, "records": records }))
                .unwrap();
        client_b.send(frame);
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pump(&client_a, &rx_a, &mut library_a);
        pump(&client_b, &rx_b, &mut library_b);
        if text_of(&library_a, &page) == "hello sync and back" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a did not receive the edit from b"
        );
        sleep(Duration::from_millis(10));
    }
}
