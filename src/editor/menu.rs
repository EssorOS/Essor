use super::*;

/// An entry in the block/slash menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuItem {
    Block(BlockKind),
    Delete,
}

impl MenuItem {
    pub(super) const BLOCKS: [Self; 4] = [
        Self::Block(BlockKind::Paragraph),
        Self::Block(BlockKind::Heading1),
        Self::Block(BlockKind::Heading2),
        Self::Block(BlockKind::Bullet),
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Block(BlockKind::Paragraph) => "Text",
            Self::Block(BlockKind::Heading1) => "Heading 1",
            Self::Block(BlockKind::Heading2) => "Heading 2",
            Self::Block(BlockKind::Bullet) => "Bullet list",
            Self::Delete => "Delete",
        }
    }

    pub(super) fn matches(self, query: &str) -> bool {
        query.is_empty() || self.label().to_ascii_lowercase().contains(query)
    }

    pub(super) fn kind(self) -> Option<BlockKind> {
        match self {
            Self::Block(kind) => Some(kind),
            Self::Delete => None,
        }
    }
}

/// A transient popup: either the slash menu (filters as you type) or a block's
/// options menu (opened from the gutter).
pub(super) struct Menu {
    pub(super) block: usize,
    pub(super) anchor: Point,
    pub(super) items: Vec<MenuItem>,
    pub(super) selected: usize,
    pub(super) slash: bool,
    pub(super) query: String,
}

impl Menu {
    pub(super) fn filtered(&self) -> Vec<MenuItem> {
        if self.slash {
            self.items
                .iter()
                .copied()
                .filter(|item| item.matches(&self.query))
                .collect()
        } else {
            self.items.clone()
        }
    }

    pub(super) fn rect(&self) -> Rect {
        let rows = self.filtered().len().max(1);
        Rect::from_origin_size(
            (self.anchor.x, self.anchor.y + MENU_PAD),
            (MENU_WIDTH, MENU_PAD * 2.0 + MENU_ROW * rows as f64),
        )
    }

    /// The visible row under `point`, if the point is inside the popup.
    pub(super) fn row_at(&self, point: Point) -> Option<usize> {
        let rect = self.rect();
        if !rect.contains(point) {
            return None;
        }
        let count = self.filtered().len();
        if count == 0 {
            return None;
        }
        let row = ((point.y - rect.y0 - MENU_PAD) / MENU_ROW) as usize;
        Some(row.min(count - 1))
    }
}

impl Editor {
    // --- gutter & menus ---------------------------------------------------

    pub(super) fn line_center_for(&self, index: usize) -> f64 {
        self.layouts.get(index).map_or(0.0, |block| {
            block.top + line_center(&block.layout, 0) as f64
        })
    }

    pub(super) fn plus_rect(&self, index: usize) -> Rect {
        Rect::from_center_size(
            (
                (self.page_left - GUTTER_PLUS_OFFSET).max(GUTTER_MIN_CENTER),
                self.line_center_for(index),
            ),
            (GUTTER_ICON, GUTTER_ICON),
        )
    }

    pub(super) fn options_rect(&self, index: usize) -> Rect {
        Rect::from_center_size(
            (
                (self.page_left - GUTTER_OPTIONS_OFFSET).max(GUTTER_MIN_CENTER),
                self.line_center_for(index),
            ),
            (GUTTER_ICON, GUTTER_ICON),
        )
    }

    pub(super) fn open_slash_menu(&mut self) {
        let anchor = self
            .focus_rect()
            .map_or(Point::new(self.page_left, 0.0), |rect| {
                Point::new(rect.x0, rect.y1)
            });
        self.menu = Some(Menu {
            block: self.selection.focus.block,
            anchor,
            items: MenuItem::BLOCKS.to_vec(),
            selected: 0,
            slash: true,
            query: String::new(),
        });
    }

    pub(super) fn open_block_menu(&mut self, index: usize) {
        let Some(block) = self.layouts.get(index) else {
            return;
        };
        let mut items = MenuItem::BLOCKS.to_vec();
        items.push(MenuItem::Delete);
        self.menu = Some(Menu {
            block: index,
            anchor: Point::new(self.page_left, block.top),
            items,
            selected: 0,
            slash: false,
            query: String::new(),
        });
    }

    pub(super) fn insert_block_below(&mut self, index: usize) {
        let at = (index + 1).min(self.doc.len());
        let _ = self.doc.insert_block(at);
        self.set_caret(Position::new(at, 0));
        self.layouts_dirty = true;
    }

    /// Refresh the slash query from the block text; close if the `/` is gone.
    pub(super) fn update_slash(&mut self) {
        let Some(index) = self
            .menu
            .as_ref()
            .filter(|menu| menu.slash)
            .map(|menu| menu.block)
        else {
            return;
        };
        let Some(block) = self.block(index) else {
            self.menu = None;
            return;
        };
        let text = self.doc.text(&block);
        if !text.starts_with('/') {
            self.menu = None;
            return;
        }
        let query = text[1..].to_lowercase();
        if let Some(menu) = &mut self.menu {
            menu.query = query;
            let count = menu.filtered().len();
            menu.selected = menu.selected.min(count.saturating_sub(1));
        }
    }

    pub(super) fn menu_move(&mut self, delta: isize) {
        if let Some(menu) = &mut self.menu {
            let count = menu.filtered().len();
            if count == 0 {
                return;
            }
            menu.selected = (menu.selected as isize + delta).clamp(0, count as isize - 1) as usize;
        }
    }

