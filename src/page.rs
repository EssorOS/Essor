//! The store-backed per-page view: [`PageHandle`] and its [`Doc`] implementation.
//!
//! This is where editing semantics (run splicing, block insert/remove, undo)
//! meet the record store. The [`Doc`] trait itself is defined in
//! [`crate::doc`]; the SQLite/LWW store is in [`crate::workspace`].

use std::rc::Rc;

use fractional_index::FractionalIndex;

use crate::doc::{BlockId, BlockKind, BlockSnapshot, Doc, Mark, TextRun};
use crate::history::Change;
use crate::model::{Clock, Record, RecordPayload};
use crate::runs::{clamp_boundary, coalesce, common_edges, marks_at, runs_text, split_runs};
use crate::workspace::Workspace;

/// A page plus its blocks, addressed by id. Cheap to clone; shares the store.
#[derive(Clone)]
pub struct PageHandle {
    workspace: Rc<Workspace>,
    page: String,
}

impl PageHandle {
    pub fn new(workspace: Rc<Workspace>, page: String) -> Self {
        Self { workspace, page }
    }

    pub fn workspace(&self) -> &Rc<Workspace> {
        &self.workspace
    }

    pub fn page_id(&self) -> &str {
        &self.page
    }

    /// Apply a group of record transitions from one editing operation.
    pub fn commit(&self, changes: Vec<Change>, coalesce: Option<String>) {
        for change in &changes {
            self.workspace.write_local(&change.after);
        }
        self.workspace.push_undo(&self.page, changes, coalesce);
    }

    pub fn block_records(&self) -> Vec<Record> {
        self.workspace.block_records(&self.page)
    }

    pub fn get(&self, id: &str) -> Option<Record> {
        self.workspace.get(id)
    }

    pub fn next_clock(&self) -> Clock {
        self.workspace.next_clock()
    }

    /// Build the next record for a block, preserving identity and position but
    /// taking a fresh version.
    pub fn block_record(
        &self,
        id: &str,
        position: &str,
        kind: BlockKind,
        runs: Vec<TextRun>,
    ) -> Record {
        Record {
            id: id.to_string(),
            page: self.page.clone(),
            position: position.to_string(),
            version: self.workspace.next_clock(),
            deleted: false,
            payload: RecordPayload::Block { kind, runs },
        }
    }
}

impl Doc for PageHandle {
    fn snapshot(&self) -> Vec<BlockSnapshot> {
        self.block_records()
            .into_iter()
            .filter_map(|record| {
                let id = BlockId::parse(&record.id)?;
                let (kind, runs) = record.as_block()?;
                Some(BlockSnapshot {
                    id,
                    runs: runs.to_vec(),
                    kind: *kind,
                })
            })
            .collect()
    }

    fn text(&self, block: BlockId) -> String {
        self.get(&block.to_simple())
            .and_then(|record| record.as_block().map(|(_, runs)| runs_text(runs)))
            .unwrap_or_default()
    }

    fn runs(&self, block: BlockId) -> Vec<TextRun> {
        self.get(&block.to_simple())
            .and_then(|record| record.as_block().map(|(_, runs)| runs.to_vec()))
            .unwrap_or_default()
    }

    fn set_text(&self, block: BlockId, value: &str) {
        let Some(record) = self.get(&block.to_simple()) else {
            return;
        };
        let Some((kind, old_runs)) = record.as_block().map(|(kind, runs)| (*kind, runs.to_vec()))
        else {
            return;
        };
        let old = runs_text(&old_runs);
        if old == value {
            return;
        }

        let (prefix, suffix) = common_edges(&old, value);
        let insert = &value[prefix..value.len() - suffix];

        let (bold, italic) = marks_at(&old_runs, prefix);
        let (mut runs, _) = split_runs(&old_runs, prefix);
        let (_, right) = split_runs(&old_runs, old.len() - suffix);
        if !insert.is_empty() {
            runs.push(TextRun {
                text: insert.to_string(),
                bold,
                italic,
            });
        }
        runs.extend(right);
        let runs = coalesce(runs);

        let after = self.block_record(&record.id, &record.position, kind, runs);
        self.commit(
            vec![Change {
                before: Some(record.clone()),
                after,
            }],
            Some(record.id.clone()),
        );
    }

