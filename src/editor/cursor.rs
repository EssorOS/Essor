use super::*;

impl Editor {
    pub(super) fn focus_rect(&self) -> Option<Rect> {
        let block = self.layout_of(self.selection.focus.block)?;
        let cursor = block.cursor_at(self.selection.focus.offset);
        let bounds = cursor.geometry(&block.layout, block.wrap);
        Some(Rect::from_origin_size(
            (self.block_text_x(block) + bounds.x0, block.top + bounds.y0),
            (CARET_WIDTH, (bounds.y1 - bounds.y0).max(1.0)),
        ))
    }

    pub(super) fn scroll_caret_into_view(&self, ctx: &mut EventCtx<'_>) {
        // When the document already fits the viewport there is nothing to scroll,
        // and asking the enclosing `Portal` to pan it makes the Portal divide by
        // zero to recompute its scrollbar thumb progress. That stores `NaN`, which
        // the scrollbar then renders with garbage geometry until some later scroll
        // recomputes it — so the scrollbar goes missing and the frame blows up. Skip
        // the request entirely while the content fits.
        if self.viewport_height <= 0.0 || self.content_height <= self.viewport_height {
            return;
        }
        if let Some(rect) = self.focus_rect() {
            ctx.request_scroll_to(rect);
        }
    }

    // --- cursor motion ----------------------------------------------------

    pub(super) fn move_horizontal(&mut self, dir: isize, extend: bool, word: bool) {
        if !extend && !self.collapsed() {
            let (start, end) = self.selection();
            self.set_caret(if dir < 0 { start } else { end });
            self.selection.preferred_x = None;
            return;
        }

        let pos = self.selection.focus;
        let Some(index) = self.index_of(pos.block) else {
            return;
        };
        let Some(block) = self.layouts.get(index) else {
            return;
        };
        let cursor = block.cursor_at(pos.offset);
        let moved = if word {
            if dir < 0 {
                cursor.previous_visual_word(&block.layout)
            } else {
                cursor.next_visual_word(&block.layout)
            }
        } else if dir < 0 {
            cursor.previous_visual(&block.layout)
        } else {
            cursor.next_visual(&block.layout)
        };

        let mut target = Position::new(pos.block, moved.index());
        if target.offset == pos.offset {
            if dir < 0 && index > 0 {
                let previous = &self.layouts[index - 1];
                target = Position::new(previous.id, previous.text.len());
            } else if dir > 0 && index + 1 < self.layouts.len() {
                let next = &self.layouts[index + 1];
                target = Position::new(next.id, 0);
            }
        }
        self.set_focus(target, extend);
        self.selection.preferred_x = None;
    }

    pub(super) fn move_vertical(&mut self, dir: isize, extend: bool) {
        if !extend && !self.collapsed() {
            self.set_caret(self.selection.focus);
        }
        let pos = self.selection.focus;
        let Some((x, y)) = self.cursor_point(pos) else {
            return;
        };
        let x = self.selection.preferred_x.unwrap_or(x);
        self.selection.preferred_x = Some(x);
        if let Some(target) = self.vertical_target(pos, dir, x, y) {
            self.set_focus(target, extend);
        }
    }

    pub(super) fn move_line_edge(&mut self, to_end: bool, extend: bool) {
        let pos = self.selection.focus;
        let Some((_, y)) = self.cursor_point(pos) else {
            return;
        };
        let Some(block) = self.layout_of(pos.block) else {
            return;
        };
        let line = line_at_y(&block.layout, y as f32);
        let Some(line) = block.layout.get(line) else {
            return;
        };
        let range = line.text_range();
        let offset = if to_end { range.end } else { range.start };
        self.set_focus(Position::new(pos.block, offset), extend);
        self.selection.preferred_x = None;
    }

    pub(super) fn cursor_point(&self, pos: Position) -> Option<(f64, f64)> {
        let block = self.layout_of(pos.block)?;
        let cursor = block.cursor_at(pos.offset);
        let geometry = cursor.geometry(&block.layout, block.wrap);
        Some((geometry.x0, geometry.y0))
    }

    pub(super) fn vertical_target(
        &self,
        pos: Position,
        dir: isize,
        x: f64,
        y: f64,
    ) -> Option<Position> {
        let index = self.index_of(pos.block)?;
        let block = self.layouts.get(index)?;
        let line = line_at_y(&block.layout, y as f32) as isize + dir;
        if line < 0 {
            let previous = index.checked_sub(1)?;
            let target = &self.layouts[previous];
            let last = target.layout.len().saturating_sub(1);
            let cursor =
                Cursor::from_point(&target.layout, x as f32, line_center(&target.layout, last));
            Some(Position::new(target.id, cursor.index()))
        } else if line as usize >= block.layout.len() {
            let next = index + 1;
            let target = self.layouts.get(next)?;
            let cursor =
                Cursor::from_point(&target.layout, x as f32, line_center(&target.layout, 0));
            Some(Position::new(target.id, cursor.index()))
        } else {
            let cursor = Cursor::from_point(
                &block.layout,
                x as f32,
                line_center(&block.layout, line as usize),
            );
            Some(Position::new(pos.block, cursor.index()))
        }
    }
}
