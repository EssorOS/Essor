//! The record store: SQLite plus the LWW merge and digests.
//!
//! Every page and block is a [`Record`] keyed by id and merged by
//! last-write-wins on its [`Clock`]. SQLite is the single source of truth;
//! there is no separate in-memory record map, so reads and writes always agree.
//!
//! The merge is one guarded upsert (`WHERE (version) > (records.version)` with
//! `RETURNING`). The same SQL is used by the server, so the conflict rule is
//! written once. A per-page SHA-256 digest over record versions lets a
//! reconnect transfer only the pages that changed.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;
use std::rc::Rc;

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::history::{Change, History, HistoryTarget};
use crate::model::{Clock, Record, RecordPayload};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS records (
  id             TEXT PRIMARY KEY,
  page           TEXT NOT NULL,
  rtype          TEXT NOT NULL,
  position       TEXT NOT NULL,
  version_c      INTEGER NOT NULL,
  version_client INTEGER NOT NULL,
  deleted        INTEGER NOT NULL,
  data           TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS records_page ON records(page);
CREATE TABLE IF NOT EXISTS pages (
  id     TEXT PRIMARY KEY,
  digest TEXT NOT NULL
) STRICT;
";

const UPSERT: &str = "
INSERT INTO records (id, page, rtype, position, version_c, version_client, deleted, data)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(id) DO UPDATE SET
  page = excluded.page,
  rtype = excluded.rtype,
  position = excluded.position,
  version_c = excluded.version_c,
  version_client = excluded.version_client,
  deleted = excluded.deleted,
  data = excluded.data
WHERE (excluded.version_c, excluded.version_client)
    > (records.version_c, records.version_client)
RETURNING id
";

/// The workspace: every page and block, persisted to SQLite.
pub struct Workspace {
    conn: Connection,
    client: u64,
    counter: Cell<u64>,
    /// Records with local (unsent) changes, for the outbound sync batch.
    dirty: RefCell<BTreeSet<String>>,
    /// Pages whose stored digest is stale and must be recomputed lazily.
    pending_digests: RefCell<BTreeSet<String>>,
    history: History,
}

impl Workspace {
    /// Open the store at `path`, or an in-memory one when `None` (tests).
    pub fn open(path: Option<PathBuf>) -> io::Result<Rc<Self>> {
        let conn = match &path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                Connection::open(path).map_err(io::Error::other)?
            }
            None => Connection::open_in_memory().map_err(io::Error::other)?,
        };
        conn.execute_batch(SCHEMA).map_err(io::Error::other)?;
        let client = load_client_id(&conn);
        let counter = conn
            .query_row(
                "SELECT COALESCE(MAX(version_c), 0) FROM records",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            .max(0) as u64;
        let workspace = Rc::new(Self {
            conn,
            client,
            counter: Cell::new(counter),
            dirty: RefCell::new(BTreeSet::new()),
            pending_digests: RefCell::new(BTreeSet::new()),
            history: History::new(),
        });
        // Rebuild digests from the records on open. A crash between a record
        // write and its digest update could otherwise leave a stale digest that
        // happens to match the server, hiding a local edit from the handshake.
        workspace.rebuild_digests();
        Ok(workspace)
    }

    /// A fresh version, one past every counter seen so far.
    pub fn next_clock(&self) -> Clock {
        let next = self.counter.get() + 1;
        self.counter.set(next);
        Clock::new(next, self.client)
    }

    /// Read one record by id.
    pub fn get(&self, id: &str) -> Option<Record> {
        self.conn
            .query_row(
                "SELECT id, page, rtype, position, version_c, version_client, deleted, data
                 FROM records WHERE id = ?1",
                params![id],
                row_to_record,
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Every page record (live), ordered for the sidebar.
    pub fn page_records(&self) -> Vec<Record> {
        self.query_records(
            "SELECT id, page, rtype, position, version_c, version_client, deleted, data
             FROM records WHERE rtype = 'page' AND deleted = 0
             ORDER BY position, id",
            params![],
        )
    }

    /// A page's live blocks, in order.
    pub fn block_records(&self, page: &str) -> Vec<Record> {
        self.query_records(
            "SELECT id, page, rtype, position, version_c, version_client, deleted, data
             FROM records WHERE page = ?1 AND rtype = 'block' AND deleted = 0
             ORDER BY position, id",
            params![page],
        )
    }

    /// Every record belonging to a page, tombstones included (for sync/persist).
    pub fn records_for_page(&self, page: &str) -> Vec<Record> {
        self.query_records(
            "SELECT id, page, rtype, position, version_c, version_client, deleted, data
             FROM records WHERE page = ?1 ORDER BY rtype, position, id",
            params![page],
        )
    }

    pub fn page_exists(&self, id: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM records WHERE id = ?1 AND rtype = 'page' AND deleted = 0",
                params![id],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Merge remote records, returning the ones that actually changed.
    ///
    /// Every incoming counter raises the local clock so later local edits always
    /// outrank what we have already seen.
    pub fn merge(&self, records: Vec<Record>) -> Vec<Record> {
        let mut applied = Vec::new();
        let mut touched = BTreeSet::new();
        let mut max_seen = self.counter.get();
        for record in records {
            max_seen = max_seen.max(record.version.c);
            if self.upsert(&record) {
                touched.insert(record.page.clone());
                applied.push(record);
            }
        }
        if max_seen > self.counter.get() {
            self.counter.set(max_seen);
        }
        self.mark_digests_pending(touched);
        applied
    }

    /// Write a locally-authored record: upsert, mark it dirty, and mark its page
    /// digest stale.
    pub fn write_local(&self, record: &Record) {
        self.upsert(record);
        self.dirty.borrow_mut().insert(record.id.clone());
        self.mark_digest_pending(&record.page);
    }

    /// Write a record without marking it dirty. Used for provisional pages that
    /// should not be sent until confirmed.
    pub fn write_local_quiet(&self, record: &Record) {
        self.upsert(record);
        self.mark_digest_pending(&record.page);
    }

    /// Permanently remove every record of a page, its digest, and its undo
    /// history. Only safe for a page that was never synced (e.g. a discarded
    /// provisional page), since peers would never learn of a hard delete.
    pub fn purge_page(&self, page: &str) {
        let ids: Vec<String> = self
            .records_for_page(page)
            .into_iter()
            .map(|record| record.id)
            .collect();
        let _ = self
            .conn
            .execute("DELETE FROM records WHERE page = ?1", params![page]);
        let _ = self
            .conn
            .execute("DELETE FROM pages WHERE id = ?1", params![page]);
        self.dirty.borrow_mut().retain(|id| !ids.contains(id));
        self.pending_digests.borrow_mut().remove(page);
        self.history.forget(page);
    }

    /// Mark every record of `page` as locally changed so the next outbound flush
    /// sends it. Used when a provisional page is admitted to sync.
    pub fn mark_page_dirty(&self, page: &str) {
        let ids: Vec<String> = self
            .records_for_page(page)
            .into_iter()
            .map(|record| record.id)
            .collect();
        self.dirty.borrow_mut().extend(ids);
    }

    fn upsert(&self, record: &Record) -> bool {
        let data = serde_json::to_string(&record.payload).unwrap_or_else(|_| "{}".to_string());
        let mut stmt = self.conn.prepare_cached(UPSERT).expect("prepare upsert");
        let mut rows = stmt
            .query(params![
                record.id,
                record.page,
                record.payload.kind_name(),
                record.position,
                record.version.c as i64,
                record.version.client as i64,
                record.deleted as i64,
                data,
            ])
            .expect("run upsert");
        matches!(rows.next(), Ok(Some(_)))
    }

    fn query_records(&self, sql: &str, params: impl rusqlite::Params) -> Vec<Record> {
        let mut stmt = match self.conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params, row_to_record) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(Result::ok).collect()
    }

    /// Mark a page's stored digest stale; it is recomputed on the next
    /// [`Self::digests`] call rather than on every keystroke.
    fn mark_digest_pending(&self, page: &str) {
        self.pending_digests.borrow_mut().insert(page.to_string());
    }

    fn mark_digests_pending<I: IntoIterator<Item = String>>(&self, pages: I) {
        self.pending_digests.borrow_mut().extend(pages);
    }

    /// Recompute and store digests for every page whose records changed.
    fn flush_digests(&self) {
        if self.pending_digests.borrow().is_empty() {
            return;
        }
        let pages: Vec<String> = std::mem::take(&mut *self.pending_digests.borrow_mut())
            .into_iter()
            .collect();
        for page in pages {
            let digest = self.compute_digest(&page);
            let _ = self.conn.execute(
                "INSERT INTO pages (id, digest) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET digest = excluded.digest",
                params![page, digest],
            );
        }
    }

    /// Drop and recompute every stored digest from the records. Runs once on
    /// open so a crash between a write and its digest update cannot leave a stale
    /// digest that hides a local edit from the handshake.
    fn rebuild_digests(&self) {
        let _ = self.conn.execute("DELETE FROM pages", []);
        for page in self.distinct_pages() {
            let digest = self.compute_digest(&page);
            let _ = self.conn.execute(
                "INSERT INTO pages (id, digest) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET digest = excluded.digest",
                params![page, digest],
            );
        }
    }

    /// Every distinct page id that has at least one record.
    fn distinct_pages(&self) -> Vec<String> {
        let mut stmt = match self
            .conn
            .prepare("SELECT DISTINCT page FROM records ORDER BY page")
        {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    fn compute_digest(&self, page: &str) -> String {
        let mut hasher = Sha256::new();
        let mut stmt = match self.conn.prepare(
            "SELECT id, version_c, version_client, deleted FROM records
             WHERE page = ?1 ORDER BY id",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return String::new(),
        };
        let rows = stmt.query_map(params![page], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        });
        if let Ok(rows) = rows {
            for row in rows.flatten() {
                let (id, c, client, deleted) = row;
                hasher.update((id.len() as u32).to_be_bytes());
                hasher.update(id.as_bytes());
                hasher.update((c as u64).to_be_bytes());
                hasher.update((client as u64).to_be_bytes());
                hasher.update([deleted as u8]);
            }
        }
        format!("{:x}", hasher.finalize())
    }

    /// Per-page digests, for the reconnect handshake. Flushes any stale digests
    /// first so the handshake always sees the current record versions.
    pub fn digests(&self) -> BTreeMap<String, String> {
        self.flush_digests();
        let mut stmt = match self.conn.prepare("SELECT id, digest FROM pages") {
            Ok(stmt) => stmt,
            Err(_) => return BTreeMap::new(),
        };
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        });
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => BTreeMap::new(),
        }
    }

    /// Take the locally-changed records, grouped by page and clearing the dirty
    /// set. Only the records that actually changed are returned.
    pub fn take_outbound(&self) -> BTreeMap<String, Vec<Record>> {
        let ids: Vec<String> = self.dirty.borrow_mut().iter().cloned().collect();
        self.dirty.borrow_mut().clear();
        let mut outbound: BTreeMap<String, Vec<Record>> = BTreeMap::new();
        for id in ids {
            if let Some(record) = self.get(&id) {
                outbound
                    .entry(record.page.clone())
                    .or_default()
                    .push(record);
            }
        }
        outbound
    }

    // --- undo / redo ------------------------------------------------------

    /// Record a locally-applied edit. Consecutive edits sharing `coalesce` fold
    /// into one undo step; see [`crate::history::History::push`].
    pub fn push_undo(&self, page: &str, changes: Vec<Change>, coalesce: Option<String>) {
        self.history.push(page, changes, coalesce);
    }

    pub fn undo(&self, page: &str) -> bool {
        self.history.undo(self, page)
    }

    pub fn redo(&self, page: &str) -> bool {
        self.history.redo(self, page)
    }
}

