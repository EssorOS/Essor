use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;

use yrs::undo::UndoManager;
use yrs::updates::decoder::Decode;
use yrs::{
    Any, Array, ArrayRef, Doc as YrsDoc, GetString, Map, MapPrelim, MapRef, Out, ReadTxn,
    StateVector, Text, TextPrelim, TextRef, Transact, TransactionMut, Update, types::Attrs,
};

const ROOT: &str = "blocks";
const KEY_KIND: &str = "kind";
const KEY_TEXT: &str = "text";
const UNDO_ORIGIN: &str = "local";

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

/// Opaque, cheaply cloneable handle to a block in a [`Doc`].
#[derive(Clone)]
pub struct Block(MapRef);

/// A block together with a snapshot of its content, read in a single
/// transaction by [`Doc::snapshot`].
pub struct BlockSnapshot {
    pub block: Block,
    pub runs: Vec<TextRun>,
    pub kind: BlockKind,
}

/// The boundary the editor talks to. `yrs` lives entirely behind this trait, so the
/// UI never depends on the CRDT crate directly. Swap in another backend later without
/// touching the view layer.
pub trait Doc {
    /// Every block with its runs and kind, read in one transaction.
    fn snapshot(&self) -> Vec<BlockSnapshot>;
    fn text(&self, block: &Block) -> String;
    fn runs(&self, block: &Block) -> Vec<TextRun>;
    fn set_text(&self, block: &Block, value: &str);
    fn mark(&self, block: &Block, start: usize, end: usize, mark: Mark, on: bool);
    fn kind(&self, block: &Block) -> BlockKind;
    fn set_kind(&self, block: &Block, kind: BlockKind);
    fn insert_block(&self, index: usize) -> Block;
    fn remove_block(&self, index: usize);
    fn len(&self) -> usize;
    fn undo(&mut self) -> bool;
    fn redo(&mut self) -> bool;
    /// Persist the document to disk if a path is configured and it has unsaved changes.
    fn persist(&self) -> std::io::Result<()>;
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
    pub fn open(path: Option<PathBuf>) -> Self {
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
        if document.is_empty() {
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

    fn touch(&self) {
        self.dirty.set(true);
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
                    let runs = match map.get(&txn, KEY_TEXT) {
                        Some(Out::YText(text)) => runs_of(&text, &txn),
                        _ => Vec::new(),
                    };
                    let kind = kind_of(&map, &txn);
                    Some(BlockSnapshot {
                        block: Block(map),
                        runs,
                        kind,
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn text(&self, block: &Block) -> String {
        let txn = self.doc.transact();
        match block.0.get(&txn, KEY_TEXT) {
            Some(Out::YText(text)) => text.get_string(&txn),
            _ => String::new(),
        }
    }

    fn set_text(&self, block: &Block, value: &str) {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        if let Some(Out::YText(text)) = block.0.get(&txn, KEY_TEXT) {
            splice(&text, &mut txn, value);
        }
        drop(txn);
        self.touch();
    }

    fn runs(&self, block: &Block) -> Vec<TextRun> {
        let txn = self.doc.transact();
        match block.0.get(&txn, KEY_TEXT) {
            Some(Out::YText(text)) => runs_of(&text, &txn),
            _ => Vec::new(),
        }
    }

    fn mark(&self, block: &Block, start: usize, end: usize, mark: Mark, on: bool) {
        if start >= end {
            return;
        }
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        if let Some(Out::YText(text)) = block.0.get(&txn, KEY_TEXT) {
            let value = if on { Any::Bool(true) } else { Any::Undefined };
            let attrs: Attrs = [(Arc::from(mark.key()), value)].into_iter().collect();
            text.format(&mut txn, start as u32, (end - start) as u32, attrs);
        }
        drop(txn);
        self.touch();
    }

    fn kind(&self, block: &Block) -> BlockKind {
        let txn = self.doc.transact();
        kind_of(&block.0, &txn)
    }

    fn set_kind(&self, block: &Block, kind: BlockKind) {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        block.0.insert(&mut txn, KEY_KIND, kind.key());
        drop(txn);
        self.touch();
    }

    fn insert_block(&self, index: usize) -> Block {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        let map = self
            .blocks
            .insert(&mut txn, index as u32, MapPrelim::default());
        map.insert(&mut txn, KEY_KIND, BlockKind::Paragraph.key());
        map.insert(&mut txn, KEY_TEXT, TextPrelim::new(String::new()));
        drop(txn);
        self.touch();
        Block(map)
    }

    fn remove_block(&self, index: usize) {
        let mut txn = self.doc.transact_mut_with(UNDO_ORIGIN);
        self.blocks.remove(&mut txn, index as u32);
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
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.dirty.get() {
            return Ok(());
        }
        let txn = self.doc.transact();
        let update = txn.encode_state_as_update_v1(&StateVector::default());
        drop(txn);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, &update)?;
        std::fs::rename(&tmp, path)?;
        self.dirty.set(false);
        Ok(())
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

    fn first(doc: &YrsDocument) -> Block {
        doc.snapshot().remove(0).block
    }

    #[test]
    fn starts_with_one_empty_block() {
        let doc = YrsDocument::new();
        assert_eq!(doc.len(), 1);
        let block = first(&doc);
        assert_eq!(doc.text(&block), "");
        assert_eq!(doc.kind(&block), BlockKind::Paragraph);
    }

    #[test]
    fn set_text_splices() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(&block, "hello");
        doc.set_text(&block, "hello world");
        doc.set_text(&block, "hello there");
        assert_eq!(doc.text(&block), "hello there");
    }

    #[test]
    fn set_text_covers_insert_replace_and_clear() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(&block, "hello");
        doc.set_text(&block, "hello world");
        assert_eq!(doc.text(&block), "hello world");
        doc.set_text(&block, "hello brave world");
        assert_eq!(doc.text(&block), "hello brave world");
        doc.set_text(&block, "world");
        assert_eq!(doc.text(&block), "world");
        doc.set_text(&block, "world");
        assert_eq!(doc.text(&block), "world");
        doc.set_text(&block, "");
        assert_eq!(doc.text(&block), "");
        doc.set_text(&block, "fresh");
        assert_eq!(doc.text(&block), "fresh");
    }

    #[test]
    fn multibyte_edits_stay_on_char_boundaries() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(&block, "héllo ★");
        assert_eq!(doc.text(&block), "héllo ★");
        doc.set_text(&block, "héllo ★ world");
        assert_eq!(doc.text(&block), "héllo ★ world");
        doc.set_text(&block, "héllo");
        assert_eq!(doc.text(&block), "héllo");
    }

    #[test]
    fn insert_and_remove_blocks() {
        let doc = YrsDocument::new();
        let second = doc.insert_block(1);
        doc.set_text(&second, "second");
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.text(&doc.snapshot()[1].block), "second");

        doc.remove_block(0);
        assert_eq!(doc.len(), 1);
        assert_eq!(doc.text(&doc.snapshot()[0].block), "second");
    }

    #[test]
    fn kind_roundtrips() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_kind(&block, BlockKind::Heading1);
        assert_eq!(doc.kind(&block), BlockKind::Heading1);
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
        doc.set_text(&block, "hello");
        assert!(doc.undo());
        assert_eq!(doc.text(&first(&doc)), "");
        assert!(doc.redo());
        assert_eq!(doc.text(&first(&doc)), "hello");
    }

    #[test]
    fn marks_split_runs_and_can_be_removed() {
        let doc = YrsDocument::new();
        let block = first(&doc);
        doc.set_text(&block, "hello world");

        doc.mark(&block, 0, 5, Mark::Bold, true);
        let runs = doc.runs(&block);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].text, "hello");
        assert!(runs[0].bold);
        assert_eq!(runs[1].text, " world");
        assert!(!runs[1].bold);

        doc.mark(&block, 0, 5, Mark::Bold, false);
        assert!(doc.runs(&block).iter().all(|run| !run.bold));
    }

    #[test]
    fn persists_and_reloads() {
        let path = std::env::temp_dir().join(format!("essor-test-{}.ydoc", std::process::id()));
        let _ = std::fs::remove_file(&path);

        {
            let doc = YrsDocument::open(Some(path.clone()));
            let block = first(&doc);
            doc.set_text(&block, "persisted");
            doc.persist().unwrap();
        }

        let doc = YrsDocument::open(Some(path.clone()));
        assert_eq!(doc.text(&first(&doc)), "persisted");
        let _ = std::fs::remove_file(&path);
    }
}
