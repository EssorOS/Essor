use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use yrs::undo::UndoManager;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayRef, BranchID, ClientID, Doc as YrsDoc, GetString, Map, MapPrelim, MapRef,
    Out, ReadTxn, SharedRef, StateVector, Text, TextPrelim, TextRef, Transact, TransactionMut,
    Update, types::Attrs,
};

const ROOT: &str = "blocks";
const KEY_KIND: &str = "kind";
const KEY_TEXT: &str = "text";
const UNDO_ORIGIN: &str = "local";
/// Origin for updates applied on behalf of a sync peer. It keeps them out of the
/// undo stack and lets the sync client tell remote edits from local ones so they
/// are never echoed back.
pub const REMOTE_ORIGIN: &str = "essor-remote";

/// The kind of a block. Kept intentionally small for now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlockKind {
    #[default]
    Paragraph,
    Heading1,
    Heading2,
    Bullet,
}

impl BlockKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Paragraph => "paragraph",
            Self::Heading1 => "heading1",
            Self::Heading2 => "heading2",
            Self::Bullet => "bullet",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "heading1" => Self::Heading1,
            "heading2" => Self::Heading2,
            "bullet" => Self::Bullet,
            _ => Self::Paragraph,
        }
    }

    pub fn font_size(self) -> f32 {
        match self {
            Self::Paragraph | Self::Bullet => 16.0,
            Self::Heading1 => 28.0,
            Self::Heading2 => 22.0,
        }
    }
}

/// A contiguous run of text sharing the same inline marks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextRun {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
}

/// An inline mark that can be toggled over a range of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Bold,
    Italic,
}

impl Mark {
    fn key(self) -> &'static str {
        match self {
            Self::Bold => "bold",
            Self::Italic => "italic",
        }
    }

    fn is_set(self, attrs: Option<&Attrs>) -> bool {
        matches!(attrs.and_then(|a| a.get(self.key())), Some(Any::Bool(true)))
    }
}

/// Stable identity of a block.
///
/// It is the CRDT's own id for the block's nested map (client id + clock), so
/// every replica derives the same id for the same block and it survives
/// persistence. The editor keys selection and geometry by this instead of by a
/// positional index, which keeps the caret on its block when a peer inserts or
/// removes blocks above it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(yrs::ID);

impl Default for BlockId {
    /// A placeholder used before the first layout pass, when no real block is
    /// known yet. `yrs` client ids are random 53-bit values, so this exact
    /// `(client, clock)` pair is a notional id rather than one the layout path
    /// assigns; the chance of it colliding with a real block is negligible, and
    /// the selection is re-anchored against the real layouts on the first pass.
    fn default() -> Self {
        BlockId(yrs::ID::new(ClientID::new((1u64 << 53) - 1), u32::MAX))
    }
}

/// A block together with a snapshot of its content, read in a single
/// transaction by [`Doc::snapshot`].
pub struct BlockSnapshot {
    pub id: BlockId,
    pub runs: Vec<TextRun>,
    pub kind: BlockKind,
}

/// The boundary the editor talks to. `yrs` lives entirely behind this trait, so the
/// UI never depends on the CRDT crate directly. Swap in another backend later without
/// touching the view layer.
pub trait Doc {
    /// Every block with its runs and kind, read in one transaction.
    fn snapshot(&self) -> Vec<BlockSnapshot>;
    fn text(&self, block: BlockId) -> String;
    fn runs(&self, block: BlockId) -> Vec<TextRun>;
    fn set_text(&self, block: BlockId, value: &str);
    fn mark(&self, block: BlockId, start: usize, end: usize, mark: Mark, on: bool);
    fn kind(&self, block: BlockId) -> BlockKind;
    fn set_kind(&self, block: BlockId, kind: BlockKind);
    fn insert_block(&self, index: usize) -> BlockId;
    fn remove_block(&self, block: BlockId);
    fn len(&self) -> usize;
    fn undo(&mut self) -> bool;
    fn redo(&mut self) -> bool;
    /// Persist the document to disk if a path is configured and it has unsaved changes.
    fn persist(&self) -> std::io::Result<()>;
    /// Apply an update received from a sync peer. The update is tagged as remote
    /// so it is not treated as a local edit. Returns whether it changed the
    /// document, so a peer's echo of a local edit can be ignored. Must be called
    /// on the thread that owns the document.
    fn apply_remote(&self, update: &[u8]) -> bool;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A `yrs`-backed [`Doc`]: a root `Y.Array` of block `Y.Map`s, each holding a
/// `kind` (plain string) and a rich `Y.Text`.
pub struct YrsDocument {
    doc: YrsDoc,
    blocks: ArrayRef,
    undo: UndoManager<()>,
    path: Option<PathBuf>,
    dirty: Cell<bool>,
}

impl YrsDocument {
    /// An in-memory document (no persistence). Used by tests.
    pub fn new() -> Self {
        Self::open(None)
    }

