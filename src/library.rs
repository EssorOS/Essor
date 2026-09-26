//! The document library: the set of pages, their titles and order.
//!
//! All pages live in one [`Workspace`]; the library is the UI-facing view over
//! it (active page, catalog operations) and implements [`SyncSource`] with the
//! primitives the session uses (digests, page records, outbound batches).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use fractional_index::FractionalIndex;

use crate::model::{BlockId, BlockKind, Record, RecordPayload};
use crate::page::PageHandle;
use crate::session::SyncSource;
use crate::workspace::Workspace;

/// Metadata for one page in the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentMeta {
    pub id: String,
    pub title: String,
}

/// A collection of pages, backed by one SQLite workspace. A `None` directory
/// yields an in-memory library, which keeps tests and embedding trivial.
pub struct Library {
    workspace: Rc<Workspace>,
    /// The page currently shown, by id. Local UI state; never synced.
    active: Option<String>,
    /// A page seeded locally before the first handshake. It is withheld from the
    /// sync digests until the handshake shows the server has no pages, so a
    /// fresh client doesn't push an empty page onto a populated library.
    provisional: Option<String>,
}

impl Library {
    /// Open (or initialise) the library. Seeding a first page, if the store is
    /// empty, is left to the caller so a client joining an existing shared
    /// library doesn't invent one.
    pub fn open(dir: Option<PathBuf>) -> std::io::Result<Self> {
        let path = dir.map(|dir| dir.join("essor.db"));
        let workspace = Workspace::open(path)?;
        let mut library = Self {
            workspace,
            active: None,
            provisional: None,
        };
        library.active = library.entries().first().map(|meta| meta.id.clone());
        Ok(library)
    }

    #[cfg(test)]
    pub fn workspace(&self) -> Rc<Workspace> {
        self.workspace.clone()
    }

    /// The ordered pages shown in the sidebar.
    pub fn entries(&self) -> Vec<DocumentMeta> {
        self.workspace
            .page_records()
            .into_iter()
            .map(|record| {
                let title = match &record.payload {
                    RecordPayload::Page { title } if !title.trim().is_empty() => title.clone(),
                    _ => "Untitled".to_string(),
                };
                DocumentMeta {
                    id: record.id,
                    title,
                }
            })
            .collect()
    }

