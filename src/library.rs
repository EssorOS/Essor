use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::doc::{Doc, YrsDocument};

/// Longest title shown in the sidebar (in characters), before truncation.
const TITLE_MAX: usize = 40;

/// Metadata for one document in the library.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentMeta {
    pub id: String,
    pub title: String,
}

impl DocumentMeta {
    fn untitled(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: "Untitled".to_string(),
        }
    }
}

/// On-disk index: the ordered documents and which one is active.
#[derive(Default, Serialize, Deserialize)]
struct IndexFile {
    active: usize,
    documents: Vec<DocumentMeta>,
}

/// A collection of documents persisted under a data directory:
///
/// ```text
/// <dir>/documents.json        ordered ids, titles, and the active index
/// <dir>/documents/<id>.ydoc   one Yrs update file per document
/// ```
///
/// A `None` directory yields a single in-memory document, which keeps tests and
/// embedding trivial.
pub struct Library {
    dir: Option<PathBuf>,
    docs: Vec<DocumentMeta>,
    active: usize,
    index_dirty: Cell<bool>,
}

impl Library {
    /// Open (or initialise) the library in `dir`. `None` creates an in-memory
    /// library with a single blank document.
    pub fn open(dir: Option<PathBuf>) -> Self {
        let mut library = Self {
            dir,
            docs: Vec::new(),
            active: 0,
            index_dirty: Cell::new(false),
        };
        library.load();
        library
    }

    fn load(&mut self) {
        let Some(dir) = self.dir.clone() else {
            self.docs = vec![DocumentMeta::untitled("memory")];
            self.active = 0;
            return;
        };

        let docs_dir = dir.join("documents");
        let _ = std::fs::create_dir_all(&docs_dir);

        if let Ok(bytes) = std::fs::read(dir.join("documents.json"))
            && let Ok(index) = serde_json::from_slice::<IndexFile>(&bytes)
            && !index.documents.is_empty()
        {
            self.docs = index.documents;
            self.active = index.active.min(self.docs.len() - 1);
            self.refresh_titles();
            return;
        }

        // Migrate the single-document file from before the sidebar existed.
        let legacy = dir.join("essor.ydoc");
        if legacy.exists() {
            let meta = self.import_legacy(&legacy);
            self.docs = vec![meta];
            self.active = 0;
            self.write_index();
            return;
        }

        let meta = self.new_document();
        self.docs = vec![meta];
        self.active = 0;
        self.write_index();
    }

    /// Derive titles from file contents so the stored cache can never drift.
    fn refresh_titles(&mut self) {
        let Some(docs_dir) = self.docs_dir() else {
            return;
        };
        for meta in &mut self.docs {
            let path = docs_dir.join(format!("{}.ydoc", meta.id));
            if !path.exists() {
                continue;
            }
            let doc = YrsDocument::open(Some(path));
            meta.title = derive_title(&doc);
        }
    }

    fn import_legacy(&self, legacy: &Path) -> DocumentMeta {
        let id = self.unique_id();
        let doc = YrsDocument::open(Some(legacy.to_path_buf()));
        let title = derive_title(&doc);
        if let Some(dest) = self.doc_path(&id)
            && std::fs::rename(legacy, &dest).is_err()
        {
            let _ = std::fs::copy(legacy, &dest);
            let _ = std::fs::remove_file(legacy);
        }
        DocumentMeta { id, title }
    }

    /// Create a fresh document on disk and return its metadata.
    fn new_document(&self) -> DocumentMeta {
        let id = self.unique_id();
        if let Some(path) = self.doc_path(&id) {
            let doc = YrsDocument::open(Some(path));
            let _ = doc.save_now();
        }
        DocumentMeta::untitled(id)
    }

    /// A timestamp-based id that does not collide with an existing file.
    fn unique_id(&self) -> String {
        let mut counter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        loop {
            let id = format!("{counter:x}");
            let exists = self.doc_path(&id).is_some_and(|path| path.exists());
            if !exists {
                return id;
            }
            counter = counter.wrapping_add(1);
        }
    }