    /// Open a document, loading prior state from `path` if it exists and is valid.
    ///
    /// A brand-new file (path absent) is seeded with one empty block. An existing
    /// file is never seeded, so documents materialised from a peer's list — which
    /// start as empty files and receive their content over sync — don't each
    /// contribute a duplicate seed block.
    pub fn open(path: Option<PathBuf>) -> Self {
        let seed = path.as_ref().map(|path| !path.exists()).unwrap_or(true);
        let doc = YrsDoc::new();
        let blocks = doc.get_or_insert_array(ROOT);

        if let Some(path) = &path
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(update) = Update::decode_v1(&bytes)
        {
            let mut txn = doc.transact_mut();
            let _ = txn.apply_update(update);
        }

        let mut document = Self {
            doc,
            blocks,
            undo: UndoManager::new(),
            path,
            dirty: Cell::new(false),
        };
        // Seed one empty block before the undo manager starts observing, so the
        // initial state isn't an undoable step.
        if seed && document.is_empty() {
            document.insert_block(0);
        }
        document.dirty.set(false);
        {
            let Self {
                doc, blocks, undo, ..
            } = &mut document;
            undo.include_origin(UNDO_ORIGIN);
            undo.expand_scope(doc, blocks);
        }
        document
    }

    /// Write an empty, unseeded update file. Used for documents created by a
    /// peer, whose content arrives later over sync.
    ///
    /// If the write fails the file is simply absent, so a later [`Self::open`]
    /// falls back to seeding one block rather than yielding an unusable page.
    pub fn create_empty(path: &Path) -> std::io::Result<()> {
        let doc = YrsDoc::new();
        let txn = doc.transact();
        let update = txn.encode_state_as_update_v1(&StateVector::default());
        drop(txn);
        crate::fs::atomic_write(path, &update)
    }

    fn touch(&self) {
        self.dirty.set(true);
    }

    /// Encode the current state and atomically replace the backing file.
    fn write_to_disk(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let txn = self.doc.transact();
        let update = txn.encode_state_as_update_v1(&StateVector::default());
        drop(txn);
        crate::fs::atomic_write(path, &update)
    }

    /// Persist immediately, even when there are no unsaved changes. Used to
    /// materialise a freshly created document on disk.
    pub fn save_now(&self) -> std::io::Result<()> {
        self.write_to_disk()?;
        self.dirty.set(false);
        Ok(())
    }

    /// A second handle to the underlying CRDT store. Cheap to clone and safe to
    /// move across threads; both handles observe the same document.
    pub fn handle(&self) -> YrsDoc {
        self.doc.clone()
    }

    /// Resolve a block's nested map from its stable id, or `None` when the block
    /// no longer exists.
    fn block_map<T: ReadTxn>(&self, txn: &T, id: BlockId) -> Option<MapRef> {
        BranchID::Nested(id.0).get_branch(txn).map(MapRef::from)
    }
}

/// The stable id of a block map, or `None` if it is somehow not nested.
fn block_id(map: &MapRef) -> Option<BlockId> {
    match map.hook().id() {
        BranchID::Nested(id) => Some(BlockId(*id)),
        BranchID::Root(_) => None,
    }
}

impl Default for YrsDocument {
    fn default() -> Self {
        Self::new()
    }
}

impl Doc for YrsDocument {
    fn snapshot(&self) -> Vec<BlockSnapshot> {
        let txn = self.doc.transact();
        self.blocks
            .iter(&txn)
            .filter_map(|value| match value {
                Out::YMap(map) => {
                    let id = block_id(&map)?;
                    let runs = match map.get(&txn, KEY_TEXT) {
                        Some(Out::YText(text)) => runs_of(&text, &txn),
                        _ => Vec::new(),
                    };
                    let kind = kind_of(&map, &txn);
                    Some(BlockSnapshot { id, runs, kind })
                }
                _ => None,
            })
            .collect()
    }