    fn mark(&self, block: BlockId, start: usize, end: usize, mark: Mark, on: bool) {
        let Some(record) = self.get(&block.to_simple()) else {
            return;
        };
        let Some((kind, runs)) = record.as_block().map(|(kind, runs)| (*kind, runs.to_vec()))
        else {
            return;
        };
        let text = runs_text(&runs);
        let start = clamp_boundary(&text, start);
        let end = clamp_boundary(&text, end).max(start);
        if start >= end {
            return;
        }
        let (left, rest) = split_runs(&runs, start);
        let (mid, right) = split_runs(&rest, end - start);
        let mid: Vec<TextRun> = mid
            .into_iter()
            .map(|mut run| {
                match mark {
                    Mark::Bold => run.bold = on,
                    Mark::Italic => run.italic = on,
                }
                run
            })
            .collect();
        let mut updated = left;
        updated.extend(mid);
        updated.extend(right);
        let updated = coalesce(updated);
        if updated == runs {
            return;
        }
        let after = self.block_record(&record.id, &record.position, kind, updated);
        self.commit(
            vec![Change {
                before: Some(record),
                after,
            }],
            None,
        );
    }

    fn kind(&self, block: BlockId) -> BlockKind {
        self.get(&block.to_simple())
            .and_then(|record| record.as_block().map(|(kind, _)| *kind))
            .unwrap_or_default()
    }

    fn set_kind(&self, block: BlockId, kind: BlockKind) {
        let Some(record) = self.get(&block.to_simple()) else {
            return;
        };
        let Some((current, runs)) = record.as_block().map(|(kind, runs)| (*kind, runs.to_vec()))
        else {
            return;
        };
        if current == kind {
            return;
        }
        let after = self.block_record(&record.id, &record.position, kind, runs);
        self.commit(
            vec![Change {
                before: Some(record),
                after,
            }],
            None,
        );
    }

    fn insert_block(&self, index: usize) -> BlockId {
        let records = self.block_records();
        let index = index.min(records.len());
        let lower = index
            .checked_sub(1)
            .and_then(|i| records.get(i))
            .map(|record| record.position.as_str());
        let upper = records.get(index).map(|record| record.position.as_str());
        let position = next_position(lower, upper);
        let id = BlockId::new();
        let after = self.block_record(&id.to_simple(), &position, BlockKind::Paragraph, Vec::new());
        self.commit(
            vec![Change {
                before: None,
                after,
            }],
            None,
        );
        id
    }

    fn remove_block(&self, block: BlockId) {
        let Some(record) = self.get(&block.to_simple()) else {
            return;
        };
        let mut after = record.tombstone();
        after.version = self.next_clock();
        self.commit(
            vec![Change {
                before: Some(record),
                after,
            }],
            None,
        );
    }

    fn len(&self) -> usize {
        self.block_records().len()
    }

    fn undo(&mut self) -> bool {
        self.workspace().undo(self.page_id())
    }

    fn redo(&mut self) -> bool {
        self.workspace().redo(self.page_id())
    }
}

/// A fractional index strictly between `lower` and `upper` (either may be
/// absent), falling back to an append/default when the bounds are unusable.
fn next_position(lower: Option<&str>, upper: Option<&str>) -> String {
    let lower = lower.and_then(|s| FractionalIndex::from_string(s).ok());
    let upper = upper.and_then(|s| FractionalIndex::from_string(s).ok());
    FractionalIndex::new(lower.as_ref(), upper.as_ref())
        .or_else(|| lower.as_ref().map(FractionalIndex::new_after))
        .map(|index| index.to_string())
        .unwrap_or_else(|| FractionalIndex::default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Workspace;

    fn handle() -> PageHandle {
        let ws = Workspace::open(None).unwrap();
        PageHandle::new(ws, "p".to_string())
    }

    #[test]
    fn inserting_inside_bold_keeps_the_bold_prefix() {
        let h = handle();
        let id = h.insert_block(0);
        h.set_text(id, "hello world");
        h.mark(id, 0, 5, Mark::Bold, true);
        h.set_text(id, "hello brave world");
        let runs = h.runs(id);
        assert_eq!(runs_text(&runs), "hello brave world");
        assert!(runs[0].bold, "the bold prefix survived: {runs:?}");
        assert_eq!(runs[0].text, "hello");
        assert!(!runs[1].bold);
    }

    #[test]
    fn deleting_inside_a_marked_range_keeps_the_mark() {
        let h = handle();
        let id = h.insert_block(0);
        h.set_text(id, "hello world");
        h.mark(id, 0, 5, Mark::Bold, true);
        h.set_text(id, "hell");
        let runs = h.runs(id);
        assert_eq!(runs_text(&runs), "hell");
        assert!(runs[0].bold, "the mark survived the deletion: {runs:?}");
    }

    #[test]
    fn inserted_blocks_keep_order_between_neighbors() {
        let h = handle();
        let a = h.insert_block(0);
        let c = h.insert_block(1);
        let b = h.insert_block(1);
        let ids: Vec<_> = h.snapshot().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![a, b, c]);
    }
}