    fn docs_dir(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|dir| dir.join("documents"))
    }

    fn doc_path(&self, id: &str) -> Option<PathBuf> {
        self.docs_dir().map(|dir| dir.join(format!("{id}.ydoc")))
    }

    /// The ordered documents shown in the sidebar.
    pub fn entries(&self) -> &[DocumentMeta] {
        &self.docs
    }

    /// Index of the document currently open in the editor.
    pub fn active(&self) -> usize {
        self.active
    }

    /// Load the document at `index` as a fresh in-memory handle.
    pub fn open_document(&self, index: usize) -> YrsDocument {
        let path = self
            .docs
            .get(index)
            .and_then(|meta| self.doc_path(&meta.id));
        YrsDocument::open(path)
    }

    /// Make `index` the active document.
    pub fn set_active(&mut self, index: usize) {
        if index < self.docs.len() && index != self.active {
            self.active = index;
            self.index_dirty.set(true);
        }
    }

    /// Create a new document, make it active, and return its index.
    pub fn create(&mut self) -> usize {
        let meta = self.new_document();
        self.docs.push(meta);
        self.active = self.docs.len() - 1;
        self.index_dirty.set(true);
        self.active
    }

    /// Delete the document at `index`. Refused when it is the last one.
    pub fn delete(&mut self, index: usize) -> bool {
        if self.docs.len() <= 1 || index >= self.docs.len() {
            return false;
        }
        let meta = self.docs.remove(index);
        if let Some(path) = self.doc_path(&meta.id) {
            let _ = std::fs::remove_file(path);
        }
        if self.active > index {
            self.active -= 1;
        } else if self.active == index {
            self.active = index.min(self.docs.len() - 1);
        }
        self.index_dirty.set(true);
        true
    }

    /// Cache a new title for `index`. Written to disk lazily by [`save_index`].
    pub fn set_title(&mut self, index: usize, title: String) {
        if let Some(meta) = self.docs.get_mut(index)
            && meta.title != title
        {
            meta.title = title;
            self.index_dirty.set(true);
        }
    }

    /// Write pending changes (if any) to disk.
    pub fn save_index(&self) {
        if self.index_dirty.get() {
            self.write_index();
        }
    }

    fn write_index(&self) {
        let Some(dir) = &self.dir else {
            self.index_dirty.set(false);
            return;
        };
        let index = IndexFile {
            active: self.active,
            documents: self.docs.clone(),
        };
        if let Ok(json) = serde_json::to_vec_pretty(&index) {
            let _ = crate::fs::atomic_write(&dir.join("documents.json"), &json);
        }
        self.index_dirty.set(false);
    }
}

/// A sidebar title derived from a document's first non-empty line.
pub fn derive_title(doc: &dyn Doc) -> String {
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
    use crate::doc::Doc;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("essor-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn starts_with_one_document() {
        let library = Library::open(None);
        assert_eq!(library.entries().len(), 1);
        assert_eq!(library.active(), 0);
        assert_eq!(library.entries()[0].title, "Untitled");
    }

    #[test]
    fn create_and_delete_roundtrip() {
        let dir = temp_dir("crud");
        let mut library = Library::open(Some(dir.clone()));

        let second = library.create();
        assert_eq!(second, 1);
        assert_eq!(library.entries().len(), 2);
        assert_eq!(library.active(), 1);

        assert!(library.delete(1));
        assert_eq!(library.entries().len(), 1);
        assert_eq!(library.active(), 0);
        library.save_index();

        // Reopening sees the same state.
        let reopened = Library::open(Some(dir.clone()));
        assert_eq!(reopened.entries().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_to_delete_last_document() {
        let dir = temp_dir("last");
        let mut library = Library::open(Some(dir.clone()));
        assert!(!library.delete(0));
        assert_eq!(library.entries().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn persists_active_selection() {
        let dir = temp_dir("active");
        let mut library = Library::open(Some(dir.clone()));
        library.create();
        library.create();
        library.set_active(0);
        library.save_index();

        let reopened = Library::open(Some(dir.clone()));
        assert_eq!(reopened.active(), 0);
        assert_eq!(reopened.entries().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_reflect_content() {
        let dir = temp_dir("titles");
        let mut library = Library::open(Some(dir.clone()));
        let doc = library.open_document(0);
        let block = doc.snapshot().remove(0).block;
        doc.set_text(&block, "Hello world\nsecond line");
        doc.persist().unwrap();

        library.set_title(0, derive_title(&doc));
        library.save_index();

        let reopened = Library::open(Some(dir.clone()));
        assert_eq!(reopened.entries()[0].title, "Hello world");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_legacy_document() {
        let dir = temp_dir("legacy");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let doc = YrsDocument::open(Some(dir.join("essor.ydoc")));
            let block = doc.snapshot().remove(0).block;
            doc.set_text(&block, "Legacy notes");
            doc.save_now().unwrap();
        }

        let library = Library::open(Some(dir.clone()));
        assert_eq!(library.entries().len(), 1);
        assert_eq!(library.entries()[0].title, "Legacy notes");
        assert!(!dir.join("essor.ydoc").exists());
        assert!(dir.join("documents.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncates_long_titles() {
        let title = truncate_title(&"a".repeat(TITLE_MAX + 5));
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), TITLE_MAX + 1);
    }
}