    fn text(&self, block: BlockId) -> String {
        let txn = self.doc.transact();
        match self
            .block_map(&txn, block)
            .and_then(|map| map.get(&txn, KEY_TEXT))
        {
            Some(Out::YText(text)) => text.get_string(&txn),
            _ => String::new(),
        }
    }

    fn set_text(&self, block: BlockId, value: &str) {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        if let Some(Out::YText(text)) = self
            .block_map(&txn, block)
            .and_then(|map| map.get(&txn, KEY_TEXT))
        {
            splice(&text, &mut txn, value);
        }
        drop(txn);
        self.touch();
    }

    fn runs(&self, block: BlockId) -> Vec<TextRun> {
        let txn = self.doc.transact();
        match self
            .block_map(&txn, block)
            .and_then(|map| map.get(&txn, KEY_TEXT))
        {
            Some(Out::YText(text)) => runs_of(&text, &txn),
            _ => Vec::new(),
        }
    }

    fn mark(&self, block: BlockId, start: usize, end: usize, mark: Mark, on: bool) {
        if start >= end {
            return;
        }
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        if let Some(Out::YText(text)) = self
            .block_map(&txn, block)
            .and_then(|map| map.get(&txn, KEY_TEXT))
        {
            let value = if on { Any::Bool(true) } else { Any::Undefined };
            let attrs: Attrs = [(Arc::from(mark.key()), value)].into_iter().collect();
            text.format(&mut txn, start as u32, (end - start) as u32, attrs);
        }
        drop(txn);
        self.touch();
    }

    fn kind(&self, block: BlockId) -> BlockKind {
        let txn = self.doc.transact();
        match self.block_map(&txn, block) {
            Some(map) => kind_of(&map, &txn),
            None => BlockKind::default(),
        }
    }

    fn set_kind(&self, block: BlockId, kind: BlockKind) {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        if let Some(map) = self.block_map(&txn, block) {
            map.insert(&mut txn, KEY_KIND, kind.key());
        }
        drop(txn);
        self.touch();
    }

    fn insert_block(&self, index: usize) -> BlockId {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        let map = self
            .blocks
            .insert(&mut txn, index as u32, MapPrelim::default());
        map.insert(&mut txn, KEY_KIND, BlockKind::Paragraph.key());
        map.insert(&mut txn, KEY_TEXT, TextPrelim::new(String::new()));
        drop(txn);
        self.touch();
        block_id(&map).expect("a freshly inserted block always has a nested id")
    }

    fn remove_block(&self, block: BlockId) {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        let index = self
            .blocks
            .iter(&txn)
            .position(|value| matches!(value, Out::YMap(map) if block_id(&map) == Some(block)));
        if let Some(index) = index {
            self.blocks.remove(&mut txn, index as u32);
        }
        drop(txn);
        self.touch();
    }

    fn len(&self) -> usize {
        let txn = self.doc.transact();
        self.blocks.len(&txn) as usize
    }

    fn undo(&mut self) -> bool {
        let changed = self.undo.undo_blocking();
        if changed {
            self.touch();
        }
        changed
    }

    fn redo(&mut self) -> bool {
        let changed = self.undo.redo_blocking();
        if changed {
            self.touch();
        }
        changed
    }

    fn persist(&self) -> std::io::Result<()> {
        if !self.dirty.get() {
            return Ok(());
        }
        self.write_to_disk()?;
        self.dirty.set(false);
        Ok(())
    }

