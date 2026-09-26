//! Undo/redo history over record transitions.
//!
//! [`History`] owns the per-page undo and redo stacks. It records a transition
//! and applies it by delegating the actual reads, writes and clock to a
//! [`HistoryTarget`] (the [`crate::workspace::Workspace`]), so the store's merge
//! and persistence logic do not share a module with the coalescing rules.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::model::{Clock, Record};

/// Merge consecutive edits to the same block within this window into one undo
/// step, so typing a word undoes as a word rather than a character.
const COALESCE_WINDOW: Duration = Duration::from_millis(750);

/// One record's transition, captured for undo/redo. `before` is the state to
/// write when undoing (`None` means the record did not exist), and `after`
/// carries the guard version plus the payload redo restores.
#[derive(Clone, Debug)]
pub struct Change {
    pub before: Option<Record>,
    pub after: Record,
}

/// The store operations the history needs. Implemented by the workspace.
pub trait HistoryTarget {
    fn get(&self, id: &str) -> Option<Record>;
    fn write_local(&self, record: &Record);
    fn next_clock(&self) -> Clock;
    fn client_id(&self) -> u64;
}

struct Entry {
    changes: Vec<Change>,
    coalesce: Option<String>,
    at: Instant,
}

/// The per-page undo and redo stacks.
#[derive(Default)]
pub struct History {
    undo: RefCell<HashMap<String, Vec<Entry>>>,
    redo: RefCell<HashMap<String, Vec<Entry>>>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a locally-applied edit. Consecutive edits sharing `coalesce`
    /// within [`COALESCE_WINDOW`] fold into one undo step.
    pub fn push(&self, page: &str, changes: Vec<Change>, coalesce: Option<String>) {
        if changes.is_empty() {
            return;
        }
        self.redo.borrow_mut().remove(page);
        let mut stacks = self.undo.borrow_mut();
        let stack = stacks.entry(page.to_string()).or_default();
        if let Some(key) = &coalesce
            && let Some(last) = stack.last_mut()
            && last.coalesce.as_deref() == Some(key.as_str())
            && last.at.elapsed() < COALESCE_WINDOW
        {
            for change in changes {
                match last
                    .changes
                    .iter_mut()
                    .find(|existing| existing.after.id == change.after.id)
                {
                    Some(existing) => existing.after = change.after,
                    None => last.changes.push(change),
                }
            }
            last.at = Instant::now();
            return;
        }
        stack.push(Entry {
            changes,
            coalesce,
            at: Instant::now(),
        });
    }

    pub fn undo<H: HistoryTarget>(&self, target: &H, page: &str) -> bool {
        self.apply_top(target, page, true)
    }

    pub fn redo<H: HistoryTarget>(&self, target: &H, page: &str) -> bool {
        self.apply_top(target, page, false)
    }

    /// Drop a page's history entirely (used when a page is purged).
    pub fn forget(&self, page: &str) {
        self.undo.borrow_mut().remove(page);
        self.redo.borrow_mut().remove(page);
    }

    /// Pop and apply the top entry, discarding entries a peer has since
    /// overwritten. Such an entry can never be applied — its records now belong
    /// to another client — so skipping it lets undo reach the next edit the user
    /// still owns instead of silently eating the step.
    fn apply_top<H: HistoryTarget>(&self, target: &H, page: &str, from_undo: bool) -> bool {
        loop {
            let entry = {
                let mut stack = if from_undo {
                    self.undo.borrow_mut()
                } else {
                    self.redo.borrow_mut()
                };
                match stack.get_mut(page).and_then(Vec::pop) {
                    Some(entry) => entry,
                    None => return false,
                }
            };
            if self.apply_entry(target, page, entry, from_undo) {
                return true;
            }
        }
    }

    /// Apply an entry's changes, pushing the inverse onto the opposite stack.
    /// `from_undo` selects which stack receives the inverse.
    fn apply_entry<H: HistoryTarget>(
        &self,
        target: &H,
        page: &str,
        entry: Entry,
        from_undo: bool,
    ) -> bool {
        let mut inverse = Vec::new();
        for change in entry.changes {
            if let Some(made) = apply_change(target, &change) {
                inverse.push(made);
            }
        }
        if inverse.is_empty() {
            return false;
        }
        let mut stack = if from_undo {
            self.redo.borrow_mut()
        } else {
            self.undo.borrow_mut()
        };
        stack.entry(page.to_string()).or_default().push(Entry {
            changes: inverse,
            coalesce: None,
            at: Instant::now(),
        });
        true
    }
}

/// Apply one transition, returning the inverse to push onto the opposite stack,
/// or `None` when a peer has overwritten the record since our edit.
fn apply_change<H: HistoryTarget>(target: &H, change: &Change) -> Option<Change> {
    if let Some(current) = target.get(&change.after.id)
        && current.version.client != target.client_id()
    {
        // A peer has changed this record since our edit; leave it alone. Our own
        // later edits keep the same client id and are still ours to undo, so
        // they don't block.
        return None;
    }
    let mut next = match &change.before {
        Some(before) => before.clone(),
        None => change.after.tombstone(),
    };
    next.version = target.next_clock();
    target.write_local(&next);
    Some(Change {
        before: Some(change.after.clone()),
        after: next,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BlockKind, RecordPayload, TextRun};
    use std::cell::Cell;

    #[derive(Default)]
    struct Fake {
        records: RefCell<HashMap<String, Record>>,
        counter: Cell<u64>,
    }

    impl HistoryTarget for Fake {
        fn get(&self, id: &str) -> Option<Record> {
            self.records.borrow().get(id).cloned()
        }

        fn write_local(&self, record: &Record) {
            self.records
                .borrow_mut()
                .insert(record.id.clone(), record.clone());
        }

        fn next_clock(&self) -> Clock {
            let c = self.counter.get() + 1;
            self.counter.set(c);
            Clock::new(c, 1)
        }

        fn client_id(&self) -> u64 {
            1
        }
    }

    fn block(id: &str, text: &str, version: Clock) -> Record {
        Record {
            id: id.to_string(),
            page: "p".to_string(),
            position: "80".to_string(),
            version,
            deleted: false,
            payload: RecordPayload::Block {
                kind: BlockKind::Paragraph,
                runs: vec![TextRun {
                    text: text.to_string(),
                    bold: false,
                    italic: false,
                }],
            },
        }
    }

    fn text(record: &Record) -> String {
        record
            .as_block()
            .map(|(_, runs)| runs.iter().map(|run| run.text.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn undo_and_redo_round_trip_a_transition() {
        let target = Fake::default();
        let history = History::new();
        let before = block("b", "one", Clock::new(1, 1));
        target.write_local(&before);
        let after = block("b", "two", Clock::new(2, 1));
        target.write_local(&after);

        history.push(
            "p",
            vec![Change {
                before: Some(before),
                after,
            }],
            None,
        );

        assert!(history.undo(&target, "p"));
        assert_eq!(text(&target.get("b").unwrap()), "one");
        assert!(history.redo(&target, "p"));
        assert_eq!(text(&target.get("b").unwrap()), "two");
    }
}
