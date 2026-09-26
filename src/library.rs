//! The document library: the set of pages and their order.
//!
//! The list itself is a `yrs::Doc` (the *catalog*), not a plain file. It holds
//! two root maps keyed by page id — `titles` (existence) and `positions`
//! (fractional-index order) — and is persisted locally as a `.ydoc` update and,
//! when a sync URL is configured, connected to the shared `library` room. Being
//! a CRDT means additions, renames and removals merge automatically across
//! clients and are carried as real tombstones, so a peer with the state can
//! always restore it.
//!
//! Page *content* stays one `yrs::Doc` per page, persisted under
//! `documents/<id>.ydoc`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fractional_index::FractionalIndex;
use yrs::updates::decoder::Decode;
use yrs::{Doc as YrsDoc, Map, MapRef, ReadTxn, StateVector, Transact, Update};

use crate::doc::YrsDocument;

/// Longest title shown in the sidebar (in characters), before truncation.
const TITLE_MAX: usize = 40;

/// Local file holding the catalog's CRDT state.
const CATALOG_FILE: &str = "library.ydoc";
/// Root map of `id -> title`; key presence is what makes a page exist.
const TITLES: &str = "titles";
/// Root map of `id -> fractional index` that orders the pages.
const POSITIONS: &str = "positions";

/// Metadata for one page in the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentMeta {
    pub id: String,
    pub title: String,
}

/// A collection of pages persisted under a data directory:
///
/// ```text
/// <dir>/library.ydoc          the synced catalog (list, titles, order)
/// <dir>/documents/<id>.ydoc   one Yrs update file per page
/// ```
///
/// A `None` directory yields an in-memory library, which keeps tests and
/// embedding trivial.
pub struct Library {
    dir: Option<PathBuf>,
    doc: YrsDoc,
    titles: MapRef,
    positions: MapRef,
    /// The page currently shown, by id. Local UI state; never synced.
    active: Option<String>,
    /// The catalog changed and should be written to disk. Shared with the sync
    /// worker so a remote update marks it dirty too.
    dirty: Arc<AtomicBool>,
    /// Every page id this catalog has listed in this process. Orphan files
    /// outside this set are left alone, so a stale catalog can't delete a file
    /// for a page it has simply never loaded.
    ///
    /// Being per-process, this also means a page deleted by a peer while this
    /// client was offline has its file retained: the id is never listed here, so
    /// `sync_files` won't reclaim it. That is the safe choice — the alternative
    /// would let a partial or rolled-back catalog delete live pages — but it
    /// does mean such files can accumulate until the page is listed again (at
    /// which point a later deletion reclaims it) or removed by hand.
    ever_seen: RefCell<HashSet<String>>,
}

impl Library {
    /// Open (or initialise) the library in `dir`, loading the catalog from disk
    /// if present. Seeding a first page, if the catalog is empty, is left to the
    /// caller so a client joining an existing shared library doesn't invent one.
    pub fn open(dir: Option<PathBuf>) -> Self {
        let doc = YrsDoc::new();
        let titles = doc.get_or_insert_map(TITLES);
        let positions = doc.get_or_insert_map(POSITIONS);
        let mut library = Self {
            dir,
            doc,
            titles,
            positions,
            active: None,
            dirty: Arc::new(AtomicBool::new(false)),
            ever_seen: RefCell::new(HashSet::new()),
        };
        library.load();
        library
    }

