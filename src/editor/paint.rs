use super::*;

pub(super) fn draw_plus(scene: &mut Scene, rect: Rect, color: Color) {
    let center = rect.center();
    let arm = rect.width() * 0.5;
    let thickness = 1.5;
    let brush = Brush::Solid(color);
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        &brush,
        None,
        &Rect::from_center_size(center, (arm * 2.0, thickness)),
    );
    scene.fill(
        Fill::NonZero,
        Affine::IDENTITY,
        &brush,
        None,
        &Rect::from_center_size(center, (thickness, arm * 2.0)),
    );
}

pub(super) fn draw_dots(scene: &mut Scene, rect: Rect, color: Color) {
    let center = rect.center();
    for column in DOT_COLUMNS {
        for row in DOT_ROWS {
            scene.fill(
                Fill::NonZero,
                Affine::IDENTITY,
                color,
                None,
                &Circle::new((center.x + column, center.y + row), DOT_RADIUS),
            );
        }
    }
}

pub(super) fn text_label(
    lcx: &mut LayoutContext<BrushIndex>,
    fcx: &mut FontContext,
    text: &str,
    size: f32,
) -> Layout<BrushIndex> {
    let builder = base_builder(fcx, lcx, text, size);
    let mut layout = builder.build(text);
    layout.break_all_lines(None);
    layout.align(None, TextAlign::Start, TextAlignOptions::default());
    layout
}

impl Editor {
    // --- painting ---------------------------------------------------------