    /// The id of the currently active page, if any. Returns `None` when the
    /// active page no longer exists (for example it was removed by a peer).
    pub fn active_id(&self) -> Option<String> {
        let id = self.active.clone()?;
        self.workspace.page_exists(&id).then_some(id)
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
        if self.workspace.page_exists(id) {
            self.active = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// Create a new page, seed its content with one empty block, make it active,
    /// and return its id.
    pub fn create(&mut self) -> String {
        self.create_page(false)
    }

    /// Like [`Self::create`], but withhold the page from sync until the first
    /// handshake confirms the server has no pages. Used to seed a first page
    /// before the window is built.
    pub fn create_provisional(&mut self) -> String {
        let id = self.create_page(true);
        self.provisional = Some(id.clone());
        id
    }

    fn create_page(&mut self, quiet: bool) -> String {
        let id = BlockId::new().to_simple();
        let position = next_page_position(&self.workspace);
        let page = Record {
            id: id.clone(),
            page: id.clone(),
            position,
            version: self.workspace.next_clock(),
            deleted: false,
            payload: RecordPayload::Page {
                title: "Untitled".to_string(),
            },
        };
        self.write(&page, quiet);

        let block = Record {
            id: BlockId::new().to_simple(),
            page: id.clone(),
            position: FractionalIndex::default().to_string(),
            version: self.workspace.next_clock(),
            deleted: false,
            payload: RecordPayload::Block {
                kind: BlockKind::Paragraph,
                runs: Vec::new(),
            },
        };
        self.write(&block, quiet);

        self.active = Some(id.clone());
        id
    }

    fn write(&self, record: &Record, quiet: bool) {
        if quiet {
            self.workspace.write_local_quiet(record);
        } else {
            self.workspace.write_local(record);
        }
    }

    /// Delete the page with `id` by tombstoning its page record. Refused when it
    /// is the last page or unknown. Block records are left inert; see the plan.
    pub fn delete(&mut self, id: &str) -> bool {
        if self.entries().len() <= 1 || !self.workspace.page_exists(id) {
            return false;
        }
        if let Some(page) = self.workspace.get(id) {
            let mut tombstone = page.tombstone();
            tombstone.version = self.workspace.next_clock();
            self.workspace.write_local(&tombstone);
        }
        if self.active.as_deref() == Some(id) {
            self.active = self.entries().first().map(|meta| meta.id.clone());
        }
        true
    }

    /// Cache a new title for `id`. A no-op when it already matches.
    pub fn set_title(&mut self, id: &str, title: &str) {
        let Some(mut page) = self.workspace.get(id) else {
            return;
        };
        if let RecordPayload::Page { title: current } = &page.payload
            && current == title
        {
            return;
        }
        page.payload = RecordPayload::Page {
            title: title.to_string(),
        };
        page.version = self.workspace.next_clock();
        self.workspace.write_local(&page);
    }

    /// A handle to edit the page with `id`, or `None` when it is unknown.
    pub fn open_document(&self, id: &str) -> Option<PageHandle> {
        self.workspace
            .page_exists(id)
            .then(|| PageHandle::new(self.workspace.clone(), id.to_string()))
    }

    #[cfg(test)]
    pub fn page_exists(&self, id: &str) -> bool {
        self.workspace.page_exists(id)
    }
}

impl SyncSource for Library {
    /// Whether a provisional page is being withheld from sync.
    fn has_provisional(&self) -> bool {
        self.provisional.is_some()
    }

    /// Admit the provisional page into sync (the server has no pages of its own).
    /// The handshake replies with the page on its own; marking the records dirty
    /// as well means a failed or dropped handshake send is retried on the next
    /// flush.
    fn confirm_provisional(&mut self) {
        if let Some(id) = self.provisional.take() {
            self.workspace.mark_page_dirty(&id);
        }
    }

    /// Discard the provisional page (the server already had pages). It was never
    /// synced, so its records are removed outright rather than tombstoned.
    fn drop_provisional(&mut self) {
        let Some(id) = self.provisional.take() else {
            return;
        };
        self.workspace.purge_page(&id);
        if self.active.as_deref() == Some(id.as_str()) {
            self.active = self.entries().first().map(|meta| meta.id.clone());
        }
    }

    /// Per-page digests for the reconnect handshake. A provisional page is
    /// withheld so it isn't pushed to the server.
    fn digests(&self) -> BTreeMap<String, String> {
        let mut digests = self.workspace.digests();
        if let Some(id) = &self.provisional {
            digests.remove(id);
        }
        digests
    }

    /// Every record of a page, tombstones included.
    fn records_for_page(&self, page: &str) -> Vec<Record> {
        self.workspace.records_for_page(page)
    }

    /// The locally-changed records, grouped by page and clearing the dirty set.
    fn take_outbound(&self) -> BTreeMap<String, Vec<Record>> {
        self.workspace.take_outbound()
    }

    /// Merge remote records, returning the pages that changed.
    fn merge_records(&self, records: Vec<Record>) -> Vec<String> {
        let mut pages: Vec<String> = self
            .workspace
            .merge(records)
            .into_iter()
            .map(|record| record.page)
            .collect();
        pages.sort();
        pages.dedup();
        pages
    }
}

/// The next page position, after every existing page.
fn next_page_position(workspace: &Workspace) -> String {
    let max = workspace
        .page_records()
        .into_iter()
        .filter_map(|record| FractionalIndex::from_string(&record.position).ok())
        .max();
    match max {
        Some(index) => FractionalIndex::new_after(&index).to_string(),
        None => FractionalIndex::default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirming_a_provisional_page_marks_it_outbound() {
        let mut library = Library::open(None).unwrap();
        let id = library.create_provisional();
        assert!(library.has_provisional());
        assert!(
            library.take_outbound().is_empty(),
            "a provisional page is withheld from sync until it is confirmed"
        );

        library.confirm_provisional();
        assert!(!library.has_provisional());
        let outbound = library.take_outbound();
        assert_eq!(
            outbound.get(&id).map(Vec::len),
            Some(2),
            "the page and its seed block are queued for sending"
        );
    }
}
