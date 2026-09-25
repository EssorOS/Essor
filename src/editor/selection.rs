use super::*;

/// A point in the document: a block index plus a byte offset within that block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Position {
    pub(super) block: usize,
    pub(super) offset: usize,
}

impl Position {
    pub(super) fn new(block: usize, offset: usize) -> Self {
        Self { block, offset }
    }
}

/// Granularity of an in-progress selection drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Granularity {
    Word,
    Line,
}

/// The span (word or line) selected by the initial double/triple click. Dragging
/// extends from this whole span rather than from a single caret.
#[derive(Clone, Copy)]
pub(super) struct DragSpan {
    block: usize,
    start: usize,
    end: usize,
    granularity: Granularity,
}

/// Cursor and selection state for the editor.
#[derive(Default)]
pub(super) struct SelectionState {
    pub(super) anchor: Position,
    pub(super) focus: Position,
    pub(super) preferred_x: Option<f64>,
    pub(super) dragging: bool,
    pub(super) drag_started: bool,
    pub(super) down_point: Option<Point>,
    pub(super) drag_span: Option<DragSpan>,
}

impl Editor {
    // --- selection model --------------------------------------------------

    pub(super) fn collapsed(&self) -> bool {
        self.selection.anchor == self.selection.focus
    }

    pub(super) fn selection(&self) -> (Position, Position) {
        let (a, f) = (self.selection.anchor, self.selection.focus);
        if a.block < f.block || (a.block == f.block && a.offset <= f.offset) {
            (a, f)
        } else {
            (f, a)
        }
    }

    pub(super) fn set_focus(&mut self, pos: Position, extend: bool) {
        if extend {
            self.selection.focus = pos;
        } else {
            self.selection.anchor = pos;
            self.selection.focus = pos;
        }
    }

    pub(super) fn set_caret(&mut self, pos: Position) {
        self.selection.anchor = pos;
        self.selection.focus = pos;
    }

    /// The selection's byte range within `index`, clamped to char boundaries, or
    /// `None` when `index` lies outside the selection. The range may be empty
    /// (e.g. a block fully inside a multi-block selection with no text).
    pub(super) fn selection_in_block(&self, index: usize) -> Option<(usize, usize)> {
        if self.collapsed() {
            return None;
        }
        let (start, end) = self.selection();
        if index < start.block || index > end.block {
            return None;
        }
        let layout = self.layouts.get(index)?;
        let from = if index == start.block {
            clamp_char_boundary(&layout.text, start.offset)
        } else {
            0
        };
        let to = if index == end.block {
            clamp_char_boundary(&layout.text, end.offset)
        } else {
            layout.text.len()
        };
        Some((from, to))
    }

    // --- hit testing ------------------------------------------------------

    pub(super) fn hit_test(&self, point: Point) -> Position {
        let index = self.block_at_y(point.y);
        let Some(block) = self.layouts.get(index) else {
            return Position::new(0, 0);
        };
        let cursor = Cursor::from_point(
            &block.layout,
            (point.x - self.block_text_x(block)) as f32,
            (point.y - block.top) as f32,
        );
        Position::new(index, cursor.index())
    }

    /// Select a word (double click) or the whole block (triple click) at `point`.
    pub(super) fn select_span(&mut self, point: Point, count: u8) {
        let index = self.block_at_y(point.y);
        let Some(block) = self.layouts.get(index) else {
            return;
        };
        let x = (point.x - self.block_text_x(block)) as f32;
        let y = (point.y - block.top) as f32;
        let (granularity, selection) = if count == 2 {
            (
                Granularity::Word,
                Selection::word_from_point(&block.layout, x, y),
            )
        } else {
            (
                Granularity::Line,
                Selection::hard_line_from_point(&block.layout, x, y),
            )
        };
        let a = selection.anchor().index();
        let f = selection.focus().index();
        let (start, end) = (a.min(f), a.max(f));
        self.selection.anchor = Position::new(index, start);
        self.selection.focus = Position::new(index, end);
        self.selection.drag_span = Some(DragSpan {
            block: index,
            start,
            end,
            granularity,
        });
    }