    /// Point the open menu's highlight at the row under `point`. Returns `true`
    /// if the selection changed.
    pub(super) fn menu_hover(&mut self, point: Point) -> bool {
        let Some(menu) = &mut self.menu else {
            return false;
        };
        let Some(row) = menu.row_at(point) else {
            return false;
        };
        if menu.selected == row {
            return false;
        }
        menu.selected = row;
        true
    }

    pub(super) fn menu_confirm(&mut self) {
        if let Some(item) = self
            .menu
            .as_ref()
            .and_then(|menu| menu.filtered().get(menu.selected).copied())
        {
            self.apply_menu(item);
        }
    }

    /// Handle a key that the open menu consumes. Returns `true` if consumed.
    pub(super) fn menu_key(&mut self, key: &KeyboardEvent) -> bool {
        let slash = self.menu.as_ref().is_some_and(|menu| menu.slash);
        match key.key {
            Key::Named(NamedKey::ArrowUp) => {
                self.menu_move(-1);
                true
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.menu_move(1);
                true
            }
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Tab) => {
                self.menu_confirm();
                true
            }
            Key::Named(NamedKey::Escape) => {
                self.close_menu(slash);
                true
            }
            _ => false,
        }
    }

    pub(super) fn close_menu(&mut self, strip_slash: bool) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        if strip_slash && menu.slash {
            self.strip_slash_query(&menu);
        }
    }

    pub(super) fn strip_slash_query(&mut self, menu: &Menu) {
        if let Some(block) = self.block(menu.block) {
            let text = self.doc.text(&block);
            if text.starts_with('/') {
                self.doc.set_text(&block, "");
                self.set_caret(Position::new(menu.block, 0));
            }
            self.layouts_dirty = true;
        }
    }

    pub(super) fn apply_menu(&mut self, item: MenuItem) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        match item {
            MenuItem::Delete => {
                if self.doc.len() > 1 {
                    self.doc.remove_block(menu.block);
                    let target = menu.block.saturating_sub(1);
                    self.set_caret(Position::new(target, 0));
                }
            }
            _ => {
                if menu.slash {
                    self.strip_slash_query(&menu);
                }
                if let (Some(block), Some(kind)) = (self.block(menu.block), item.kind()) {
                    self.doc.set_kind(&block, kind);
                }
            }
        }
        self.layouts_dirty = true;
    }

    /// Handle a pointer press against the menu / gutter. Returns `true` if consumed.
    pub(super) fn handle_overlay_click(&mut self, point: Point) -> bool {
        if self.menu.is_some() {
            let mut clicked = None;
            let mut outside = false;
            if let Some(menu) = &self.menu {
                let rect = menu.rect();
                if rect.contains(point) {
                    let rows = menu.filtered();
                    let row = ((point.y - rect.y0 - MENU_PAD) / MENU_ROW) as usize;
                    clicked = rows.get(row).copied();
                } else {
                    outside = true;
                }
            }
            if let Some(item) = clicked {
                self.apply_menu(item);
                return true;
            }
            if outside {
                self.close_menu(true);
                return true;
            }
            return true;
        }

        if let Some(index) = self.hover {
            if self.plus_rect(index).contains(point) {
                self.insert_block_below(index);
                return true;
            }
            if self.options_rect(index).contains(point) {
                self.open_block_menu(index);
                return true;
            }
        }
        false
    }

    /// If a menu is open, consume navigation/confirm/dismiss keys. Returns `true`
    /// when the event was consumed.
    pub(super) fn handle_menu_key(&mut self, ctx: &mut EventCtx<'_>, key: &KeyboardEvent) -> bool {
        if self.menu.is_none() {
            return false;
        }
        if self.menu_key(key) {
            self.flush_edits(ctx);
            ctx.set_handled();
            ctx.request_render();
            return true;
        }
        // A non-slash menu is dismissed by any other key.
        if self.menu.as_ref().is_some_and(|menu| !menu.slash) {
            self.menu = None;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_menu_filters_by_query() {
        let mut menu = Menu {
            block: 0,
            anchor: Point::ZERO,
            items: MenuItem::BLOCKS.to_vec(),
            selected: 0,
            slash: true,
            query: "head".into(),
        };
        let filtered = menu.filtered();
        assert_eq!(
            filtered,
            vec![
                MenuItem::Block(BlockKind::Heading1),
                MenuItem::Block(BlockKind::Heading2)
            ]
        );

        menu.query.clear();
        assert_eq!(menu.filtered().len(), MenuItem::BLOCKS.len());
    }

    #[test]
    fn row_at_maps_pointer_to_item() {
        let menu = Menu {
            block: 0,
            anchor: Point::new(0.0, 0.0),
            items: MenuItem::BLOCKS.to_vec(),
            selected: 0,
            slash: false,
            query: String::new(),
        };
        let rect = menu.rect();

        let row_y = |row: usize| rect.y0 + MENU_PAD + MENU_ROW * (row as f64 + 0.5);
        let inside_x = rect.x0 + 10.0;

        assert_eq!(menu.row_at(Point::new(inside_x, row_y(0))), Some(0));
        assert_eq!(menu.row_at(Point::new(inside_x, row_y(2))), Some(2));

        // Points outside the popup map to nothing.
        assert_eq!(menu.row_at(Point::new(inside_x, rect.y0 - 5.0)), None);
        assert_eq!(menu.row_at(Point::new(rect.x1 + 5.0, row_y(0))), None);

        // Bottom padding clamps to the last row.
        assert_eq!(
            menu.row_at(Point::new(inside_x, rect.y1 - 1.0)),
            Some(MenuItem::BLOCKS.len() - 1)
        );
    }
}
