//! The per-page editing interface.
//!
//! The editor talks to a [`Doc`], which for the app is a
//! [`crate::page::PageHandle`] over the shared [`crate::workspace::Workspace`].
//! Blocks are records; text and marks live in a run list that
//! [`crate::page::PageHandle`] mutates with a byte-level splice (see
//! [`crate::runs`]), preserving the marks of untouched text and inheriting the
//! mark at an edit boundary.

pub use crate::model::{BlockId, BlockKind, Mark, TextRun};

/// A block together with a snapshot of its content, read in one pass by
/// [`Doc::snapshot`].
pub struct BlockSnapshot {
    pub id: BlockId,
    pub runs: Vec<TextRun>,
    pub kind: BlockKind,
}

/// The boundary the editor talks to. It is a *per-page* view: sync and
/// persistence are handled by the [`crate::workspace::Workspace`], not here.
pub trait Doc {
    /// Every live block with its runs and kind, in order.
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
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