    fn load(&mut self) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let _ = std::fs::create_dir_all(dir.join("documents"));
        if let Ok(bytes) = std::fs::read(dir.join(CATALOG_FILE))
            && let Ok(update) = Update::decode_v1(&bytes)
        {
            let mut txn = self.doc.transact_mut();
            let _ = txn.apply_update(update);
        }
        self.active = self.entries().first().map(|meta| meta.id.clone());
    }

    /// A second handle to the catalog, for the sync layer to share with its
    /// worker. Cheap to clone and safe to move across threads.
    pub fn catalog_handle(&self) -> YrsDoc {
        self.doc.clone()
    }

    /// A shared handle to the catalog's dirty flag. The sync worker sets it when
    /// it applies a remote update, so the change is persisted like a local one.
    pub fn catalog_dirty(&self) -> Arc<AtomicBool> {
        self.dirty.clone()
    }

    /// The ordered pages shown in the sidebar.
    pub fn entries(&self) -> Vec<DocumentMeta> {
        let txn = self.doc.transact();
        let mut ordered: Vec<(String, DocumentMeta)> = self
            .titles
            .iter(&txn)
            .filter_map(|(id, value)| {
                let title = match value.cast::<String>() {
                    Ok(title) if !title.trim().is_empty() => title,
                    Ok(_) => "Untitled".to_string(),
                    Err(_) => return None,
                };
                let position = self
                    .positions
                    .get(&txn, id)
                    .and_then(|value| value.cast::<String>().ok())
                    .unwrap_or_else(|| id.to_string());
                let meta = DocumentMeta {
                    id: id.to_string(),
                    title,
                };
                Some((position, meta))
            })
            .collect();
        ordered.sort_by(|(a_position, a), (b_position, b)| {
            a_position.cmp(b_position).then_with(|| a.id.cmp(&b.id))
        });
        self.ever_seen
            .borrow_mut()
            .extend(ordered.iter().map(|(_, meta)| meta.id.clone()));
        ordered.into_iter().map(|(_, meta)| meta).collect()
    }

    /// The id of the currently active page, if any. Returns `None` when the
    /// active page no longer exists (for example it was removed by a peer).
    pub fn active_id(&self) -> Option<String> {
        let id = self.active.clone()?;
        self.contains(&id).then_some(id)
    }

    /// The active page's index in [`Self::entries`], or `0` when there is none.
    pub fn active_index(&self) -> usize {
        let Some(id) = self.active_id() else {
            return 0;
        };
        self.entries()
            .iter()
            .position(|meta| meta.id == id)
            .unwrap_or(0)
    }

    /// Make the page with `id` active. Returns `false` when it is unknown.
    pub fn set_active_id(&mut self, id: &str) -> bool {
        if self.contains(id) {
            self.active = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// Create a new page, seed its content with one empty block, make it active,
    /// and return its id.
    pub fn create(&mut self) -> String {
        let id = self.unique_id();
        {
            let mut txn = self.doc.transact_mut();
            self.titles.insert(&mut txn, id.as_str(), "Untitled");
            let position = next_position(&self.positions, &txn);
            self.positions.insert(&mut txn, id.as_str(), position);
        }
        if let Some(path) = self.doc_path(&id) {
            let doc = YrsDocument::open(Some(path));
            let _ = doc.save_now();
        }
        self.active = Some(id.clone());
        self.dirty.store(true, Ordering::Relaxed);
        id
    }

    /// Delete the page with `id`, removing its file. Refused when it is the last
    /// page or unknown.
    pub fn delete(&mut self, id: &str) -> bool {
        if self.entries().len() <= 1 || !self.contains(id) {
            return false;
        }
        {
            let mut txn = self.doc.transact_mut();
            self.titles.remove(&mut txn, id);
            self.positions.remove(&mut txn, id);
        }
        if let Some(path) = self.doc_path(id) {
            let _ = std::fs::remove_file(path);
        }
        if self.active.as_deref() == Some(id) {
            self.active = self.entries().first().map(|meta| meta.id.clone());
        }
        self.dirty.store(true, Ordering::Relaxed);
        true
    }

    /// Cache a new title for `id`. A no-op when it already matches, so
    /// re-deriving an unchanged title doesn't churn the CRDT.
    pub fn set_title(&mut self, id: &str, title: &str) {
        let mut txn = self.doc.transact_mut();
        let current = self
            .titles
            .get(&txn, id)
            .and_then(|value| value.cast::<String>().ok());
        if current.as_deref() != Some(title) {
            self.titles.insert(&mut txn, id, title);
            drop(txn);
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    /// Load the page with `id` as a fresh in-memory handle, or `None` when it is
    /// unknown. Call [`Self::sync_files`] first so a synced page has a file.
    pub fn open_document(&self, id: &str) -> Option<YrsDocument> {
        self.doc_path(id).map(|path| YrsDocument::open(Some(path)))
    }

    /// Write pending catalog changes (if any) to disk.
    ///
    /// The flag is cleared up front with `swap`, so a remote update that lands
    /// on the sync worker while we are encoding — or writing — re-arms it and is
    /// persisted by a later `save` rather than being dropped. A failed write
    /// likewise re-arms it so the change is retried.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        if let Some(dir) = &self.dir {
            let txn = self.doc.transact();
            let update = txn.encode_state_as_update_v1(&StateVector::default());
            drop(txn);
            if crate::fs::atomic_write(&dir.join(CATALOG_FILE), &update).is_err() {
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Mirror the catalog onto the page files: materialise an empty file for
    /// every live page and delete the file for a page this catalog used to list
    /// but no longer does. Only ids this catalog has actually seen are eligible,
    /// so a stale catalog can never delete a file for a page it never loaded.
    ///
    /// A page deleted by a peer while this client was offline is the known gap:
    /// its id was never listed here, so its file is left behind (see
    /// [`Self::ever_seen`]). It is reclaimed once the page is listed and later
    /// deleted, or can be removed manually.
    pub fn sync_files(&self) {
        let Some(docs_dir) = self.docs_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&docs_dir);
        let live: HashSet<String> = self.entries().into_iter().map(|meta| meta.id).collect();

        for id in &live {
            let path = docs_dir.join(format!("{id}.ydoc"));
            if !path.exists() {
                let _ = YrsDocument::create_empty(&path);
            }
        }

        let Ok(read) = std::fs::read_dir(&docs_dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("ydoc") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !live.contains(stem) && self.ever_seen.borrow().contains(stem) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    fn contains(&self, id: &str) -> bool {
        let txn = self.doc.transact();
        self.titles.get(&txn, id).is_some()
    }

    /// A globally-unique id that does not collide with an existing file.
    ///
    /// UUIDv4, so two clients creating a page at the same moment never produce
    /// the same id. The existence check is a cheap belt-and-braces guard; a
    /// collision is astronomically unlikely.
    fn unique_id(&self) -> String {
        loop {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let exists = self.doc_path(&id).is_some_and(|path| path.exists());
            if !exists {
                return id;
            }
        }
    }

    fn docs_dir(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join("documents"))
    }

    fn doc_path(&self, id: &str) -> Option<PathBuf> {
        self.docs_dir().map(|dir| dir.join(format!("{id}.ydoc")))
    }
}

/// A fractional index that sorts after every existing position in `positions`.
/// The string form is lexicographically ordered, so appends keep insertion
/// order even when clients publish concurrently. Unparsable values are ignored,
/// so a corrupt entry can't poison ordering.
fn next_position<T: ReadTxn>(positions: &MapRef, txn: &T) -> String {
    let max = positions
        .iter(txn)
        .filter_map(|(_, value)| value.cast::<String>().ok())
        .filter_map(|value| FractionalIndex::from_string(&value).ok())
        .max();
    match max {
        Some(index) => FractionalIndex::new_after(&index).to_string(),
        None => FractionalIndex::default().to_string(),
    }
}

/// A sidebar title derived from a document's first non-empty line.
pub fn derive_title(doc: &dyn crate::doc::Doc) -> String {
    first_title(doc.snapshot().into_iter().map(|snapshot| {
        snapshot
            .runs
            .iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
    }))
}

/// The first non-empty title among `texts`, trimmed and truncated, or
/// `"Untitled"` when none qualify.
pub fn first_title<S: AsRef<str>>(texts: impl IntoIterator<Item = S>) -> String {
    texts
        .into_iter()
        .find_map(|text| title_from_text(text.as_ref()))
        .unwrap_or_else(|| "Untitled".to_string())
}

/// The first non-empty line of `text`, trimmed and truncated, if any.
pub fn title_from_text(text: &str) -> Option<String> {
    let line = text.lines().next().unwrap_or("").trim();
    (!line.is_empty()).then(|| truncate_title(line))
}

/// Trim a line to [`TITLE_MAX`] characters, adding an ellipsis when cut.
pub fn truncate_title(text: &str) -> String {
    let mut chars = text.chars();
    let truncated: String = chars.by_ref().take(TITLE_MAX).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("essor-lib-{name}-{}", std::process::id()))
    }

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = temp_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn starts_empty() {
        let library = Library::open(None);
        assert!(library.entries().is_empty());
        assert_eq!(library.active_id(), None);
    }

    #[test]
    fn documents_get_unique_uuid_ids() {
        let dir = fresh_dir("uuid");
        let mut library = Library::open(Some(dir.clone()));
        let a = library.create();
        let b = library.create();

        let ids: Vec<String> = library.entries().into_iter().map(|meta| meta.id).collect();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 2);
        assert_ne!(a, b);
        for id in &ids {
            // UUIDv4 in simple form: 32 lowercase hex characters.
            assert_eq!(id.len(), 32, "unexpected id length: {id}");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "not a simple UUID: {id}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_and_delete_roundtrip() {
        let dir = fresh_dir("crud");
        let mut library = Library::open(Some(dir.clone()));
        let first = library.create();
        let second = library.create();
        assert_eq!(library.entries().len(), 2);
        assert_eq!(library.active_id().as_deref(), Some(second.as_str()));
        library.save();

        assert!(library.delete(&second));
        assert_eq!(library.entries().len(), 1);
        assert_eq!(library.active_id().as_deref(), Some(first.as_str()));
        library.save();

        // Reopening sees the same single page.
        let reopened = Library::open(Some(dir.clone()));
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].id, first);
        assert_eq!(reopened.active_id().as_deref(), Some(first.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_id_ignores_a_page_removed_from_the_catalog() {
        let dir = fresh_dir("active-gone");
        let mut library = Library::open(Some(dir.clone()));
        let _a = library.create();
        let b = library.create();
        library.set_active_id(&b);

        // Simulate a peer deleting the active page directly in the catalog,
        // bypassing `Library::delete`'s local bookkeeping.
        let doc = library.catalog_handle();
        let titles = doc.get_or_insert_map(TITLES);
        let positions = doc.get_or_insert_map(POSITIONS);
        let mut txn = doc.transact_mut();
        titles.remove(&mut txn, b.as_str());
        positions.remove(&mut txn, b.as_str());
        drop(txn);

        assert_eq!(library.active_id(), None);
        assert_eq!(library.active_index(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_delete_last_document() {
        let dir = fresh_dir("last");
        let mut library = Library::open(Some(dir.clone()));
        let only = library.create();
        assert!(!library.delete(&only));
        assert_eq!(library.entries().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalogs_order_is_stable() {
        let dir = fresh_dir("order");
        let mut library = Library::open(Some(dir.clone()));
        let a = library.create();
        let b = library.create();
        let c = library.create();
        let ids: Vec<String> = library.entries().into_iter().map(|meta| meta.id).collect();
        assert_eq!(ids, vec![a, b, c]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_are_cached_in_the_catalog() {
        let dir = fresh_dir("titles");
        let mut library = Library::open(Some(dir.clone()));
        let id = library.create();
        library.set_title(&id, "Hello world");
        library.save();

        let reopened = Library::open(Some(dir.clone()));
        assert_eq!(reopened.entries()[0].title, "Hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_files_materialises_live_pages_and_removes_deleted_ones() {
        let dir = fresh_dir("syncfiles");
        let mut library = Library::open(Some(dir.clone()));
        let id = library.create();
        library.save();
        library.sync_files();
        let path = dir.join(format!("documents/{id}.ydoc"));
        assert!(path.exists());

        // Simulate a peer deleting the page directly in the catalog, leaving the
        // file behind.
        let doc = library.catalog_handle();
        let titles = doc.get_or_insert_map(TITLES);
        let positions = doc.get_or_insert_map(POSITIONS);
        let mut txn = doc.transact_mut();
        titles.remove(&mut txn, id.as_str());
        positions.remove(&mut txn, id.as_str());
        drop(txn);

        library.sync_files();
        assert!(!path.exists(), "a deleted page's file must be removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_files_preserves_files_never_listed_by_the_catalog() {
        let dir = fresh_dir("syncfiles-stale");
        let library = Library::open(Some(dir.clone()));
        // A peer's page file whose catalog entry never loaded (stale catalog).
        let stray = dir.join("documents/peerpage.ydoc");
        std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
        std::fs::write(&stray, b"junk").unwrap();

        library.sync_files();
        assert!(stray.exists(), "an unseen file must not be deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remotely_dirtied_catalog_is_persisted() {
        let dir = fresh_dir("remote-dirty");
        let library = Library::open(Some(dir.clone()));
        let dirty = library.catalog_dirty();

        // Apply a remote-style update, exactly as the sync worker does, then
        // mark the catalog dirty through the shared flag.
        let peer = YrsDoc::new();
        {
            let titles = peer.get_or_insert_map(TITLES);
            let positions = peer.get_or_insert_map(POSITIONS);
            let mut txn = peer.transact_mut();
            titles.insert(&mut txn, "remote-page", "Remote");
            positions.insert(&mut txn, "remote-page", "a0");
        }
        let update = peer
            .transact()
            .encode_state_as_update_v1(&StateVector::default());
        let doc = library.catalog_handle();
        let mut txn = doc.transact_mut();
        let _ = txn.apply_update(Update::decode_v1(&update).unwrap());
        drop(txn);
        dirty.store(true, Ordering::Relaxed);

        library.save();

        let reopened = Library::open(Some(dir.clone()));
        assert_eq!(reopened.entries().len(), 1);
        assert_eq!(reopened.entries()[0].id, "remote-page");
        assert_eq!(reopened.entries()[0].title, "Remote");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncates_long_titles() {
        let title = truncate_title(&"a".repeat(TITLE_MAX + 5));
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_MAX + 1);
    }
}