    fn apply_remote(&self, update: &[u8]) -> bool {
        let Ok(update) = Update::decode_v1(update) else {
            return false;
        };
        let mut txn = self.doc.transact_mut_with(REMOTE_ORIGIN);
        let _ = txn.apply_update(update);
        // The same test `yrs` uses to decide a transaction changed anything: a
        // delete set, or an advanced state vector. This catches delete-only
        // updates (which don't advance the state vector) and treats an echo of a
        // local edit — whose blocks are already known — as a no-op.
        let changed = !txn.delete_set().is_empty() || txn.after_state() != txn.before_state();
        drop(txn);
        if changed {
            self.touch();
        }
        changed
    }
}

/// Collect the marked text runs of `text` within an already-open transaction.
fn runs_of<T: ReadTxn>(text: &TextRef, txn: &T) -> Vec<TextRun> {
    let mut runs: Vec<TextRun> = Vec::new();
    for diff in text.diff(txn, |_| ()) {
        let content = diff.insert.to_string(txn);
        if content.is_empty() {
            continue;
        }
        let attrs = diff.attributes.as_deref();
        let run = TextRun {
            text: content,
            bold: Mark::Bold.is_set(attrs),
            italic: Mark::Italic.is_set(attrs),
        };
        match runs.last_mut() {
            Some(last) if last.bold == run.bold && last.italic == run.italic => {
                last.text.push_str(&run.text);
            }
            _ => runs.push(run),
        }
    }
    runs
}

/// Read a block map's kind within an already-open transaction.
fn kind_of<T: ReadTxn>(map: &MapRef, txn: &T) -> BlockKind {
    map.get(txn, KEY_KIND)
        .and_then(|value| value.cast::<String>().ok())
        .map(|key| BlockKind::from_key(&key))
        .unwrap_or_default()
}

/// Push a full `new` value into a `Y.Text` as a single splice, preserving the
/// untouched prefix/suffix. Keeps concurrent-editing intent far better than a
/// wholesale replace. `yrs` uses byte offsets by default, so we stay on byte
/// boundaries.
fn splice(text: &TextRef, txn: &mut TransactionMut, new: &str) {
    let old = text.get_string(txn);
    if old == new {
        return;
    }

    let max = old.len().min(new.len());

    let mut prefix = 0;
    while prefix < max && old.as_bytes()[prefix] == new.as_bytes()[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !new.is_char_boundary(prefix) {
        prefix -= 1;
    }

    let mut suffix = 0;
    let max_suffix = max - prefix;
    while suffix < max_suffix
        && old.as_bytes()[old.len() - 1 - suffix] == new.as_bytes()[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && !(old.is_char_boundary(old.len() - suffix) && new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }

    let remove_len = old.len() - prefix - suffix;
    let insert = &new[prefix..new.len() - suffix];

    if remove_len > 0 {
        text.remove_range(txn, prefix as u32, remove_len as u32);
    }
    if !insert.is_empty() {
        text.insert(txn, prefix as u32, insert);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first(doc: &YrsDocument) -> BlockId {
        doc.snapshot().remove(0).id
    }

    #[test]
    fn starts_with_one_empty_block() {
        let doc = YrsDocument::new();
        assert_eq!(doc.len(), 1);
        let block = first(&doc);
        assert_eq!(doc.text(block), "");
        assert_eq!(doc.kind(block), BlockKind::Paragraph);
    }

    #[test]
    fn nonexistent_file_is_seeded() {
        let path = std::env::temp_dir().join(format!("essor-seed-{}.ydoc", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let doc = YrsDocument::open(Some(path.clone()));
        assert_eq!(doc.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn existing_empty_file_is_not_seeded() {
        let path = std::env::temp_dir().join(format!("essor-empty-{}.ydoc", std::process::id()));
        let _ = std::fs::remove_file(&path);
        YrsDocument::create_empty(&path).unwrap();
        let doc = YrsDocument::open(Some(path.clone()));
        assert_eq!(doc.len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn applying_a_known_remote_update_reports_no_change() {
        let source = YrsDocument::new();
        let source_block = first(&source);
        source.set_text(source_block, "hello");
        let update = source
            .doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());

        // An empty target so the update's block is genuinely new.
        let target = YrsDocument::new();
        target.remove_block(first(&target));

        assert!(
            target.apply_remote(&update),
            "first apply must change the document"
        );
        assert_eq!(target.text(first(&target)), "hello");
        assert!(
            !target.apply_remote(&update),
            "re-applying a known update must be a no-op"
        );
    }

    #[test]
    fn applying_a_remote_deletion_reports_change() {
        let source = YrsDocument::new();
        let second = source.insert_block(1);
        source.set_text(second, "second");
        let update = source
            .doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default());

        let target = YrsDocument::new();
        target.remove_block(first(&target));
        assert!(target.apply_remote(&update));
        assert_eq!(target.len(), 2);

        // Delete a block on the source and send only the diff: a delete-only
        // update, which advances no state vector but must still count.
        source.remove_block(second);
        let target_sv = target.doc.transact().state_vector();
        let deletion = source.doc.transact().encode_state_as_update_v1(&target_sv);
        assert!(target.apply_remote(&deletion));
        assert_eq!(target.len(), 1);
    }

    #[test]
    fn set_text_splices() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(block, "hello");
        doc.set_text(block, "hello world");
        doc.set_text(block, "hello there");
        assert_eq!(doc.text(block), "hello there");
    }

    #[test]
    fn set_text_covers_insert_replace_and_clear() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(block, "hello");
        doc.set_text(block, "hello world");
        assert_eq!(doc.text(block), "hello world");
        doc.set_text(block, "hello brave world");
        assert_eq!(doc.text(block), "hello brave world");
        doc.set_text(block, "world");
        assert_eq!(doc.text(block), "world");
        doc.set_text(block, "world");
        assert_eq!(doc.text(block), "world");
        doc.set_text(block, "");
        assert_eq!(doc.text(block), "");
        doc.set_text(block, "fresh");
        assert_eq!(doc.text(block), "fresh");
    }

    #[test]
    fn multibyte_edits_stay_on_char_boundaries() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(block, "héllo ★");
        assert_eq!(doc.text(block), "héllo ★");
        doc.set_text(block, "héllo ★ world");
        assert_eq!(doc.text(block), "héllo ★ world");
        doc.set_text(block, "héllo");
        assert_eq!(doc.text(block), "héllo");
    }

    #[test]
    fn insert_and_remove_blocks() {
        let doc = YrsDocument::new();
        let second = doc.insert_block(1);
        doc.set_text(second, "second");
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.text(doc.snapshot()[1].id), "second");

        doc.remove_block(doc.snapshot()[0].id);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.text(doc.snapshot()[0].id), "second");
    }

    #[test]
    fn kind_roundtrips() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_kind(block, BlockKind::Heading1);
        assert_eq!(doc.kind(block), BlockKind::Heading1);
    }

    #[test]
    fn initial_state_is_not_undoable() {
        let mut doc = YrsDocument::new();
        assert!(!doc.undo());
    }

    #[test]
    fn undo_and_redo_restore_text() {
        let mut doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(block, "hello");
        assert!(doc.undo());
        assert_eq!(doc.text(first(&doc)), "");
        assert!(doc.redo());
        assert_eq!(doc.text(first(&doc)), "hello");
    }

    #[test]
    fn marks_split_runs_and_can_be_removed() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(block, "hello world");

        doc.mark(block, 0, 5, Mark::Bold, true);
        let runs = doc.runs(block);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "hello");
        assert!(runs[0].bold);
        assert_eq!(runs[1].text, " world");
        assert!(!runs[1].bold);

        doc.mark(block, 0, 5, Mark::Bold, false);
        assert!(doc.runs(block).iter().all(|run| !run.bold));
    }