    /// Fills only the visible slice of the page. The editor reports its full
    /// document height (so the `Portal` can scroll it), and a full-height
    /// background would make every repaint GPU-bound for long documents.
    pub(super) fn paint_background(&self, scene: &mut Scene, size: Size, visible: Visible) {
        let (y0, y1) = visible.clamped_to(size.height);
        if y1 <= y0 {
            return;
        }
        scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            BACKGROUND,
            None,
            &Rect::from_origin_size((0.0, y0), (size.width, y1 - y0)),
        );
    }

    pub(super) fn paint_blocks(
        &self,
        scene: &mut Scene,
        font_cx: &mut FontContext,
        layout_cx: &mut LayoutContext<BrushIndex>,
        focused: bool,
        visible: Visible,
    ) {
        let caret_block = self.selection.focus.block;
        let caret_offset = self.selection.focus.offset;

        for (index, block) in self.layouts.iter().enumerate() {
            if !visible.contains(block.top, block.height) {
                continue;
            }
            let text_x = self.block_text_x(block);
            let block_width = block.wrap;
            let block_top = block.top;

            if let Some((from, to)) = self
                .selection_in_block(index)
                .filter(|(from, to)| from < to)
            {
                let anchor = block.cursor_at(from);
                let focus = block.cursor_at(to);
                for (bounds, _) in Selection::new(anchor, focus).geometry(&block.layout) {
                    let rect = Rect::from_origin_size(
                        (text_x + bounds.x0, block_top + bounds.y0),
                        ((bounds.x1 - bounds.x0).max(1.0), bounds.y1 - bounds.y0),
                    );
                    scene.fill(Fill::NonZero, Affine::IDENTITY, SELECTION, None, &rect);
                }
            }

            if block.kind == BlockKind::Bullet {
                let center = (
                    text_x - BULLET_INDENT * 0.5,
                    block_top + line_center(&block.layout, 0) as f64,
                );
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    BULLET,
                    None,
                    &Circle::new(center, BULLET_RADIUS),
                );
            }

            masonry::core::render_text(
                scene,
                Affine::translate((text_x, block_top)),
                &block.layout,
                &[Brush::Solid(TEXT)],
                true,
            );

            if focused
                && block.id == caret_block
                && block.text.is_empty()
                && block.kind != BlockKind::Bullet
            {
                let hint = if index == 0 {
                    "Untitled"
                } else {
                    "Type '/' for commands"
                };
                let run = TextRun {
                    text: hint.to_string(),
                    bold: false,
                    italic: false,
                };
                let layout = build_layout(font_cx, layout_cx, &[run], block.kind, block_width);
                masonry::core::render_text(
                    scene,
                    Affine::translate((text_x, block_top)),
                    &layout,
                    &[Brush::Solid(PLACEHOLDER)],
                    true,
                );
            }

            if focused && self.caret_visible && block.id == caret_block {
                let cursor = block.cursor_at(caret_offset);
                let bounds = cursor.geometry(&block.layout, block_width);
                let rect = Rect::from_origin_size(
                    (text_x + bounds.x0, block_top + bounds.y0),
                    (CARET_WIDTH, (bounds.y1 - bounds.y0).max(1.0)),
                );
                scene.fill(Fill::NonZero, Affine::IDENTITY, CARET, None, &rect);
            }
        }
    }

    pub(super) fn paint_preedit(
        &self,
        scene: &mut Scene,
        font_cx: &mut FontContext,
        layout_cx: &mut LayoutContext<BrushIndex>,
        focused: bool,
    ) {
        if !focused || self.preedit.is_empty() {
            return;
        }
        let Some(origin) = self.focus_rect() else {
            return;
        };
        let font_size = self
            .layout_of(self.selection.focus.block)
            .map_or(DEFAULT_FONT_SIZE, |block| font_size(block.kind));
        let builder = base_builder(font_cx, layout_cx, &self.preedit, font_size);
        let mut layout = builder.build(&self.preedit);
        layout.break_all_lines(None);
        layout.align(None, TextAlign::Start, TextAlignOptions::default());

        let transform = Affine::translate((origin.x0, origin.y0));
        masonry::core::render_text(scene, transform, &layout, &[Brush::Solid(TEXT)], true);

        let underline = Rect::from_origin_size(
            (origin.x0, origin.y1),
            (layout.width().max(1.0) as f64, CARET_WIDTH),
        );
        scene.fill(Fill::NonZero, Affine::IDENTITY, CARET, None, &underline);
    }

    pub(super) fn paint_gutter(&self, scene: &mut Scene) {
        if let Some(index) = self.hover
            && index < self.layouts.len()
        {
            draw_plus(scene, self.plus_rect(index), GUTTER_ICON_COLOR);
            draw_dots(scene, self.options_rect(index), GUTTER_ICON_COLOR);
        }
    }

    pub(super) fn paint_menu(
        &self,
        scene: &mut Scene,
        font_cx: &mut FontContext,
        layout_cx: &mut LayoutContext<BrushIndex>,
    ) {
        let Some(menu) = &self.menu else {
            return;
        };
        let rect = menu.rect();
        let panel = RoundedRect::from_rect(rect, MENU_RADIUS);
        let shadow = RoundedRect::from_rect(
            Rect::from_origin_size((rect.x0, rect.y0 + 2.0), rect.size()).inflate(1.0, 1.0),
            MENU_RADIUS,
        );
        scene.fill(Fill::NonZero, Affine::IDENTITY, MENU_SHADOW, None, &shadow);
        scene.fill(Fill::NonZero, Affine::IDENTITY, MENU_BG, None, &panel);
        scene.stroke(
            &Stroke::new(1.0),
            Affine::IDENTITY,
            MENU_BORDER,
            None,
            &panel,
        );

        let rows = menu.filtered();
        if rows.is_empty() {
            let layout = text_label(layout_cx, font_cx, "No results", MENU_FONT_SIZE);
            let y = rect.y0 + (MENU_ROW - layout.height() as f64) * 0.5 + MENU_PAD;
            masonry::core::render_text(
                scene,
                Affine::translate((rect.x0 + 12.0, y)),
                &layout,
                &[Brush::Solid(PLACEHOLDER)],
                true,
            );
        }
        for (row_index, item) in rows.iter().enumerate() {
            let row = Rect::from_origin_size(
                (
                    rect.x0 + MENU_PAD,
                    rect.y0 + MENU_PAD + row_index as f64 * MENU_ROW,
                ),
                (rect.width() - MENU_PAD * 2.0, MENU_ROW),
            );
            if row_index == menu.selected {
                let highlight = RoundedRect::from_rect(row, 4.0);
                scene.fill(
                    Fill::NonZero,
                    Affine::IDENTITY,
                    MENU_SELECTED,
                    None,
                    &highlight,
                );
            }
            let layout = text_label(layout_cx, font_cx, item.label(), MENU_FONT_SIZE);
            let y = row.y0 + (MENU_ROW - layout.height() as f64) * 0.5;
            masonry::core::render_text(
                scene,
                Affine::translate((row.x0 + 10.0, y)),
                &layout,
                &[Brush::Solid(TEXT)],
                true,
            );
        }
    }
}