    /// Extend the current selection by whole words/lines from the double/triple
    /// click span, or character-by-character for a plain drag.
    pub(super) fn extend_drag(&mut self, point: Point) {
        let Some(span) = self.selection.drag_span else {
            self.selection.focus = self.hit_test(point);
            return;
        };

        let target = self.block_at_y(point.y);
        if target != span.block {
            // Dragged into another block: keep the span and extend to the edge.
            if target < span.block {
                self.selection.anchor = Position::new(span.block, span.end);
                self.selection.focus = Position::new(target, 0);
            } else {
                self.selection.anchor = Position::new(span.block, span.start);
                let len = self.layouts.get(target).map_or(0, |block| block.text.len());
                self.selection.focus = Position::new(target, len);
            }
            return;
        }

        let Some(block) = self.layouts.get(span.block) else {
            return;
        };
        let x = (point.x - self.block_text_x(block)) as f32;
        let y = (point.y - block.top) as f32;
        let selection = match span.granularity {
            Granularity::Word => Selection::word_from_point(&block.layout, x, y),
            Granularity::Line => Selection::hard_line_from_point(&block.layout, x, y),
        };
        let a = selection.anchor().index();
        let f = selection.focus().index();
        let (group_start, group_end) = (a.min(f), a.max(f));
        let offset = self.hit_test(point).offset;

        if offset < span.start {
            self.selection.anchor = Position::new(span.block, span.end);
            self.selection.focus = Position::new(span.block, group_start);
        } else if offset > span.end {
            self.selection.anchor = Position::new(span.block, span.start);
            self.selection.focus = Position::new(span.block, group_end);
        } else {
            self.selection.anchor = Position::new(span.block, span.start);
            self.selection.focus = Position::new(span.block, span.end);
        }
    }

    pub(super) fn block_at_y(&self, y: f64) -> usize {
        self.layouts
            .iter()
            .position(|block| y < block.top + block.height)
            .unwrap_or(self.layouts.len().saturating_sub(1))
    }

    // --- pointer handling -------------------------------------------------

    pub(super) fn pointer_down(
        &mut self,
        ctx: &mut EventCtx<'_>,
        point: Point,
        count: u8,
        extend: bool,
    ) {
        self.hover = Some(self.block_at_y(point.y));
        if self.handle_overlay_click(point) {
            self.flush_edits(ctx);
            ctx.request_focus();
            ctx.set_handled();
            ctx.request_render();
            return;
        }
        let pos = self.hit_test(point);
        if count >= 2 {
            self.select_span(point, count);
        } else {
            self.selection.drag_span = None;
            self.set_focus(pos, extend);
        }
        self.selection.preferred_x = None;
        self.selection.dragging = true;
        self.selection.drag_started = false;
        self.selection.down_point = Some(point);
        self.activity();
        ctx.capture_pointer();
        ctx.request_focus();
        self.scroll_caret_into_view(ctx);
        ctx.set_handled();
        ctx.request_render();
    }

    pub(super) fn pointer_move(
        &mut self,
        ctx: &mut EventCtx<'_>,
        point: Point,
        primary_down: bool,
    ) {
        let hover = self.block_at_y(point.y);
        if self.hover != Some(hover) {
            self.hover = Some(hover);
            ctx.request_render();
        }
        if !self.selection.dragging || !primary_down {
            return;
        }
        if !self.selection.drag_started {
            self.selection.drag_started = self
                .selection
                .down_point
                .is_none_or(|down| dist2(point, down) >= DRAG_START_DIST_SQ);
        }
        if self.selection.drag_started {
            self.extend_drag(point);
            self.activity();
            ctx.set_handled();
            ctx.request_render();
        }
    }

    pub(super) fn pointer_leave(&mut self, ctx: &mut EventCtx<'_>) {
        if self.hover.is_some() {
            self.hover = None;
            ctx.request_render();
        }
    }

    pub(super) fn pointer_up(&mut self, ctx: &mut EventCtx<'_>) {
        self.selection.dragging = false;
        self.selection.drag_started = false;
        self.selection.down_point = None;
        self.selection.drag_span = None;
        ctx.release_pointer();
    }
}