    #[test]
    fn remove_block_removes_the_named_block() {
        let doc = YrsDocument::new();
        let first = doc.snapshot()[0].id;
        let second = doc.insert_block(1);
        doc.set_text(second, "keep");

        doc.remove_block(first);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.snapshot()[0].id, second);
        assert_eq!(doc.text(second), "keep");
    }

    #[test]
    fn block_ids_are_stable_across_persistence() {
        let path = std::env::temp_dir().join(format!("essor-id-{}.ydoc", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let id = {
            let doc = YrsDocument::open(Some(path.clone()));
            let id = doc.snapshot()[0].id;
            doc.set_text(id, "x");
            doc.persist().unwrap();
            id
        };

        let doc = YrsDocument::open(Some(path.clone()));
        assert_eq!(doc.snapshot()[0].id, id, "id changed across reload");
        assert_eq!(doc.text(id), "x");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn persists_and_reloads() {
        let path = std::env::temp_dir().join(format!("essor-test-{}.ydoc", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let doc = YrsDocument::open(Some(path.clone()));
            let block = first(&doc);
            doc.set_text(block, "persisted");
            doc.persist().unwrap();
        }

        let doc = YrsDocument::open(Some(path.clone()));
        assert_eq!(doc.text(first(&doc)), "persisted");
        let _ = std::fs::remove_file(&path);
    }
}