impl HistoryTarget for Workspace {
    fn get(&self, id: &str) -> Option<Record> {
        Workspace::get(self, id)
    }

    fn write_local(&self, record: &Record) {
        Workspace::write_local(self, record);
    }

    fn next_clock(&self) -> Clock {
        Workspace::next_clock(self)
    }

    fn client_id(&self) -> u64 {
        self.client
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    let id: String = row.get(0)?;
    let page: String = row.get(1)?;
    let _rtype: String = row.get(2)?;
    let position: String = row.get(3)?;
    let version = Clock::new(
        row.get::<_, i64>(4)?.max(0) as u64,
        row.get::<_, i64>(5)?.max(0) as u64,
    );
    let deleted = row.get::<_, i64>(6)? != 0;
    let data: String = row.get(7)?;
    let payload: RecordPayload = serde_json::from_str(&data).unwrap_or(RecordPayload::Block {
        kind: crate::model::BlockKind::Paragraph,
        runs: Vec::new(),
    });
    Ok(Record {
        id,
        page,
        position,
        version,
        deleted,
        payload,
    })
}

/// Load a persisted 53-bit client id, creating one on first run.
fn load_client_id(conn: &Connection) -> u64 {
    // Ensure the schema marker exists even when the id was written by a build
    // that only stored the client id.
    let _ = conn.execute(
        "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1')",
        [],
    );
    if let Ok(value) = conn.query_row(
        "SELECT value FROM meta WHERE key = 'client_id'",
        [],
        |row| row.get::<_, String>(0),
    ) && let Ok(id) = value.parse::<u64>()
    {
        return id;
    }
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let raw = u64::from_le_bytes(bytes[..8].try_into().expect("uuid slice"));
    // 53 bits, so the value stays exact in JavaScript on the server side.
    let id = raw & 0x001f_ffff_ffff_ffff;
    let id = if id == 0 { 1 } else { id };
    let _ = conn.execute(
        "INSERT INTO meta (key, value) VALUES ('client_id', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![id.to_string()],
    );
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;
    use crate::model::{BlockKind, Record, RecordPayload, TextRun};
    use crate::page::PageHandle;

    fn record_text(record: &Record) -> String {
        match &record.payload {
            RecordPayload::Block { runs, .. } => runs.iter().map(|r| r.text.as_str()).collect(),
            RecordPayload::Page { title } => title.clone(),
        }
    }

    fn block(id: &str, c: u64, client: u64, text: &str, deleted: bool) -> Record {
        Record {
            id: id.to_string(),
            page: "p".to_string(),
            position: "80".to_string(),
            version: Clock::new(c, client),
            deleted,
            payload: RecordPayload::Block {
                kind: BlockKind::Paragraph,
                runs: if text.is_empty() {
                    Vec::new()
                } else {
                    vec![TextRun {
                        text: text.to_string(),
                        bold: false,
                        italic: false,
                    }]
                },
            },
        }
    }

    #[test]
    fn digest_matches_the_canonical_encoding() {
        let ws = Workspace::open(None).unwrap();
        ws.merge(vec![
            block("1111", 1, 2, "x", false),
            block("2222", 5, 2, "", true),
        ]);
        assert_eq!(
            ws.compute_digest("p"),
            "93630e7bcd59bbdae071559336c082766caf3feb9dfad1eb384d637ca7f8ba4c"
        );
    }

    #[test]
    fn merge_is_last_write_wins() {
        let ws = Workspace::open(None).unwrap();
        assert_eq!(ws.merge(vec![block("b1", 1, 1, "one", false)]).len(), 1);
        // An older version is ignored.
        assert!(ws.merge(vec![block("b1", 0, 1, "zero", false)]).is_empty());
        assert_eq!(record_text(&ws.get("b1").unwrap()), "one");
        // A newer version wins.
        assert_eq!(ws.merge(vec![block("b1", 2, 1, "two", false)]).len(), 1);
        assert_eq!(record_text(&ws.get("b1").unwrap()), "two");
        // The counter rose to the highest version seen: the next local write is 3.
        assert_eq!(ws.next_clock().c, 3);
    }

    #[test]
    fn tombstones_hide_and_cannot_be_resurrected_by_older_edits() {
        let ws = Workspace::open(None).unwrap();
        ws.merge(vec![block("b1", 1, 1, "live", false)]);
        ws.merge(vec![block("b1", 2, 1, "", true)]);
        assert!(ws.block_records("p").is_empty());
        // A stale edit with a lower version loses to the tombstone.
        assert!(ws.merge(vec![block("b1", 1, 2, "stale", false)]).is_empty());
        assert!(ws.block_records("p").is_empty());
    }

    #[test]
    fn outbound_carries_only_the_changed_records() {
        let ws = Workspace::open(None).unwrap();
        let handle = PageHandle::new(ws.clone(), "p".to_string());
        let a = handle.insert_block(0);
        handle.insert_block(1);
        // Both creations are dirty.
        let first = ws.take_outbound();
        assert_eq!(first.get("p").map(Vec::len), Some(2));
        assert!(ws.take_outbound().is_empty(), "the dirty set was cleared");

        // A single edit sends only that block, not the whole page.
        handle.set_text(a, "hello");
        let second = ws.take_outbound();
        let records = second.get("p").unwrap();
        assert_eq!(records.len(), 1, "only the edited block is sent");
        assert_eq!(records[0].id, a.to_simple());
    }

    #[test]
    fn purge_page_removes_records_digest_and_undo() {
        let ws = Workspace::open(None).unwrap();
        let handle = PageHandle::new(ws.clone(), "p".to_string());
        let id = handle.insert_block(0);
        handle.set_text(id, "hello");
        assert!(ws.digests().contains_key("p"));

        ws.purge_page("p");
        assert!(ws.records_for_page("p").is_empty());
        assert!(!ws.digests().contains_key("p"));
        // Undo history is gone, so undo is a no-op rather than a resurrection.
        assert!(!ws.undo("p"));
    }

    #[test]
    fn digests_are_rebuilt_on_open() {
        let dir = std::env::temp_dir().join(format!("essor-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("essor.db");

        let expected;
        {
            let ws = Workspace::open(Some(db.clone())).unwrap();
            let handle = PageHandle::new(ws.clone(), "p".to_string());
            let id = handle.insert_block(0);
            handle.set_text(id, "hello");
            expected = ws.digests()["p"].clone();
        }

        // Corrupt the stored digest, then reopen: it must be recomputed from the
        // records rather than trusted.
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE pages SET digest = 'stale' WHERE id = 'p'", [])
                .unwrap();
        }
        let reopened = Workspace::open(Some(db)).unwrap();
        assert_eq!(reopened.digests()["p"], expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_and_redo_round_trip_an_edit_and_a_creation() {
        let ws = Workspace::open(None).unwrap();
        let mut handle = PageHandle::new(ws, "p".to_string());
        let id = handle.insert_block(0);
        handle.set_text(id, "hello");
        assert_eq!(handle.text(id), "hello");
        assert!(handle.undo());
        assert_eq!(handle.len(), 1);
        assert_eq!(handle.text(id), "");
        assert!(handle.undo());
        assert_eq!(handle.len(), 0, "undoing the creation removes the block");
        assert!(handle.redo());
        assert_eq!(handle.len(), 1);
        assert!(handle.redo());
        assert_eq!(handle.text(id), "hello");
    }

    #[test]
    fn undo_skips_a_step_a_peer_overwrote() {
        let ws = Workspace::open(None).unwrap();
        let mut handle = PageHandle::new(ws.clone(), "p".to_string());
        let a = handle.insert_block(0);
        handle.set_text(a, "a");
        let b = handle.insert_block(1);
        handle.set_text(b, "b");

        // A peer takes over b with a newer version.
        let mut peer = ws.get(&b.to_simple()).unwrap();
        peer.version = Clock::new(999, 999);
        peer.payload = RecordPayload::Block {
            kind: BlockKind::Paragraph,
            runs: vec![TextRun {
                text: "peer".to_string(),
                bold: false,
                italic: false,
            }],
        };
        ws.merge(vec![peer]);

        // b's edit and creation are now peer-owned, so undo skips both and
        // reverts the earlier edit to a instead of silently eating the step.
        assert!(handle.undo());
        assert_eq!(handle.text(a), "");
        assert_eq!(handle.text(b), "peer");
    }
}
