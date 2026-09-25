use super::*;

/// A cached, laid-out block. Rebuilt only when its runs, kind, or the available
/// width changes — so events and paint can both query geometry cheaply.
pub(super) struct BlockLayout {
    pub(super) layout: Layout<BrushIndex>,
    pub(super) runs: Vec<TextRun>,
    pub(super) text: String,
    pub(super) kind: BlockKind,
    pub(super) top: f64,
    pub(super) height: f64,
    /// The wrap width this block was shaped with (page width minus indent).
    pub(super) wrap: f32,
}

impl BlockLayout {
    /// A Parley cursor at `offset`, clamped to a UTF-8 char boundary.
    pub(super) fn cursor_at(&self, offset: usize) -> Cursor {
        Cursor::from_byte_index(
            &self.layout,
            clamp_char_boundary(&self.text, offset),
            Affinity::Downstream,
        )
    }
}

pub(super) fn build_block_layout(
    fcx: &mut FontContext,
    lcx: &mut LayoutContext<BrushIndex>,
    runs: Vec<TextRun>,
    kind: BlockKind,
    width: f32,
) -> BlockLayout {
    let text = runs.iter().map(|run| run.text.as_str()).collect::<String>();
    let wrap = (width - indent(kind) as f32).max(1.0);
    let layout = build_layout(fcx, lcx, &runs, kind, wrap);
    let (line_height, _) = block_metrics(kind);
    let height = (layout.height() as f64).max(kind.font_size() as f64 * line_height as f64);
    BlockLayout {
        layout,
        runs,
        text,
        kind,
        top: 0.0,
        height,
        wrap,
    }
}

/// A Parley ranged builder preloaded with Essor's base font stack and size.
pub(super) fn base_builder<'a>(
    fcx: &'a mut FontContext,
    lcx: &'a mut LayoutContext<BrushIndex>,
    text: &'a str,
    size: f32,
) -> RangedBuilder<'a, BrushIndex> {
    let mut builder = lcx.ranged_builder(fcx, text, 1.0, true);
    builder.push_default(StyleProperty::FontStack(FontStack::Single(
        FontFamily::Generic(GenericFamily::SansSerif),
    )));
    builder.push_default(StyleProperty::FontSize(size));
    builder
}

pub(super) fn build_layout(
    fcx: &mut FontContext,
    lcx: &mut LayoutContext<BrushIndex>,
    runs: &[TextRun],
    kind: BlockKind,
    width: f32,
) -> Layout<BrushIndex> {
    let text = runs.iter().map(|run| run.text.as_str()).collect::<String>();
    let mut builder = base_builder(fcx, lcx, &text, kind.font_size());
    let (line_height, _) = block_metrics(kind);
    builder.push_default(StyleProperty::LineHeight(LineHeight::FontSizeRelative(
        line_height,
    )));
    if matches!(kind, BlockKind::Heading1 | BlockKind::Heading2) {
        builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD));
    }

    let mut offset = 0;
    for run in runs {
        let start = offset;
        let end = offset + run.text.len();
        if run.bold {
            builder.push(StyleProperty::FontWeight(FontWeight::BOLD), start..end);
        }
        if run.italic {
            builder.push(StyleProperty::FontStyle(FontStyle::Italic), start..end);
        }
        offset = end;
    }

    let mut layout = builder.build(&text);
    layout.break_all_lines(Some(width));
    layout.align(Some(width), TextAlign::Start, TextAlignOptions::default());
    layout
}

pub(super) fn line_at_y(layout: &Layout<BrushIndex>, y: f32) -> usize {
    let mut last = 0;
    for (index, line) in layout.lines().enumerate() {
        last = index;
        if y < line.metrics().max_coord {
            return index;
        }
    }
    last
}

pub(super) fn line_center(layout: &Layout<BrushIndex>, line: usize) -> f32 {
    layout
        .get(line)
        .map(|line| {
            let metrics = line.metrics();
            // Vertical center of the em box, which tracks the glyphs rather than
            // the (variable) line box height.
            metrics.baseline - (metrics.ascent - metrics.descent) * 0.5
        })
        .unwrap_or(0.0)
}

pub(super) fn clamp_position(pos: &mut Position, layouts: &[BlockLayout]) {
    let Some(last) = layouts.len().checked_sub(1) else {
        *pos = Position::new(0, 0);
        return;
    };
    if pos.block > last {
        pos.block = last;
        pos.offset = 0;
    }
    if let Some(layout) = layouts.get(pos.block) {
        pos.offset = clamp_char_boundary(&layout.text, pos.offset);
    }
}

impl Editor {
    /// Left edge of a block's text (page margin plus any bullet indent).
    pub(super) fn block_text_x(&self, block: &BlockLayout) -> f64 {
        self.page_left + indent(block.kind)
    }

    // --- layout cache -----------------------------------------------------

    pub(super) fn ensure_layouts(
        &mut self,
        fcx: &mut FontContext,
        lcx: &mut LayoutContext<BrushIndex>,
        width: f32,
    ) {
        let snapshot = self.doc.snapshot();

        let added_or_removed = self.layouts.len() != snapshot.len();
        let width_changed = self.layout_width != width;
        let stale = self.layouts_dirty
            || width_changed
            || added_or_removed
            || self
                .layouts
                .iter()
                .zip(&snapshot)
                .any(|(cached, data)| cached.runs != data.runs || cached.kind != data.kind);

        self.blocks = snapshot.iter().map(|data| data.block.clone()).collect();
        if !stale {
            return;
        }

        if width_changed || added_or_removed {
            self.layouts = snapshot
                .into_iter()
                .map(|data| build_block_layout(fcx, lcx, data.runs, data.kind, width))
                .collect();
        } else {
            // Rebuild only the blocks whose runs or kind actually changed; reuse
            // the (expensive) Parley layouts of untouched blocks.
            for (index, data) in snapshot.into_iter().enumerate() {
                let cached = &self.layouts[index];
                if cached.runs != data.runs || cached.kind != data.kind {
                    self.layouts[index] = build_block_layout(fcx, lcx, data.runs, data.kind, width);
                }
            }
        }
        self.layout_width = width;
        self.layouts_dirty = false;

        let mut y = PAGE_TOP;
        for (index, layout) in self.layouts.iter_mut().enumerate() {
            let (_, space_above) = block_metrics(layout.kind);
            if index > 0 {
                y += space_above;
            }
            layout.top = y;
            y += layout.height;
        }
        self.content_height = y + PAGE_BOTTOM;

        clamp_position(&mut self.selection.anchor, &self.layouts);
        clamp_position(&mut self.selection.focus, &self.layouts);
    }

    pub(super) fn refresh_layouts(&mut self, ctx: &mut EventCtx<'_>) {
        if self.layout_width <= 0.0 {
            return;
        }
        let width = self.layout_width;
        let (fcx, lcx) = ctx.text_contexts();
        self.ensure_layouts(fcx, lcx, width);
    }
}
